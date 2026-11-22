//! `cargo clickbench`: ClickBench's 43 queries over its Parquet files,
//! read in place, with Tessera on, Tessera off and pg_duckdb, on a
//! PostgreSQL cluster of the tool's own. See `bench/clickbench/README.md`.
#![forbid(unsafe_code)]

mod check;
mod cluster;
mod measure;
mod queries;
mod report;
mod session;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use console::style;

use cluster::{Cluster, Pg, ServerSettings};
use queries::Selection;
use report::{Meta, Run, RunDir};
use session::{Mode, Session, Settings};

#[derive(Debug, Parser)]
#[command(
    name = "tessera-clickbench",
    about = "ClickBench's queries over its Parquet files, with Tessera on, off and pg_duckdb"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Install the release build of Tessera, start the cluster, make the
    /// tables over the files and leave the server running.
    Setup(ClusterArgs),
    /// Check the answers: every query once in each mode, Tessera on and
    /// pg_duckdb against Tessera off. Times nothing, so it may run on a
    /// busy machine.
    Check(CheckArgs),
    /// Time the queries in one mode, three tries each, as ClickBench
    /// does, under a deadline. Run it on an idle machine, after a check.
    Run(RunArgs),
    /// Print runs again from their directories, side by side when there
    /// are several: each query's hot time and the ratio to the first.
    /// Timed or counted runs that differ only in their queries, as runs
    /// of one query each do, make one column.
    Report {
        /// Run directories, target/bench-runs/clickbench-<id>.
        #[arg(required = true)]
        dirs: Vec<PathBuf>,
    },
    /// Stop the cluster; its data stays.
    Stop(ClusterArgs),
}

#[derive(Debug, Clone, Args)]
struct ClusterArgs {
    /// The files the tables read: Parquet files, or directories whose
    /// .parquet files are read in the order of their names [default:
    /// target/clickbench-data of the repository].
    #[arg(long, value_delimiter = ',', value_name = "PATH")]
    files: Vec<PathBuf>,
    /// Port of the cluster's server.
    #[arg(long, default_value_t = 5439)]
    port: u16,
    /// shared_buffers of the server.
    #[arg(long, default_value = "2GB", value_name = "SIZE")]
    shared_buffers: String,
    /// Keep the Tessera build installed in PG_CONFIG's tree instead of
    /// building and installing the release one.
    #[arg(long)]
    no_install: bool,
    /// Leave the server running at the end even if this command started
    /// it.
    #[arg(long)]
    keep_running: bool,
}

/// The settings the queries run under.
#[derive(Debug, Clone, Args)]
struct QueryArgs {
    /// Queries: numbers and ranges from 1 (1-7 or 1,3,24) or all.
    #[arg(long, default_value = "all", value_parser = Selection::parse)]
    queries: Selection,
    /// max_parallel_workers_per_gather.
    #[arg(long, default_value_t = 0)]
    workers: u32,
    /// work_mem.
    #[arg(long, default_value = "64MB", value_name = "SIZE")]
    work_mem: String,
    /// DuckDB's threads in the mode duckdb [default: --workers + 1, the
    /// processes of a parallel plan of PostgreSQL].
    #[arg(long, value_name = "THREADS", value_parser = clap::value_parser!(u32).range(1..))]
    duckdb_threads: Option<u32>,
}

/// The modes of a check, in the order of [`Mode::ALL`].
#[derive(Debug, Clone, PartialEq)]
struct Modes(Vec<Mode>);

impl Modes {
    fn parse(text: &str) -> Result<Modes> {
        Mode::parse_list(text).map(Modes)
    }
}

#[derive(Debug, Clone, Args)]
struct CheckArgs {
    #[command(flatten)]
    cluster: ClusterArgs,
    #[command(flatten)]
    queries: QueryArgs,
    /// The modes to check, off always among them [default: on, off and,
    /// where the build has it, duckdb].
    #[arg(long, value_parser = Modes::parse)]
    modes: Option<Modes>,
    /// statement_timeout of a query and of its companion, in seconds.
    #[arg(long, default_value_t = 300, value_name = "SECONDS")]
    timeout: u64,
}

#[derive(Debug, Clone, Args)]
struct RunArgs {
    #[command(flatten)]
    cluster: ClusterArgs,
    #[command(flatten)]
    queries: QueryArgs,
    /// The mode to time.
    #[arg(long, value_enum)]
    mode: Mode,
    /// Tries of each query, each on a new connection.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..))]
    tries: u32,
    /// statement_timeout of a try, in seconds.
    #[arg(long, default_value_t = 60, value_name = "SECONDS")]
    timeout: u64,
    /// The run's deadline, in seconds from its start, its build and
    /// tables among it: tries it leaves no time for are reported as
    /// such.
    #[arg(long, default_value_t = 300, value_name = "SECONDS")]
    deadline: u64,
    /// Count the instructions and cycles of each query's execution with
    /// pg_pmu instead of timing it: on one connection, once to warm up,
    /// then a reading a try.
    #[arg(long)]
    counters: bool,
}

/// The repository's root, which holds the queries and the run directories.
fn root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize().unwrap_or(root)
}

fn runs_dir() -> PathBuf {
    root().join("target/bench-runs")
}

/// A cluster brought up for a command, with its tables over the files.
struct Up {
    cluster: Cluster,
    was_running: bool,
    files: Vec<PathBuf>,
    rows: i64,
    duckdb: Option<String>,
}

/// Brings the cluster up; with `deadline`, the build and install, and
/// the making of the tables, stop at it.
/// A server it started is stopped again when what follows fails, unless
/// `keep` says otherwise; with `pmu`, the server preloads pg_pmu.
fn bring_up(args: &ClusterArgs, deadline: Option<Instant>, keep: bool, pmu: bool) -> Result<Up> {
    let pg = Pg::discover()?;
    if pmu && !cluster::pmu_installed(&pg) {
        bail!(
            "counting needs pg_pmu in {}: https://github.com/darthunix/pg_pmu",
            pg.pkglibdir.display()
        );
    }
    let paths = if args.files.is_empty() {
        vec![root().join("target/clickbench-data")]
    } else {
        args.files.clone()
    };
    let files = cluster::files(&paths)?;
    std::fs::create_dir_all(runs_dir())?;
    if args.no_install {
        println!("Tessera: the build installed in {}", pg.pkglibdir.display());
    } else {
        let log = runs_dir().join("clickbench-install.log");
        println!(
            "Tessera: building the release libraries and installing them (log {})",
            log.display()
        );
        tessera_pgtool::install_tessera_with(&root(), &pg, &log, &[], deadline)?;
    }
    let duckdb = cluster::duckdb_installed(&pg);
    let settings = ServerSettings {
        port: args.port,
        socket_dir: "/tmp".into(),
        shared_buffers: args.shared_buffers.clone(),
        duckdb,
        pmu,
    };
    let cluster = Cluster::new(pg, &runs_dir(), settings);
    // A start may fail after the server is up, as when an extension
    // cannot be created: what ran before decides the stop.
    let was_running = cluster.running()?;
    if let Err(error) = cluster.start() {
        return Err(stopped(&cluster, was_running || keep, error));
    }
    println!(
        "cluster: {} on port {}, pg_duckdb {}",
        cluster.data.display(),
        cluster.settings.port,
        if duckdb { "preloaded" } else { "not installed" }
    );
    let prepared = (|| {
        let mut client = cluster.connect()?;
        if let Some(made) = cluster::tables(&mut client, &root(), &files, duckdb, deadline)? {
            println!("tables made and analyzed in {:.1} s", made.seconds);
        }
        cluster::describe(&mut client)
    })();
    let (rows, version) = match prepared {
        Ok(described) => described,
        Err(error) => return Err(stopped(&cluster, was_running || keep, error)),
    };
    println!("{} files, {rows} rows", files.len());
    Ok(Up {
        cluster,
        was_running,
        files,
        rows,
        duckdb: version,
    })
}

/// `error`, after the server is stopped unless `keep`: a failure to stop
/// it is added to the error, not put in its place.
fn stopped(cluster: &Cluster, keep: bool, error: anyhow::Error) -> anyhow::Error {
    if keep {
        return error;
    }
    match cluster.stop() {
        Ok(_) => error,
        Err(stop) => error.context(format!("and the server did not stop: {stop:#}")),
    }
}

fn with_cluster<T>(
    args: &ClusterArgs,
    deadline: Option<Instant>,
    pmu: bool,
    work: impl FnOnce(&Up) -> Result<T>,
) -> Result<T> {
    let up = bring_up(args, deadline, args.keep_running, pmu)?;
    let result = work(&up);
    if !up.was_running && !args.keep_running {
        up.cluster.stop()?;
    }
    result
}

/// The run's directory with what it runs on: source.txt,
/// pg_settings.txt and the scripts of bench/clickbench.
fn open_run(
    up: &Up,
    command: &str,
    modes: &[Mode],
    queries: &QueryArgs,
    timing: (u64, u64, u32),
    counters: bool,
) -> Result<(RunDir, Meta)> {
    let dir = RunDir::create(&runs_dir())?;
    let mut client = up.cluster.connect()?;
    let started: String = client.query_one("SELECT now()::text", &[])?.get(0);
    let (timeout_s, deadline_s, tries) = timing;
    let meta = Meta {
        id: dir.id.clone(),
        command: command.into(),
        modes: modes.iter().map(|mode| mode.name().to_string()).collect(),
        files: up
            .files
            .iter()
            .map(|file| file.display().to_string())
            .collect(),
        rows: up.rows,
        workers: queries.workers,
        work_mem: queries.work_mem.clone(),
        timeout_s,
        deadline_s,
        tries,
        head: tessera_pgtool::run::head(&root()),
        postgres: up.cluster.pg.version.clone(),
        duckdb: up.duckdb.clone(),
        started,
        power: tessera_pgtool::run::power(&root()),
        counters,
        duckdb_threads: Some(queries.duckdb_threads.unwrap_or(queries.workers + 1)),
    };
    let mut source = format!(
        "HEAD {}\ncommand {command}\nmodes {}\nworkers {}\nwork_mem {}\n\
         timeout {timeout_s} s\ndeadline {deadline_s} s\ntries {tries}\n\
         counters {counters}\nduckdb threads {:?}\n\
         pg_config {}\npostgres {}\npg_duckdb {}\nstarted {}\npower {}\nload average {}\n",
        meta.head,
        meta.modes.join(","),
        meta.workers,
        meta.work_mem,
        meta.duckdb_threads,
        up.cluster.pg.pg_config.display(),
        meta.postgres,
        meta.duckdb.as_deref().unwrap_or("none"),
        meta.started,
        meta.power,
        tessera_pgtool::run::load_average(&root()),
    );
    let _ = writeln!(source, "rows {}\nfiles:", up.rows);
    for file in &up.files {
        let size = std::fs::metadata(file).map_or(0, |metadata| metadata.len());
        let _ = writeln!(source, "  {size:>12}  {}", file.display());
    }
    let _ = writeln!(source, "sha256:");
    for file in cluster::binaries(&up.cluster.pg) {
        let _ = writeln!(
            source,
            "  {}  {}",
            tessera_pgtool::run::sha256(&file),
            file.display()
        );
    }
    dir.write("source.txt", &source)?;
    // The queries and the tables as they were run, which HEAD alone does
    // not give when the tree has changes.
    for name in ["queries.sql", "create.sql", "view.sql", "count.sql"] {
        let path = root().join("bench/clickbench").join(name);
        dir.write(name, &std::fs::read_to_string(&path)?)?;
    }
    let settings: String = client
        .query(
            "SELECT name || ' = ' || setting || coalesce(' ' || unit, '') FROM pg_settings \
             ORDER BY name",
            &[],
        )?
        .iter()
        .map(|row| row.get::<_, String>(0) + "\n")
        .collect();
    dir.write("pg_settings.txt", &settings)?;
    Ok((dir, meta))
}

fn settings(up: &Up, queries: &QueryArgs) -> Settings {
    Settings {
        workers: queries.workers,
        work_mem: queries.work_mem.clone(),
        duckdb: up.cluster.settings.duckdb,
        duckdb_threads: queries.duckdb_threads.unwrap_or(queries.workers + 1),
        counting: false,
    }
}

fn check_command(args: &CheckArgs, up: &Up) -> Result<()> {
    let available: Vec<Mode> = Mode::ALL
        .into_iter()
        .filter(|&mode| mode != Mode::Duckdb || up.cluster.settings.duckdb)
        .collect();
    let modes = args
        .modes
        .clone()
        .map_or_else(|| available.clone(), |modes| modes.0);
    if modes.first() != Some(&Mode::Off) {
        bail!("a check needs Tessera off, whose answers are the reference");
    }
    if let Some(mode) = modes.iter().find(|mode| !available.contains(mode)) {
        bail!("mode {mode}: pg_duckdb is not installed in this build");
    }
    let all = queries::load(&root())?;
    let (dir, meta) = open_run(
        up,
        "check",
        &modes,
        &args.queries,
        (args.timeout, 0, 1),
        false,
    )?;
    let settings = settings(up, &args.queries);
    let timeout = Duration::from_secs(args.timeout);
    let mut sessions = modes
        .iter()
        .map(|&mode| Session::open(&up.cluster, &settings, mode, timeout))
        .collect::<Result<Vec<_>>>()?;
    let heading = format!(
        "tessera-clickbench check, {}, modes {}, timeout {} s",
        meta.label(),
        meta.modes.join(","),
        args.timeout
    );
    println!("{heading}");
    let mut plans = vec![String::new(); modes.len()];
    let mut checks = Vec::new();
    for &number in &args.queries.queries.0 {
        let query = &all[number - 1];
        let checked = check::check(&mut sessions, query, &mut plans);
        let mut line = format!("{:<4}", query.name());
        for answer in &checked.answers {
            let label = answer.verdict.label();
            let shown = if answer.verdict.passed() {
                label
            } else {
                style(label).red().bold().to_string()
            };
            let _ = write!(
                line,
                "  {} {:.2} s {}{}",
                answer.mode,
                answer.seconds,
                shown,
                if answer.contained {
                    " (cut rows contained)"
                } else {
                    ""
                }
            );
        }
        let rows = checked.answers[0]
            .rows
            .map_or("-".into(), |rows| rows.to_string());
        println!("{line}  {rows} rows");
        checks.push(checked);
    }
    for (mode, plan) in modes.iter().zip(&plans) {
        dir.write(&format!("plans-{mode}.txt"), plan)?;
    }
    dir.write("results.txt", &check::results(&checks, &heading))?;
    let failed: Vec<String> = checks
        .iter()
        .filter(|checked| !checked.passed())
        .map(|checked| format!("Q{}", checked.query))
        .collect();
    let run = Run {
        meta,
        checks,
        times: Vec::new(),
    };
    dir.write("run.json", &serde_json::to_string_pretty(&run)?)?;
    println!("results: {}", dir.path.display());
    if !failed.is_empty() {
        bail!("answers differ or failed: {}", failed.join(", "));
    }
    Ok(())
}

fn run_command(args: &RunArgs, up: &Up, deadline: Instant) -> Result<()> {
    if args.mode == Mode::Duckdb && !up.cluster.settings.duckdb {
        bail!("mode duckdb: pg_duckdb is not installed in this build");
    }
    let all = queries::load(&root())?;
    let (dir, meta) = open_run(
        up,
        "run",
        &[args.mode],
        &args.queries,
        (args.timeout, args.deadline, args.tries),
        args.counters,
    )?;
    let settings = Settings {
        counting: args.counters,
        ..settings(up, &args.queries)
    };
    if args.counters {
        let path = root().join("bench/clickbench/count.sql");
        up.cluster
            .connect()?
            .batch_execute(&std::fs::read_to_string(&path)?)?;
    }
    let selected: Vec<&queries::Query> = args
        .queries
        .queries
        .0
        .iter()
        .map(|&number| &all[number - 1])
        .collect();
    // The plans the run times, for reading its times against.
    let mut plans = String::new();
    let left = deadline.saturating_duration_since(Instant::now());
    let mut session = Session::open(
        &up.cluster,
        &settings,
        args.mode,
        Duration::from_secs(args.timeout).min(left),
    )?;
    for query in &selected {
        // Each plan within what is left of the deadline, none past it.
        let left = deadline.saturating_duration_since(Instant::now());
        if left.as_secs() < 1 {
            plans.push_str("@@ the deadline left no time for the other plans\n");
            break;
        }
        session.set_timeout(Duration::from_secs(args.timeout).min(left))?;
        check::explain(&mut session, query, &mut plans);
    }
    drop(session);
    dir.write("plans.txt", &plans)?;
    println!(
        "tessera-clickbench run, {}, {} {}, timeout {} s, deadline {} s",
        meta.label(),
        if args.counters { "readings" } else { "tries" },
        args.tries,
        args.timeout,
        args.deadline
    );
    let timing = measure::Plan {
        mode: args.mode,
        settings: &settings,
        tries: args.tries,
        timeout: Duration::from_secs(args.timeout),
        deadline,
        counters: args.counters,
    };
    let times = measure::run(&up.cluster, &timing, &selected, |query, times| {
        let tries: Vec<String> = times
            .tries
            .iter()
            .map(|one| match (one.cycles, one.instructions, one.seconds) {
                (Some(cycles), Some(instructions), _) => format!(
                    "{:.3} Gcycles {:.3} Ginstructions",
                    cycles as f64 / 1e9,
                    instructions as f64 / 1e9
                ),
                (_, _, Some(seconds)) => format!("{seconds:.3}"),
                _ => one.outcome.clone(),
            })
            .collect();
        println!("{:<4} {}", query.name(), tries.join("  "));
    });
    dir.write("times.csv", &measure::csv(&times))?;
    let run = Run {
        meta,
        checks: Vec::new(),
        times,
    };
    dir.write("run.json", &serde_json::to_string_pretty(&run)?)?;
    let summary = report::single(&run);
    dir.write("summary.txt", &summary)?;
    println!("\n{summary}results: {}", dir.path.display());
    let unfinished: Vec<String> = run
        .times
        .iter()
        .filter(|times| {
            if args.counters {
                times.median(|one| one.cycles).is_none()
            } else {
                times.hot().is_none()
            }
        })
        .map(|times| format!("Q{}", times.query))
        .collect();
    if !unfinished.is_empty() {
        let what = if args.counters { "counted" } else { "timed" };
        println!("not {what}: {}", unfinished.join(", "));
    }
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Setup(args) => {
            bring_up(&args, None, args.keep_running, false)?;
            println!(
                "the server runs; psql -h /tmp -p {} {}, SET search_path = fdw or ddb, public; \
                 stop it with cargo clickbench stop",
                args.port,
                cluster::DATABASE
            );
        }
        Command::Check(args) => {
            with_cluster(&args.cluster, None, false, |up| check_command(&args, up))?;
        }
        Command::Run(args) => {
            // The deadline bounds the whole run, its build and tables too.
            let deadline = Instant::now() + Duration::from_secs(args.deadline);
            with_cluster(&args.cluster, Some(deadline), args.counters, |up| {
                run_command(&args, up, deadline)
            })?;
        }
        Command::Report { dirs } => {
            let runs = report::merged(
                dirs.iter()
                    .map(|dir| report::load(dir))
                    .collect::<Result<Vec<Run>>>()?,
            );
            if let [run] = &runs[..] {
                print!("{}", report::single(run));
            } else {
                print!("{}", report::side_by_side(&runs)?);
            }
        }
        Command::Stop(args) => {
            let pg = Pg::discover()?;
            let duckdb = cluster::duckdb_installed(&pg);
            let cluster = Cluster::new(
                pg,
                &runs_dir(),
                ServerSettings {
                    port: args.port,
                    socket_dir: "/tmp".into(),
                    shared_buffers: args.shared_buffers.clone(),
                    duckdb,
                    pmu: false,
                },
            );
            if cluster.stop()? {
                println!("stopped {}", cluster.data.display());
            } else {
                println!("{} does not run", cluster.data.display());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_interface_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_takes_a_mode_and_its_timing() {
        let cli = Cli::try_parse_from([
            "tessera-clickbench",
            "run",
            "--mode",
            "on",
            "--queries",
            "1-7",
            "--workers",
            "4",
            "--files",
            "/a.parquet,/b",
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!("{cli:?}");
        };
        assert_eq!(args.mode, Mode::On);
        assert_eq!(args.queries.queries.0, (1..=7).collect::<Vec<_>>());
        assert_eq!(args.queries.workers, 4);
        assert_eq!((args.tries, args.timeout, args.deadline), (3, 60, 300));
        assert_eq!(
            args.cluster.files,
            vec![PathBuf::from("/a.parquet"), PathBuf::from("/b")]
        );
        assert!(Cli::try_parse_from(["tessera-clickbench", "run"]).is_err());
        assert!(
            Cli::try_parse_from(["tessera-clickbench", "run", "--mode", "on", "--tries", "0"])
                .is_err()
        );
    }

    #[test]
    fn check_takes_its_modes() {
        let cli =
            Cli::try_parse_from(["tessera-clickbench", "check", "--modes", "on,off"]).unwrap();
        let Command::Check(args) = cli.command else {
            panic!("{cli:?}");
        };
        assert_eq!(args.modes, Some(Modes(vec![Mode::Off, Mode::On])));
        assert_eq!(args.timeout, 300);
        assert_eq!(args.queries.queries.0.len(), queries::COUNT);
        assert_eq!(args.queries.work_mem, "64MB");
        assert!(Cli::try_parse_from(["tessera-clickbench", "report"]).is_err());
    }
}

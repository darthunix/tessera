//! `cargo tpch`: the 22 queries derived from TPC-H on a PostgreSQL cluster
//! of the tool's own, with Tessera on and off. See `bench/tpch/README.md`.
#![forbid(unsafe_code)]

mod answers;
mod check;
mod cluster;
mod compare;
mod config;
mod load;
mod measure;
mod participation;
mod queries;
mod report;
mod rundir;
mod session;
mod stats;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use console::style;

use check::Sessions;
use cluster::{Cluster, Pg};
use config::{Scale, ServerSettings};
use load::Schema;
use queries::Selection;
use rundir::{Meta, RunDir};
use session::{Outcome, Session, SessionSettings};

#[derive(Debug, Parser)]
#[command(
    name = "tessera-tpch",
    about = "Queries derived from TPC-H on a PostgreSQL cluster of its own, with Tessera on and off",
    long_about = None,
)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Without a subcommand: run, with these arguments.
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Install the release build of Tessera, start the cluster of the
    /// scale factor, load the data unless it holds them, and leave it
    /// running.
    Setup(ClusterArgs),
    /// Check the answers: every query once with Tessera off and on, off
    /// against the published answer at SF 1, on against off. Times
    /// nothing, so it may run on a busy machine.
    Check(CheckArgs),
    /// Check, then time: the queries that answered the same in both modes,
    /// with the data in shared buffers, in pairs of one execution per mode
    /// in ABBA order. Run it on an idle machine.
    Run(RunArgs),
    /// Print a run again from its directory.
    Report {
        /// The run's directory, target/bench-runs/tpch-sf<N>-<id>.
        dir: PathBuf,
    },
    /// Compare two runs query by query: Tessera on in B against A, with
    /// off in B against A as the control of the machine. For two builds of
    /// Tessera: run on the base, run on the change, compare.
    Compare {
        /// The run of the base.
        a: PathBuf,
        /// The run of the change.
        b: PathBuf,
    },
    /// Stop the cluster of the scale factor; its data stays.
    Stop(ClusterArgs),
}

/// The cluster: one per scale factor, under target/bench-runs/.
#[derive(Debug, Clone, Args)]
struct ClusterArgs {
    /// Scale factor: 1 is 6 M rows of lineitem, about 1 GB of raw data.
    #[arg(long, default_value = "1", value_parser = Scale::parse)]
    sf: Scale,
    /// Port of the cluster's server.
    #[arg(long, default_value_t = 5434)]
    port: u16,
    /// shared_buffers of the server [default: 1.6 GB per scale unit, at
    /// least 2 GB].
    #[arg(long, value_name = "SIZE")]
    shared_buffers: Option<String>,
    /// Keep the Tessera build installed in PG_CONFIG's tree instead of
    /// building and installing the release one.
    #[arg(long)]
    no_install: bool,
    /// Variant of the schema: primary keys only, or with indexes on
    /// foreign keys and dates (bench/tpch/indexes.sql).
    #[arg(long, value_enum, default_value_t = Schema::Pk)]
    schema: Schema,
    /// Load the data anew even if the cluster holds them.
    #[arg(long)]
    reload: bool,
}

impl ClusterArgs {
    fn settings(&self) -> ServerSettings {
        ServerSettings {
            port: self.port,
            socket_dir: "/tmp".into(),
            shared_buffers: self
                .shared_buffers
                .clone()
                .unwrap_or_else(|| self.sf.default_shared_buffers()),
        }
    }

    fn cluster(&self, pg: Pg) -> Cluster {
        Cluster::new(pg, &runs_dir(), &self.sf, self.settings())
    }
}

/// Which queries run and under what settings.
#[derive(Debug, Clone, Args)]
struct QueryArgs {
    /// Queries: numbers and ranges (1,3,6 or 1-5), core (Q1, Q3, Q6, Q9,
    /// Q18) or all.
    #[arg(long, default_value = "all", value_parser = Selection::parse)]
    queries: Selection,
    /// max_parallel_workers_per_gather in both modes.
    #[arg(long, default_value_t = 0)]
    workers: u32,
    /// work_mem in both modes.
    #[arg(long, default_value = "256MB", value_name = "SIZE")]
    work_mem: String,
    /// Run with jit = on (off by default).
    #[arg(long)]
    jit: bool,
    /// statement_timeout of a query, in seconds [default: 30 per scale
    /// unit, at least 30].
    #[arg(long, value_name = "SECONDS")]
    timeout: Option<u64>,
}

impl QueryArgs {
    fn timeout(&self, scale: &Scale) -> u64 {
        self.timeout
            .unwrap_or_else(|| (30.0 * scale.factor().max(1.0)).ceil() as u64)
    }

    fn session(&self, scale: &Scale) -> SessionSettings {
        SessionSettings {
            workers: self.workers,
            work_mem: self.work_mem.clone(),
            jit: self.jit,
            timeout: Duration::from_secs(self.timeout(scale)),
        }
    }
}

#[derive(Debug, Clone, Args)]
struct CheckArgs {
    #[command(flatten)]
    cluster: ClusterArgs,
    #[command(flatten)]
    queries: QueryArgs,
    /// Leave the server running at the end even if this command started
    /// it.
    #[arg(long)]
    keep_running: bool,
    /// Write the plans of this run into the golden file of participation
    /// (bench/tpch/participation-sf<N>*.txt) instead of comparing them.
    #[arg(long)]
    update_golden: bool,
}

/// What `run` adds to the check.
#[derive(Debug, Clone, Args)]
struct RunArgs {
    #[command(flatten)]
    check: CheckArgs,
    /// Pairs of executions per query, one per mode, in ABBA order.
    #[arg(long, default_value_t = 11, value_parser = clap::value_parser!(u32).range(1..))]
    pairs: u32,
    /// Untimed executions per mode before the pairs.
    #[arg(long, default_value_t = 1)]
    warmups: u32,
    /// A ratio of on to off within this many percent of one is even.
    #[arg(long, default_value_t = 3.0)]
    threshold: f64,
}

/// The repository's root, which holds the queries and the run directories.
fn root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize().unwrap_or(root)
}

/// Where clusters and runs live, as with every measurement of the
/// repository: git ignores it.
fn runs_dir() -> PathBuf {
    root().join("target/bench-runs")
}

/// A cluster brought up for a command.
struct Up {
    cluster: Cluster,
    /// Whether its server ran before, which it then keeps doing.
    was_running: bool,
    sizes: Vec<(String, i64)>,
}

/// Installs Tessera unless asked not to, starts the cluster and loads
/// the data it lacks.
fn bring_up(args: &ClusterArgs) -> Result<Up> {
    let pg = Pg::discover()?;
    pg.require_contrib(&cluster::CONTRIB)?;
    std::fs::create_dir_all(runs_dir())?;
    if args.no_install {
        println!("Tessera: the build installed in {}", pg.pkglibdir.display());
    } else {
        let log = runs_dir().join("tpch-install.log");
        println!(
            "Tessera: building the release libraries and installing them (log {})",
            log.display()
        );
        cluster::install_tessera(&root(), &pg, &log)?;
    }
    let cluster = args.cluster(pg);
    let was_running = cluster.start()?;
    println!(
        "cluster: {} on port {}, shared_buffers {}",
        cluster.data.display(),
        cluster.settings.port,
        cluster.settings.shared_buffers
    );
    match load::ensure(&cluster, &root(), &args.sf, args.schema, args.reload)? {
        Some(loaded) => {
            println!(
                "loaded in {:.0} s; rows as clause 4.2.5 gives them:",
                loaded.seconds
            );
            for table in &loaded.tables {
                println!(
                    "  {:>9} {:>11} rows {:>9} pages, {} all-visible",
                    table.table.name(),
                    table.rows,
                    table.pages,
                    table.all_visible
                );
            }
        }
        None => println!("data: SF {} of {} is loaded", args.sf, load::GENERATOR),
    }
    let mut client = cluster.connect()?;
    let sizes = load::sizes(&mut client)?;
    let total: i64 = sizes.iter().map(|(_, size)| size).sum();
    println!(
        "schema {}, {} with indexes: {}",
        args.schema.name(),
        load::human_size(total),
        sizes
            .iter()
            .map(|(table, size)| format!("{table} {}", load::human_size(*size)))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(Up {
        cluster,
        was_running,
        sizes,
    })
}

/// Runs `work` on the cluster, then stops the server if the command
/// started it and was not asked to keep it.
fn with_cluster<T>(
    args: &ClusterArgs,
    keep_running: bool,
    work: impl FnOnce(&Up) -> Result<T>,
) -> Result<T> {
    let up = bring_up(args)?;
    let result = work(&up);
    if !up.was_running && !keep_running {
        up.cluster.stop()?;
    }
    result
}

/// The run's directory, with what it was run on.
fn open_run(
    command: &str,
    up: &Up,
    args: &CheckArgs,
    timing: Option<&RunArgs>,
) -> Result<(RunDir, Meta)> {
    let (cluster, queries) = (&args.cluster, &args.queries);
    let dir = RunDir::create(&runs_dir(), &cluster.sf)?;
    let mut client = up.cluster.connect()?;
    let started: String = client.query_one("SELECT now()::text", &[])?.get(0);
    let meta = Meta {
        id: dir.id.clone(),
        command: command.into(),
        sf: cluster.sf.to_string(),
        schema: cluster.schema.name().into(),
        workers: queries.workers,
        work_mem: queries.work_mem.clone(),
        jit: queries.jit,
        timeout_s: queries.timeout(&cluster.sf),
        head: rundir::head(&root()),
        postgres: up.cluster.pg.version.clone(),
        started,
        power: rundir::power(&root()),
        pairs: timing.map_or(0, |timing| timing.pairs),
        warmups: timing.map_or(0, |timing| timing.warmups),
        threshold: timing.map_or(0.0, |timing| timing.threshold),
    };
    dir.write(
        "source.txt",
        &rundir::source(&root(), &up.cluster, &meta, &up.sizes),
    )?;
    Ok((dir, meta))
}

/// The settings of the run, as the connection that runs Tessera sees
/// them.
fn pg_settings(session: &mut Session<'_>) -> String {
    match session
        .run("SELECT name || ' = ' || setting || coalesce(' ' || unit, '') FROM pg_settings ORDER BY name")
        .outcome
    {
        Outcome::Rows(rows) => rows
            .into_iter()
            .map(|row| row.into_iter().next().flatten().unwrap_or_default() + "\n")
            .collect(),
        outcome => format!("{outcome:?}\n"),
    }
}

/// A verdict for the console: anything but `same` in red.
fn painted(verdict: Option<&check::Verdict>) -> String {
    match verdict {
        None => "-".into(),
        Some(check::Verdict::Same) => "same".into(),
        Some(verdict) => style(verdict.label()).red().bold().to_string(),
    }
}

/// A check done: its directory, its record so far, and the connections
/// that ran it, with the statements the timing runs.
struct Checked<'c> {
    dir: RunDir,
    run: rundir::Run,
    sessions: Sessions<'c>,
}

/// The check of the answers and the plans, as `check` and `run` begin.
fn check_phase<'c>(
    args: &CheckArgs,
    up: &'c Up,
    command: &str,
    timing: Option<&RunArgs>,
) -> Result<Checked<'c>> {
    let (dir, meta) = open_run(command, up, args, timing)?;
    let settings = args.queries.session(&args.cluster.sf);
    let mut sessions = Sessions {
        off: Session::open(&up.cluster, &settings, false)?,
        on: Session::open(&up.cluster, &settings, true)?,
    };
    dir.write("pg_settings.txt", &pg_settings(&mut sessions.on))?;
    let heading = format!(
        "tessera-tpch {command}, SF {}, schema {}, workers {}, work_mem {}, jit {}, timeout {} s",
        meta.sf,
        meta.schema,
        meta.workers,
        meta.work_mem,
        if meta.jit { "on" } else { "off" },
        meta.timeout_s
    );
    println!("{heading}");
    let checks = check::run(
        &mut sessions,
        &root(),
        &args.cluster.sf,
        &args.queries.queries.0,
        |check| {
            let query = queries::get(check.query);
            println!(
                "{} {:<34} {:>6} rows  reference {:<8} on/off {:<8} {}",
                query.name(),
                query.title,
                check.rows.map_or("-".into(), |rows| rows.to_string()),
                painted(check.reference.as_ref()),
                painted(Some(&check.on_off)),
                check.detail().unwrap_or_default()
            );
        },
    )?;
    dir.write("results.txt", &check::results(&checks, &heading))?;
    println!("plans: EXPLAIN (ANALYZE, TIMING OFF) of every query that answered");
    let participation = participation::run(&mut sessions, &checks, &dir.file("plans"))?;
    dir.write("participation.txt", &participation::table(&participation))?;
    golden(args, &meta, &participation)?;
    Ok(Checked {
        dir,
        run: rundir::Run {
            meta,
            checks,
            participation,
            samples: Vec::new(),
            failures: Vec::new(),
        },
        sessions,
    })
}

/// Writes run.json and fails if an answer was wrong, Tessera off ran a
/// Tessera node or a timed execution went wrong.
fn finish(checked: &Checked<'_>) -> Result<()> {
    let run = &checked.run;
    checked
        .dir
        .write("run.json", &serde_json::to_string_pretty(run)?)?;
    checked.dir.write("summary.md", &report::markdown(run)?)?;
    println!();
    report::print(run)?;
    println!("results: {}", checked.dir.path.display());
    let name = |query: u8| queries::get(query).name();
    let mut failed: Vec<String> = run
        .checks
        .iter()
        .filter(|check| check.failed())
        .map(|check| format!("{} {}", name(check.query), check.on_off.label()))
        .collect();
    failed.extend(run.participation.iter().filter_map(|query| {
        query
            .fault()
            .map(|fault| format!("{} {fault}", name(query.query)))
    }));
    failed.extend(run.failures.iter().map(|failure| failure.reason.clone()));
    if !failed.is_empty() {
        bail!("failed: {}", failed.join(", "));
    }
    Ok(())
}

fn check_command(args: &CheckArgs, up: &Up) -> Result<()> {
    let checked = check_phase(args, up, "check", None)?;
    finish(&checked)
}

fn run_command(args: &RunArgs, up: &Up) -> Result<()> {
    let mut checked = check_phase(&args.check, up, "run", Some(args))?;
    let mut client = up.cluster.connect()?;
    let (share, bytes) = measure::prewarm(&mut client)?;
    println!(
        "shared buffers: {share:.1} % of the {} of tables and indexes",
        load::human_size(bytes)
    );
    if share < 99.0 {
        println!("warning: the data does not fit in shared buffers; raise --shared-buffers");
    }
    let plan = measure::Plan {
        warmups: args.warmups,
        pairs: args.pairs,
    };
    let timed = checked
        .run
        .checks
        .iter()
        .filter(|check| measure::timeable(check))
        .count();
    let seconds = plan.estimate_seconds(&checked.run.checks);
    let estimate = if seconds < 90.0 {
        format!("{seconds:.0} s")
    } else {
        format!("{:.0} min", (seconds / 60.0).ceil())
    };
    println!(
        "timing {timed} queries: {} warm-up and {} pairs of executions each, about {estimate}",
        plan.warmups, plan.pairs,
    );
    let (samples, failures) = measure::run(
        &mut checked.sessions,
        &checked.run.checks,
        plan,
        |summary| {
            println!(
                "{}  off {:>9.1} ms  on {:>9.1} ms  on/off {:.3} [{:.3}, {:.3}]",
                queries::get(summary.query).name(),
                summary.off_median,
                summary.on_median,
                summary.ratio,
                summary.low,
                summary.high
            );
        },
    )?;
    checked.dir.write("timings.csv", &measure::csv(&samples))?;
    checked.run.samples = samples;
    checked.run.failures = failures;
    finish(&checked)
}

/// Compares the plans with the golden file of their settings, or
/// rewrites it.
fn golden(args: &CheckArgs, meta: &Meta, all: &[participation::QueryParticipation]) -> Result<()> {
    let Some(path) = participation::golden_path(
        &root(),
        &meta.sf,
        &meta.schema,
        meta.workers,
        &meta.work_mem,
    ) else {
        println!(
            "participation: no golden file for work_mem {}",
            meta.work_mem
        );
        return Ok(());
    };
    if args.update_golden {
        participation::update_golden(&path, all)?;
        println!("participation: wrote {}", path.display());
        return Ok(());
    }
    if !path.exists() {
        println!(
            "participation: no golden file {}; check --update-golden writes it",
            path.display()
        );
        return Ok(());
    }
    let differences = participation::compare_golden(&path, all)?;
    if differences.is_empty() {
        println!("participation: as in {}", path.display());
    }
    for (query, golden, now) in differences {
        println!("participation of {query} differs from {}:", path.display());
        println!("  golden: {golden}");
        println!("  now:    {now}");
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        let args = cli.run;
        return with_cluster(&args.check.cluster, args.check.keep_running, |up| {
            run_command(&args, up)
        });
    };
    match command {
        Command::Report { dir } => {
            let (dir, run) = report::load(&dir)?;
            report::print(&run)?;
            println!("results: {}", dir.display());
        }
        Command::Compare { a, b } => {
            let (_, a) = report::load(&a)?;
            let (_, b) = report::load(&b)?;
            report::compare(&a, &b)?;
        }
        Command::Setup(args) => {
            bring_up(&args)?;
            println!(
                "the server runs; psql -h /tmp -p {} {}; stop it with cargo tpch stop --sf {}",
                args.port,
                cluster::DATABASE,
                args.sf
            );
        }
        Command::Check(args) => {
            with_cluster(&args.cluster, args.keep_running, |up| {
                check_command(&args, up)
            })?;
        }
        Command::Run(args) => {
            with_cluster(&args.check.cluster, args.check.keep_running, |up| {
                run_command(&args, up)
            })?;
        }
        Command::Stop(args) => {
            let cluster = args.cluster(Pg::discover()?);
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
    fn setup_takes_the_cluster_arguments() {
        let cli = Cli::try_parse_from([
            "tessera-tpch",
            "setup",
            "--sf",
            "10",
            "--port",
            "5500",
            "--no-install",
            "--schema",
            "indexed",
        ])
        .unwrap();
        let Some(Command::Setup(args)) = cli.command else {
            panic!("{cli:?}");
        };
        assert_eq!(args.sf.to_string(), "10");
        assert!(args.no_install);
        assert_eq!(args.schema, Schema::Indexed);
        let settings = args.settings();
        assert_eq!(settings.port, 5500);
        assert_eq!(settings.shared_buffers, "16GB");
    }

    #[test]
    fn shared_buffers_can_be_set() {
        let cli = Cli::try_parse_from(["tessera-tpch", "stop", "--shared-buffers", "4GB"]).unwrap();
        let Some(Command::Stop(args)) = cli.command else {
            panic!("{cli:?}");
        };
        assert_eq!(args.sf.to_string(), "1");
        assert_eq!(args.settings().shared_buffers, "4GB");
    }

    #[test]
    fn a_bad_scale_is_refused() {
        assert!(Cli::try_parse_from(["tessera-tpch", "setup", "--sf", "-1"]).is_err());
    }

    #[test]
    fn without_a_subcommand_it_runs() {
        let cli = Cli::try_parse_from(["tessera-tpch", "--sf", "10", "--queries", "6"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.run.check.cluster.sf.to_string(), "10");
        assert_eq!(cli.run.check.queries.queries.0, vec![6]);
        assert_eq!(cli.run.pairs, 11);
        let cli = Cli::try_parse_from(["tessera-tpch", "compare", "a", "b"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Compare { .. })));
        assert!(Cli::try_parse_from(["tessera-tpch", "--sf", "1", "check"]).is_err());
    }

    #[test]
    fn run_takes_the_timing() {
        let cli = Cli::try_parse_from(["tessera-tpch", "run", "--queries", "core", "--pairs", "3"])
            .unwrap();
        let Some(Command::Run(args)) = cli.command else {
            panic!("{cli:?}");
        };
        assert_eq!(args.check.queries.queries.0, vec![1, 3, 6, 9, 18]);
        assert_eq!((args.pairs, args.warmups, args.threshold), (3, 1, 3.0));
        assert!(Cli::try_parse_from(["tessera-tpch", "run", "--pairs", "0"]).is_err());
    }

    #[test]
    fn check_takes_queries_and_settings() {
        let cli = Cli::try_parse_from([
            "tessera-tpch",
            "check",
            "--sf",
            "10",
            "--queries",
            "core,2",
            "--workers",
            "2",
            "--work-mem",
            "4MB",
        ])
        .unwrap();
        let Some(Command::Check(args)) = cli.command else {
            panic!("{cli:?}");
        };
        assert_eq!(args.queries.queries.0, vec![1, 2, 3, 6, 9, 18]);
        let settings = args.queries.session(&args.cluster.sf);
        assert_eq!(settings.workers, 2);
        assert_eq!(settings.work_mem, "4MB");
        assert!(!settings.jit);
        assert_eq!(settings.timeout, Duration::from_secs(300));
        assert_eq!(
            QueryArgs {
                timeout: None,
                ..args.queries.clone()
            }
            .timeout(&Scale::parse("0.01").unwrap()),
            30
        );
    }
}

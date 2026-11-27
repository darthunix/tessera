//! tessera-crosscheck: random queries with Tessera on and off (plan 9.8).
//!
//! The SQL suites compare about a thousand queries in both modes, each
//! written by hand for what someone thought of; combinations of types,
//! expressions, NULLs, nodes, parallelism and spilling are not enumerated.
//! This tool draws queries over three tables of edge-leaning data from a
//! seed, runs each with Tessera on and off in one connection and compares
//! the rows as a multiset and the errors by SQLSTATE. A query whose modes
//! disagree, or whose backend crashes, is shrunk by proptest to a small
//! one that still does, and written with the data's SQL as a ready case
//! for a suite; the run then goes on with the next query. Seeds that ever
//! found something are kept in seeds.txt and run first.
#![forbid(unsafe_code)]

mod ast;
mod generate;
mod oracle;
mod schema;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use proptest::test_runner::{Config, RngAlgorithm, TestCaseError, TestError, TestRng, TestRunner};
use tessera_pgtool::{Cluster, ClusterSpec, Pg, install_tessera};

use ast::Query;
use oracle::{Oracle, Verdict};

#[derive(Parser)]
#[command(
    about = "Random queries with Tessera on and off, compared; disagreements shrunk to small cases",
    after_help = "Exit: 0 no finding, 1 findings (written under target/bench-runs/crosscheck), 2 an error."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Stop the tool's server; its data stays.
    Stop {
        #[arg(long, default_value_t = 5435)]
        port: u16,
    },
}

#[derive(Args)]
struct RunArgs {
    /// The seed of the data and the queries; a new one from the clock by
    /// default, printed.
    #[arg(long)]
    seed: Option<u64>,
    /// Queries per seed.
    #[arg(long, default_value_t = 200)]
    queries: u32,
    /// Stop drawing queries after this many seconds; 0 for no limit.
    #[arg(long, default_value_t = 0)]
    time_limit: u64,
    /// Rows of the largest table.
    #[arg(long, default_value_t = 2000)]
    rows: u32,
    /// Port of the tool's server.
    #[arg(long, default_value_t = 5435)]
    port: u16,
    /// Keep the Tessera build installed in PG_CONFIG's tree instead of
    /// building and installing the release one.
    #[arg(long)]
    no_install: bool,
    /// Do not run the seeds of seeds.txt first.
    #[arg(long)]
    no_replay: bool,
}

fn root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize().unwrap_or(root)
}

fn runs_dir() -> PathBuf {
    root().join("target/bench-runs")
}

/// Queries per replayed seed: the count its finding was recorded with.
const REPLAY_QUERIES: u32 = 200;

/// The seeds that ever found something, one per line, `#` comments.
fn replayed_seeds() -> Vec<u64> {
    let text = include_str!("../seeds.txt");
    text.lines()
        .filter_map(|line| line.split('#').next())
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

fn cluster(pg: Pg, port: u16) -> Cluster {
    let conf = format!(
        "# Written by tessera-crosscheck at every start; changes are lost.
port = {port}
listen_addresses = ''
unix_socket_directories = '/tmp'
shared_preload_libraries = 'tessera, tessera_nodes, tessera_kernels'
shared_buffers = 128MB
jit = off
max_worker_processes = 8
max_parallel_workers = 8
autovacuum = off
fsync = off
synchronous_commit = off
"
    );
    let spec = ClusterSpec {
        name: "crosscheck".into(),
        database: "crosscheck".into(),
        application: "tessera-crosscheck".into(),
        conf_name: "tessera-crosscheck.conf".into(),
        conf,
        port,
        socket_dir: "/tmp".into(),
        // UTF8, and the C collation where a query names none: text compares
        // by its bytes, and under the collations the setup makes as they do.
        initdb_args: vec!["--no-locale".into(), "--encoding=UTF8".into()],
        extensions: vec!["tessera".into(), "tessera_parquet".into()],
        stop_hint: "cargo run -p tessera-crosscheck -- stop".into(),
    };
    Cluster::new(pg, &runs_dir(), spec)
}

/// The counts of a seed's run.
#[derive(Default)]
struct Counts {
    queries: u32,
    with_tessera: u32,
    over_twin: u32,
    kinds: BTreeMap<&'static str, u32>,
}

/// A finding: the seed, the shrunk query and what its modes did on a run
/// of its own, and the query first found with what its modes did then.
struct Finding {
    seed: u64,
    /// Its place among the findings of its seed's run, from 1.
    number: usize,
    query: Query,
    verdict: Verdict,
    original: (Query, Verdict),
}

impl Finding {
    /// What the finding is, and whether it stood on the run of its own: a
    /// parallel plan can raise an error, or not, by which participant
    /// reaches a row first.
    fn headline(&self) -> String {
        let class = self.original.1.class().unwrap_or("finding");
        let class = if self.original.1.twin {
            format!("{class} over the Parquet twin")
        } else {
            class.to_string()
        };
        if self.verdict.is_finding() {
            class
        } else {
            format!("{class}, not repeated by the shrunk query on a run of its own")
        }
    }
}

/// Runs the queries of one seed and gives `report` each finding, shrunk,
/// when it is made: an error later in the run loses none.
fn run_seed(
    oracle: &RefCell<Oracle<'_>>,
    seed: u64,
    queries: u32,
    args: &RunArgs,
    deadline: Option<Instant>,
    report: &mut dyn FnMut(&Finding) -> Result<()>,
) -> Result<Counts> {
    oracle.borrow_mut().load(&schema::setup(seed, args.rows))?;
    let config = Config {
        failure_persistence: None,
        // Each step of shrinking runs a query twice: a minute at most.
        max_shrink_iters: 10_000,
        max_shrink_time: 60_000,
        ..Config::default()
    };
    let mut seed_bytes = [0_u8; 32];
    seed_bytes[..8].copy_from_slice(&seed.to_le_bytes());
    let mut rng = TestRng::from_seed(RngAlgorithm::ChaCha, &seed_bytes);
    let counts = RefCell::new(Counts::default());
    // Once a query fails, the calls that follow shrink it: not counted,
    // and a failure of another class does not stand for it.
    let original: RefCell<Option<(Query, Verdict)>> = RefCell::new(None);
    // A server that failed a query fails the run: the calls of shrinking
    // fail at once, so that none is counted or makes a finding.
    let broken: RefCell<Option<String>> = RefCell::new(None);
    let test = |query: Query| {
        if let Some(reason) = broken.borrow().as_ref() {
            return Err(TestCaseError::fail(reason.clone()));
        }
        let shrinking = original
            .borrow()
            .as_ref()
            .filter(|(_, verdict)| verdict.class().is_some())
            .map(|(_, verdict)| verdict.clone());
        if deadline.is_some_and(|deadline| Instant::now() > deadline) && shrinking.is_none() {
            return Ok(());
        }
        let mut oracle = oracle.borrow_mut();
        let verdict = match oracle.check(&query) {
            Ok(verdict) => verdict,
            // While shrinking, a query the server fails on stands for no
            // finding; the last check reports a server that stays down.
            Err(_) if shrinking.is_some() => return Ok(()),
            Err(error) => {
                let reason = format!("the server: {error:#}");
                *broken.borrow_mut() = Some(reason.clone());
                return Err(TestCaseError::fail(reason));
            }
        };
        match shrinking {
            Some(original) if oracle::same_finding(&original, &verdict) => {
                Err(TestCaseError::fail(verdict.describe()))
            }
            Some(_) => Ok(()),
            None => {
                let mut counts = counts.borrow_mut();
                counts.queries += 1;
                *counts.kinds.entry(verdict.kind()).or_default() += 1;
                if verdict.is_finding() {
                    let describe = verdict.describe();
                    *original.borrow_mut() = Some((query, verdict));
                    return Err(TestCaseError::fail(describe));
                }
                if oracle.uses_tessera(&query).unwrap_or(false) {
                    counts.with_tessera += 1;
                }
                counts.over_twin += u32::from(oracle::reads_twinned(&query.sql()));
                Ok(())
            }
        }
    };
    // A run stops at its first failure, shrunk. The queries then go on
    // from the next one: a query's draw reads the runner's generator,
    // its shrinking does not, so a run from where that one stopped draws
    // what the stopped run would have.
    let mut number = 0;
    loop {
        let cases = queries.saturating_sub(counts.borrow().queries);
        let mut runner = TestRunner::new_with_rng(
            Config {
                cases,
                ..config.clone()
            },
            rng,
        );
        let result = runner.run(&generate::query(), test);
        rng = runner.rng().clone();
        match result {
            Ok(()) => break,
            Err(TestError::Fail(reason, query)) => {
                // Without a first finding the failure is the server's.
                let Some(original) = original.take() else {
                    anyhow::bail!("{reason}");
                };
                number += 1;
                let checked = oracle.borrow_mut().check(&query);
                // A server that stays down after a crash: the query first
                // found is the finding.
                let (query, verdict) = match &checked {
                    Ok(verdict) => (query, verdict.clone()),
                    Err(_) => original.clone(),
                };
                report(&Finding {
                    seed,
                    number,
                    query,
                    verdict,
                    original,
                })?;
                checked?;
            }
            Err(TestError::Abort(reason)) => anyhow::bail!("the run stopped: {reason}"),
        }
    }
    Ok(counts.into_inner())
}

/// Writes a finding as a file a suite case can be made of: the data's
/// SQL, the settings and the query in both modes.
fn write_finding(
    finding: &Finding,
    args: &RunArgs,
    twin_files: &[PathBuf],
    directory: &Path,
) -> Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let path = directory.join(format!("finding-{}-{}.sql", finding.seed, finding.number));
    let sql = finding.query.sql();
    let comment = |text: String| text.replace('\n', "\n-- ");
    // A finding over the twin, first or on its run of its own, keeps the
    // seed's files of the twin beside it: the next seed writes others.
    let twin = if finding.original.1.twin || finding.verdict.twin {
        let mut copies = Vec::new();
        for (index, file) in twin_files.iter().enumerate() {
            let copy = directory.join(format!(
                "finding-{}-{}-words-{index}.parquet",
                finding.seed, finding.number
            ));
            fs::copy(file, &copy)?;
            copies.push(copy);
        }
        format!(
            "-- over the Parquet twin of words, from the files beside this one:\n\
             CREATE EXTENSION IF NOT EXISTS tessera_parquet;\n{}\n\
             SET tessera.enable = on;\n{};\n",
            oracle::twin_table(&copies),
            oracle::twinned(&sql)
        )
    } else {
        String::new()
    };
    let text = format!(
        "-- tessera-crosscheck finding {number}, seed {seed}, {rows} rows: {headline}\n\
         -- {verdict}\n\
         -- first found as:\n-- {original}\n-- {original_verdict}\n\n\
         {setup}\n{settings}\n\
         SET tessera.enable = on;\n{sql};\nSET tessera.enable = off;\n{sql};\n{twin}",
        number = finding.number,
        seed = finding.seed,
        rows = args.rows,
        headline = finding.headline(),
        verdict = comment(finding.verdict.describe()),
        original = comment(finding.original.0.to_string()),
        original_verdict = comment(finding.original.1.describe()),
        setup = schema::setup(finding.seed, args.rows),
        settings = finding.query.settings.statements(),
    );
    fs::write(&path, text)?;
    Ok(path)
}

fn run(args: RunArgs) -> Result<u8> {
    let pg = Pg::discover()?;
    if !args.no_install {
        let log = runs_dir().join("crosscheck-install.log");
        fs::create_dir_all(runs_dir())?;
        println!("building and installing the release build of Tessera");
        install_tessera(&root(), &pg, &log)?;
    }
    let cluster = cluster(pg, args.port);
    cluster.start()?;
    let oracle = RefCell::new(Oracle::connect(&cluster)?);
    let seed = args.seed.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(1, |since| since.as_nanos() as u64)
    });
    let mut seeds: Vec<(u64, u32)> = if args.no_replay {
        Vec::new()
    } else {
        replayed_seeds()
            .into_iter()
            .map(|seed| (seed, REPLAY_QUERIES))
            .collect()
    };
    seeds.push((seed, args.queries));
    let deadline =
        (args.time_limit > 0).then(|| Instant::now() + Duration::from_secs(args.time_limit));
    let mut findings = 0;
    let mut report = |finding: &Finding| -> Result<()> {
        findings += 1;
        let path = write_finding(
            finding,
            &args,
            &oracle.borrow().twin_files(),
            &runs_dir().join("crosscheck"),
        )?;
        println!(
            "FINDING ({}), shrunk:\n{}\n{}\nfirst found as:\n{}\n{}\nwritten to {}",
            finding.headline(),
            finding.query,
            finding.verdict.describe(),
            finding.original.0,
            finding.original.1.describe(),
            path.display()
        );
        Ok(())
    };
    for (seed, queries) in seeds {
        let started = Instant::now();
        let counts = run_seed(&oracle, seed, queries, &args, deadline, &mut report)
            .with_context(|| format!("seed {seed}"))?;
        let kinds: Vec<String> = counts
            .kinds
            .iter()
            .map(|(kind, count)| format!("{count} {kind}"))
            .collect();
        println!(
            "seed {seed}: {} queries in {:.1} s ({}), {} with a Tessera node, {} over the Parquet twin",
            counts.queries,
            started.elapsed().as_secs_f64(),
            kinds.join(", "),
            counts.with_tessera,
            counts.over_twin,
        );
    }
    Ok(u8::from(findings > 0))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::Stop { port }) => Pg::discover().and_then(|pg| {
            let cluster = cluster(pg, port);
            if cluster.stop()? {
                println!("stopped {}", cluster.data.display());
            } else {
                println!("{} does not run", cluster.data.display());
            }
            Ok(0)
        }),
        None => run(cli.run),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("ERROR: {error:#}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Expr, Select, Settings};
    use crate::oracle::Outcome;

    /// A finding first met over the twin keeps the query over the twin and
    /// copies of the seed's files of it, which the SQL reads, though its
    /// shrunk query agrees on its run of its own; the next seed's files do
    /// not touch the copies.
    #[test]
    fn a_finding_over_the_twin_keeps_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let sources: Vec<PathBuf> = (0..3)
            .map(|index| dir.path().join(format!("words-{index}.parquet")))
            .collect();
        for (index, source) in sources.iter().enumerate() {
            fs::write(source, format!("seed's {index}")).unwrap();
        }
        let query = Query {
            select: Select {
                distinct: false,
                items: vec![Expr::Column {
                    table: 0,
                    name: "t",
                }],
                from: schema::WORDS,
                joins: Vec::new(),
                filter: None,
                grouping: None,
                limit: None,
                ordered: false,
            },
            set: None,
            settings: Settings {
                workers: 0,
                work_mem: "64kB",
            },
        };
        let verdict = |off: &str, twin| Verdict {
            on: Outcome::Rows(Some("{a}".into())),
            off: Outcome::Rows(Some(off.into())),
            relaxed: None,
            twin,
        };
        let finding = Finding {
            seed: 9,
            number: 2,
            query: query.clone(),
            verdict: verdict("{a}", false),
            original: (query, verdict("{b}", true)),
        };
        let args = Cli::parse_from(["tessera-crosscheck"]).run;
        let out = dir.path().join("crosscheck");
        let path = write_finding(&finding, &args, &sources, &out).unwrap();
        assert_eq!(path, out.join("finding-9-2.sql"));
        let text = fs::read_to_string(path).unwrap();
        assert!(text.contains("FROM words_pq AS t0"), "{text}");
        for (index, source) in sources.iter().enumerate() {
            let copy = out.join(format!("finding-9-2-words-{index}.parquet"));
            assert!(text.contains(&copy.display().to_string()), "{text}");
            fs::write(source, "the next seed's").unwrap();
            assert_eq!(
                fs::read_to_string(&copy).unwrap(),
                format!("seed's {index}")
            );
        }
        assert!(text.contains("over the Parquet twin"), "{text}");
    }

    /// What run_seed counts on to go on after a finding: a run started
    /// from the generator where a failed run stopped, after shrinking,
    /// draws the queries the whole run would have drawn after that one.
    #[test]
    fn a_run_from_where_a_finding_stopped_draws_what_the_whole_run_would() {
        let config = Config {
            failure_persistence: None,
            max_shrink_iters: 1000,
            ..Config::default()
        };
        let rng = || TestRng::from_seed(RngAlgorithm::ChaCha, &[7; 32]);
        let whole = RefCell::new(Vec::new());
        TestRunner::new_with_rng(
            Config {
                cases: 40,
                ..config.clone()
            },
            rng(),
        )
        .run(&generate::query(), |query| {
            whole.borrow_mut().push(query.sql());
            Ok(())
        })
        .unwrap();
        let parts = RefCell::new(Vec::new());
        let shrinking = std::cell::Cell::new(false);
        let mut first = TestRunner::new_with_rng(
            Config {
                cases: 40,
                ..config.clone()
            },
            rng(),
        );
        let result = first.run(&generate::query(), |query| {
            // Every step of shrinking fails too: the longest shrinking.
            if shrinking.get() {
                return Err(TestCaseError::fail("shrinking"));
            }
            parts.borrow_mut().push(query.sql());
            if parts.borrow().len() == 10 {
                shrinking.set(true);
                return Err(TestCaseError::fail("a finding"));
            }
            Ok(())
        });
        assert!(matches!(result, Err(TestError::Fail(..))));
        TestRunner::new_with_rng(
            Config {
                cases: 30,
                ..config
            },
            first.rng().clone(),
        )
        .run(&generate::query(), |query| {
            parts.borrow_mut().push(query.sql());
            Ok(())
        })
        .unwrap();
        assert_eq!(parts.into_inner(), whole.into_inner());
    }
}

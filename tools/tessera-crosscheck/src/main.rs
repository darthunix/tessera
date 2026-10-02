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
//! for a suite. Seeds that ever found something are kept in seeds.txt and
//! run first.
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
    after_help = "Exit: 0 no finding, 1 a finding (written under target/bench-runs/crosscheck), 2 an error."
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
        // The C collation: text compares by its bytes in both modes.
        initdb_args: vec!["--no-locale".into()],
        extensions: vec!["tessera".into()],
        stop_hint: "cargo run -p tessera-crosscheck -- stop".into(),
    };
    Cluster::new(pg, &runs_dir(), spec)
}

/// The counts of a seed's run.
#[derive(Default)]
struct Counts {
    queries: u32,
    with_tessera: u32,
    kinds: BTreeMap<&'static str, u32>,
}

/// A finding: the seed, the shrunk query and what its modes did.
struct Finding {
    seed: u64,
    query: Query,
    verdict: Verdict,
}

/// Runs the queries of one seed; a finding, shrunk, if any.
fn run_seed(
    oracle: &RefCell<Oracle<'_>>,
    seed: u64,
    queries: u32,
    args: &RunArgs,
    deadline: Option<Instant>,
) -> Result<(Counts, Option<Finding>)> {
    oracle.borrow_mut().load(&schema::setup(seed, args.rows))?;
    let config = Config {
        cases: queries,
        failure_persistence: None,
        // Each step of shrinking runs a query twice: a minute at most.
        max_shrink_iters: 10_000,
        max_shrink_time: 60_000,
        ..Config::default()
    };
    let mut seed_bytes = [0_u8; 32];
    seed_bytes[..8].copy_from_slice(&seed.to_le_bytes());
    let mut runner = TestRunner::new_with_rng(
        config,
        TestRng::from_seed(RngAlgorithm::ChaCha, &seed_bytes),
    );
    let counts = RefCell::new(Counts::default());
    // Once a query fails, the calls that follow shrink it: not counted.
    let shrinking = RefCell::new(false);
    let result = runner.run(&generate::query(), |query| {
        if deadline.is_some_and(|deadline| Instant::now() > deadline) && !*shrinking.borrow() {
            return Ok(());
        }
        let mut oracle = oracle.borrow_mut();
        let verdict = oracle
            .check(&query)
            .map_err(|error| TestCaseError::fail(format!("the server: {error:#}")))?;
        if !*shrinking.borrow() {
            let mut counts = counts.borrow_mut();
            counts.queries += 1;
            *counts.kinds.entry(verdict.kind()).or_default() += 1;
            if !verdict.is_finding() && oracle.uses_tessera(&query).unwrap_or(false) {
                counts.with_tessera += 1;
            }
        }
        if verdict.is_finding() {
            *shrinking.borrow_mut() = true;
            return Err(TestCaseError::fail(verdict.describe()));
        }
        Ok(())
    });
    let counts = counts.into_inner();
    match result {
        Ok(()) => Ok((counts, None)),
        Err(TestError::Fail(_, query)) => {
            let verdict = oracle.borrow_mut().check(&query)?;
            Ok((
                counts,
                Some(Finding {
                    seed,
                    query,
                    verdict,
                }),
            ))
        }
        Err(TestError::Abort(reason)) => anyhow::bail!("the run stopped: {reason}"),
    }
}

/// Writes a finding as a file a suite case can be made of: the data's
/// SQL, the settings and the query in both modes.
fn write_finding(finding: &Finding, args: &RunArgs) -> Result<PathBuf> {
    let directory = runs_dir().join("crosscheck");
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("finding-{}.sql", finding.seed));
    let sql = finding.query.sql();
    let text = format!(
        "-- tessera-crosscheck finding, seed {seed}, {rows} rows\n\
         -- {verdict}\n\n{setup}\n{settings}\n\
         SET tessera.enable = on;\n{sql};\nSET tessera.enable = off;\n{sql};\n",
        seed = finding.seed,
        rows = args.rows,
        verdict = finding.verdict.describe().replace('\n', "\n-- "),
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
    for (seed, queries) in seeds {
        let started = Instant::now();
        let (counts, finding) = run_seed(&oracle, seed, queries, &args, deadline)
            .with_context(|| format!("seed {seed}"))?;
        let kinds: Vec<String> = counts
            .kinds
            .iter()
            .map(|(kind, count)| format!("{count} {kind}"))
            .collect();
        println!(
            "seed {seed}: {} queries in {:.1} s ({}), {} with a Tessera node",
            counts.queries,
            started.elapsed().as_secs_f64(),
            kinds.join(", "),
            counts.with_tessera,
        );
        if let Some(finding) = finding {
            findings += 1;
            let path = write_finding(&finding, &args)?;
            println!(
                "FINDING, shrunk:\n{}\n{}\nwritten to {}",
                finding.query,
                finding.verdict.describe(),
                path.display()
            );
        }
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

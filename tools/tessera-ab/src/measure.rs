//! An A/B of two revisions on bench/pg families: each built in release
//! from its snapshot, installed in turn into the PostgreSQL build
//! `PG_CONFIG` names, and measured by `bench/pg/run.sh measure` of the
//! checkout the tool runs from, base and candidate alternating.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use tessera_pgtool::snapshot::{Snapshot, digest, text};
use tessera_pgtool::{Pg, install_tessera_with};

use crate::stats::{self, Verdict};

/// What to compare and how.
#[derive(clap::Args, Debug)]
pub struct Options {
    /// The base revision: a Git revision.
    #[arg(long, value_name = "REF")]
    pub base: String,
    /// The candidate revision: a Git revision or WORKTREE, the files as
    /// they are.
    #[arg(long, value_name = "REF", default_value = "WORKTREE")]
    pub candidate: String,
    /// A family of bench/pg (anyagg, join, sort, ...); several are measured
    /// one after the other.
    #[arg(long, value_name = "FAMILY", required = true)]
    pub family: Vec<String>,
    /// A regular expression of the case names to time (run.sh's CASES).
    #[arg(long, value_name = "REGEX")]
    pub cases: Option<String>,
    /// The runs of each case in each mode (run.sh's REPETITIONS); the
    /// family's own by default.
    #[arg(long, value_name = "N")]
    pub repetitions: Option<NonZeroUsize>,
    /// Parallel workers for both modes (run.sh's workers).
    #[arg(long, value_name = "N", default_value = "0")]
    pub workers: usize,
    /// Runs of each side per family, alternating base and candidate.
    #[arg(long, value_name = "N", default_value = "2")]
    pub rounds: NonZeroUsize,
    /// COPT for both builds, such as -falign-functions=64: does a change
    /// in time go with the code's place?
    #[arg(long, value_name = "FLAGS")]
    pub copt: Option<String>,
    /// The move, in percent, a case must make by its minimum and by its
    /// median beyond the control's to be flagged.
    #[arg(long, value_name = "PERCENT", default_value = "2")]
    pub threshold: f64,
}

/// The installed libraries a run must have taken from the side's build.
const LIBRARIES: [(&str, &str); 3] = [
    ("bridge", "tessera"),
    ("nodes", "tessera_nodes"),
    ("kernels", "tessera_kernels"),
];

pub fn run(repo: &Path, options: &Options) -> Result<u8> {
    ensure!(
        options.threshold > 0. && options.threshold < 100.,
        "--threshold is a percent between 0 and 100"
    );
    let pg = Pg::discover()?;
    let runs = repo.join("target/bench-runs");
    ensure!(
        runs.join("pgdata-bench/PG_VERSION").is_file(),
        "no bench cluster in {}: create it with `bench/pg/run.sh setup` (same PG_CONFIG) first",
        runs.display()
    );
    for family in &options.family {
        ensure!(
            repo.join("bench/pg")
                .join(format!("{family}.sql"))
                .is_file(),
            "bench/pg has no family {family}"
        );
    }
    let root = tempfile::Builder::new()
        .prefix("ab-")
        .tempdir_in(&runs)?
        .keep();
    println!("Artifacts: {}", root.display());
    let base = Snapshot::capture(repo, &options.base, root.join("base"))?;
    let candidate = if options.base == options.candidate {
        base.duplicate(root.join("candidate"))?
    } else {
        Snapshot::capture(repo, &options.candidate, root.join("candidate"))?
    };
    let variables: Vec<String> = options
        .copt
        .iter()
        .map(|copt| format!("COPT={copt}"))
        .collect();
    let sides = [("base", &base), ("candidate", &candidate)];
    let mut measured: BTreeMap<&str, [Vec<PathBuf>; 2]> = BTreeMap::new();
    for family in &options.family {
        for round in 1..=options.rounds.get() {
            for (index, (side, snapshot)) in sides.iter().enumerate() {
                println!("{family}: {side}, round {round} of {}", options.rounds);
                install_tessera_with(
                    &snapshot.directory,
                    &pg,
                    &root.join(format!("install-{side}.log")),
                    &variables,
                )?;
                let directory = measure(repo, &pg, family, options)?;
                check_installed(&directory, &snapshot.directory)?;
                fs::write(
                    directory.join("build.txt"),
                    format!("{side} {}\n", snapshot.revision),
                )?;
                println!("  {}", directory.display());
                measured.entry(family).or_default()[index].push(directory);
            }
        }
    }
    let report = report(&pg, &base, &candidate, options, &measured)?;
    fs::write(root.join("report.md"), &report.text)?;
    println!("\n{}", report.text);
    println!(
        "The candidate stays installed. Report: {}",
        root.join("report.md").display()
    );
    Ok(u8::from(report.slower))
}

/// One run of a family: run.sh's results directory.
fn measure(repo: &Path, pg: &Pg, family: &str, options: &Options) -> Result<PathBuf> {
    let mut command = Command::new(repo.join("bench/pg/run.sh"));
    command
        .current_dir(repo)
        .env("PG_CONFIG", &pg.pg_config)
        .args(["measure", family, &options.workers.to_string()]);
    if let Some(cases) = &options.cases {
        command.env("CASES", cases);
    }
    if let Some(repetitions) = options.repetitions {
        command.env("REPETITIONS", repetitions.to_string());
    }
    let output = command.output().context("cannot start bench/pg/run.sh")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let directory = stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("results: "))
        .map(PathBuf::from);
    match directory {
        Some(directory) if output.status.success() && directory.join("timings.csv").is_file() => {
            Ok(directory)
        }
        _ => bail!(
            "bench/pg/run.sh measure {family} failed:\n{}\n{}",
            stdout.lines().rev().take(10).collect::<Vec<_>>().join("\n"),
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

/// The run measured the side's own libraries: run.sh lists the installed
/// ones' SHA-256 in source.txt, which must be those the side built.
fn check_installed(run: &Path, build: &Path) -> Result<()> {
    let source = fs::read_to_string(run.join("source.txt"))?;
    for (directory, library) in LIBRARIES {
        let file = format!("{library}{}", Pg::dlsuffix());
        let built = digest(&fs::read(build.join(directory).join(&file))?);
        let installed = source
            .lines()
            .filter_map(|line| line.split_once("  "))
            .find(|(_, path)| path.ends_with(&format!("/{file}")))
            .map(|(hash, _)| hash)
            .with_context(|| format!("{} lists no {file}", run.join("source.txt").display()))?;
        ensure!(
            installed == built,
            "{} measured another {file} than the side built",
            run.display()
        );
    }
    Ok(())
}

struct Report {
    text: String,
    slower: bool,
}

fn report(
    pg: &Pg,
    base: &Snapshot,
    candidate: &Snapshot,
    options: &Options,
    measured: &BTreeMap<&str, [Vec<PathBuf>; 2]>,
) -> Result<Report> {
    let mut text = String::new();
    let threshold = options.threshold / 100.;
    let power = if cfg!(target_os = "macos") {
        let batt = tessera_pgtool::snapshot::output(Command::new("pmset").args(["-g", "batt"]))
            .map(|out| {
                String::from_utf8_lossy(&out)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_owned()
            })
            .unwrap_or_default();
        format!("; {}", batt.trim())
    } else {
        String::new()
    };
    writeln!(
        text,
        "# A/B: {} against {}\n",
        candidate.revision, base.revision
    )?;
    writeln!(
        text,
        "PostgreSQL {} ({}); {} round(s) a side, base and candidate alternating; cases {}; \
         repetitions {}; workers {}; COPT {}{power}.\n",
        pg.major(),
        pg.pg_config.display(),
        options.rounds,
        options.cases.as_deref().unwrap_or("all"),
        options
            .repetitions
            .map_or_else(|| "the family's".to_owned(), |n| n.to_string()),
        options.workers,
        options.copt.as_deref().unwrap_or("none"),
    )?;
    writeln!(
        text,
        "Each ratio is the candidate's time over the base's; below 1 is faster. `min` \
         compares the least time of any run, `median` the mean of the runs' medians, \
         `control` the medians of mode off, without Tessera, which move with the machine, \
         not with the change. A case is flagged when it moves past {}% by its minimum \
         and by its median beyond the control.\n",
        options.threshold
    )?;
    let mut slower = false;
    for (family, [base_runs, candidate_runs]) in measured {
        let read = |runs: &[PathBuf]| -> Result<Vec<stats::Timings>> {
            runs.iter()
                .map(|run| stats::parse(&fs::read_to_string(run.join("timings.csv"))?))
                .collect()
        };
        let rows = stats::compare(&read(base_runs)?, &read(candidate_runs)?, threshold);
        slower |= rows.iter().any(|row| row.verdict == Verdict::Slower);
        writeln!(text, "## {family}\n\n{}", stats::table(&rows))?;
        let names = |runs: &[PathBuf]| {
            runs.iter()
                .filter_map(|run| run.file_name()?.to_str().map(str::to_owned))
                .collect::<Vec<_>>()
                .join(", ")
        };
        writeln!(
            text,
            "Runs: base {}; candidate {}.\n",
            names(base_runs),
            names(candidate_runs)
        )?;
    }
    if slower {
        writeln!(
            text,
            "A flagged case is measured again with more repetitions (`--cases`, \
             `--repetitions 101`); if it stays, `--copt=-falign-functions=64` tells \
             whether it is the code's place."
        )?;
    }
    Ok(Report { text, slower })
}

/// The repository the tool runs in.
pub fn repository() -> Result<PathBuf> {
    Ok(PathBuf::from(text(
        Command::new("git").args(["rev-parse", "--show-toplevel"]),
    )?))
}

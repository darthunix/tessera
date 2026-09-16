//! Per-call counter statistics from raw block readings and the project limits.
//! No time is read anywhere; instructions decide, cycles warn.

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::Path,
};

/// Blocks written by the benchmark runner for every operation.
pub const BLOCKS: usize = 10;
/// Instructions per call may grow by at most this fraction.
const INSTRUCTION_LIMIT: f64 = 0.01;
/// Blocks of one process must agree on instructions per call this closely.
const INSTRUCTION_SPREAD_LIMIT: f64 = 0.001;
/// Cycles per call growing beyond this fraction produce a warning.
const CYCLE_WARNING: f64 = 0.03;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Unstable,
}

impl Status {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::Fail => 1,
            Self::Unstable => 2,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Unstable => "UNSTABLE",
        }
    }
    pub fn combine(self, other: Self) -> Self {
        if self == Self::Fail || other == Self::Fail {
            Self::Fail
        } else if self == Self::Unstable || other == Self::Unstable {
            Self::Unstable
        } else {
            Self::Pass
        }
    }
}

#[derive(Deserialize)]
struct Record {
    id: String,
    iters: u64,
    instructions: Vec<u64>,
    cycles: Vec<u64>,
    branch_misses: Vec<u64>,
    branches: Vec<u64>,
}

/// Per-call statistics of one operation: medians over blocks, and the
/// block-to-block spread of instructions as a consistency check.
#[derive(Clone, Copy, Debug)]
pub struct Counts {
    pub instructions: f64,
    pub instructions_spread: f64,
    pub cycles: f64,
    pub cycles_min: f64,
    pub cycles_max: f64,
    pub branch_misses: f64,
    pub branches: f64,
}

impl Counts {
    pub fn consistent(&self) -> bool {
        self.instructions_spread <= INSTRUCTION_SPREAD_LIMIT
    }
    fn status(self, old: Self) -> Status {
        if !self.consistent() || !old.consistent() {
            Status::Unstable
        } else if self.instructions > old.instructions * (1. + INSTRUCTION_LIMIT) {
            Status::Fail
        } else {
            Status::Pass
        }
    }
    fn cycles_warning(self, old: Self) -> bool {
        self.cycles > old.cycles * (1. + CYCLE_WARNING)
    }
}

pub struct Entry {
    pub group: String,
    pub path: String,
    pub counts: Counts,
}
pub type Run = BTreeMap<String, Entry>;

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.
    }
}

fn per_call(values: &[u64], iters: u64, what: &str, id: &str) -> Result<Vec<f64>> {
    ensure!(
        values.len() == BLOCKS,
        "{id}: expected {BLOCKS} {what} blocks, got {}",
        values.len()
    );
    Ok(values.iter().map(|&v| v as f64 / iters as f64).collect())
}

fn counts(record: &Record) -> Result<Counts> {
    ensure!(record.iters > 0, "{}: no calls per block", record.id);
    let instructions = per_call(
        &record.instructions,
        record.iters,
        "instruction",
        &record.id,
    )?;
    let cycles = per_call(&record.cycles, record.iters, "cycle", &record.id)?;
    let branch_misses = per_call(
        &record.branch_misses,
        record.iters,
        "branch miss",
        &record.id,
    )?;
    let branches = per_call(&record.branches, record.iters, "branch", &record.id)?;
    let instructions_median = median(&instructions);
    ensure!(
        instructions_median > 0. && median(&cycles) > 0.,
        "{}: zero instructions or cycles",
        record.id
    );
    let (min, max) = instructions
        .iter()
        .fold((f64::INFINITY, 0_f64), |(min, max), &v| {
            (min.min(v), max.max(v))
        });
    Ok(Counts {
        instructions: instructions_median,
        instructions_spread: (max - min) / instructions_median,
        cycles: median(&cycles),
        cycles_min: cycles.iter().copied().fold(f64::INFINITY, f64::min),
        cycles_max: cycles.iter().copied().fold(0., f64::max),
        branch_misses: median(&branch_misses),
        branches: median(&branches),
    })
}

/// Operation ids printed by `--list`, one per line. Filters select cases, not
/// an isolated path without its scalar reference.
pub fn listed(text: &str) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        ensure!(
            line.contains('/') && names.insert(line.to_owned()),
            "duplicate or invalid benchmark name: {line}"
        );
    }
    ensure!(!names.is_empty(), "no benchmark cases matched");
    for name in &names {
        let (group, path) = name.rsplit_once('/').context("invalid benchmark name")?;
        ensure!(
            names.contains(&format!("{group}/reference")),
            "filter must include the reference for {group}"
        );
        ensure!(
            path != "reference"
                || names.iter().any(|other| other != name
                    && other.rsplit_once('/').is_some_and(|(g, _)| g == group)),
            "filter selected only a reference: {group}"
        );
    }
    Ok(names)
}

/// Read one process's JSONL output and check that it covers exactly `expected`.
pub fn load(path: &Path, expected: &BTreeSet<String>) -> Result<Run> {
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut run = Run::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let record: Record = serde_json::from_str(line)
            .with_context(|| format!("invalid record in {}", path.display()))?;
        ensure!(
            expected.contains(&record.id),
            "unexpected benchmark result: {}",
            record.id
        );
        let (group, function) = record.id.rsplit_once('/').context("invalid benchmark id")?;
        ensure!(
            matches!(function, "fold" | "words" | "scalar" | "reference"),
            "unsupported measured path: {}",
            record.id
        );
        let entry = Entry {
            group: group.to_owned(),
            path: function.to_owned(),
            counts: counts(&record)?,
        };
        ensure!(
            run.insert(record.id.clone(), entry).is_none(),
            "duplicate benchmark result: {}",
            record.id
        );
    }
    ensure!(
        run.keys().eq(expected.iter()),
        "missing benchmark results: {:?}",
        expected
            .difference(&run.keys().cloned().collect())
            .collect::<Vec<_>>()
    );
    Ok(run)
}

fn change(new: f64, old: f64) -> f64 {
    (new / old - 1.) * 100.
}

/// Runs are before and after. Instructions per call decide the status; cycles
/// only warn, and references are informational.
pub fn print(out: &mut impl Write, runs: &[Run; 2]) -> Result<Status> {
    let names: Vec<_> = runs[0].keys().collect();
    ensure!(
        !names.is_empty() && runs[1].keys().collect::<Vec<_>>() == names,
        "run case sets differ"
    );
    writeln!(
        out,
        "Per call, medians over {BLOCKS} blocks. FAIL: instructions +{:.0}%; WARNING: cycles +{:.0}%; UNSTABLE: blocks disagree on instructions beyond {:.1}%.",
        INSTRUCTION_LIMIT * 100.,
        CYCLE_WARNING * 100.,
        INSTRUCTION_SPREAD_LIMIT * 100.
    )?;
    let mut status = Status::Pass;
    let mut counts = [0; 3];
    let mut warnings = 0;
    for name in names {
        let old = &runs[0][name];
        let new = &runs[1][name];
        ensure!(
            old.group == new.group && old.path == new.path,
            "identity changed between runs"
        );
        if old.path == "reference" {
            continue;
        }
        let reference = format!("{}/reference", old.group);
        let old_reference = runs[0].get(&reference).context("missing reference")?.counts;
        let new_reference = runs[1].get(&reference).context("missing reference")?.counts;
        let (o, n) = (old.counts, new.counts);
        let outcome = n.status(o);
        status = status.combine(outcome);
        counts[outcome.exit_code() as usize] += 1;
        writeln!(
            out,
            "{} {name}: instructions before={:.1} after={:.1} change={:+.2}%; cycles before={:.1} after={:.1} change={:+.2}% [{:.1}..{:.1}]; branch misses before={:.3} after={:.3} of {:.1} branches",
            outcome.label(),
            o.instructions,
            n.instructions,
            change(n.instructions, o.instructions),
            o.cycles,
            n.cycles,
            change(n.cycles, o.cycles),
            n.cycles_min,
            n.cycles_max,
            o.branch_misses,
            n.branch_misses,
            n.branches,
        )?;
        writeln!(
            out,
            "  REFERENCE instructions before={:.1} after={:.1}; library/reference={:.3}x, cycles {:.3}x (informational)",
            old_reference.instructions,
            new_reference.instructions,
            n.instructions / new_reference.instructions,
            n.cycles / new_reference.cycles,
        )?;
        for (label, c) in [("before", o), ("after", n)] {
            if !c.consistent() {
                writeln!(
                    out,
                    "  UNSTABLE {label}: blocks disagree on instructions per call by {:.3}%",
                    c.instructions_spread * 100.
                )?;
            }
        }
        if n.cycles_warning(o) {
            warnings += 1;
            writeln!(
                out,
                "  WARNING cycles: +{:.2}% with instructions {:+.2}% and branch misses {:+.3} per call; review layout, predictor or dependency chains",
                change(n.cycles, o.cycles),
                change(n.instructions, o.instructions),
                n.branch_misses - o.branch_misses,
            )?;
        }
    }
    ensure!(
        counts.iter().sum::<usize>() != 0,
        "no library paths measured"
    );
    writeln!(
        out,
        "{}: {} PASS, {} FAIL, {} UNSTABLE library paths; {warnings} cycle warnings",
        status.label(),
        counts[0],
        counts[1],
        counts[2]
    )?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact(instructions: f64, cycles: f64) -> Counts {
        Counts {
            instructions,
            instructions_spread: 0.,
            cycles,
            cycles_min: cycles,
            cycles_max: cycles,
            branch_misses: 0.,
            branches: 0.,
        }
    }

    fn record(id: &str, iters: u64, values: [Vec<u64>; 4]) -> String {
        let [instructions, cycles, branch_misses, branches] = values;
        serde_json::json!({
            "id": id, "iters": iters, "instructions": instructions, "cycles": cycles,
            "branch_misses": branch_misses, "branches": branches,
        })
        .to_string()
    }

    #[test]
    fn instructions_decide_and_the_limit_is_inclusive() {
        assert_eq!(exact(101., 100.).status(exact(100., 100.)), Status::Pass);
        assert_eq!(exact(101.01, 100.).status(exact(100., 100.)), Status::Fail);
        assert_eq!(exact(50., 1000.).status(exact(100., 100.)), Status::Pass);
        let mut inconsistent = exact(100., 100.);
        inconsistent.instructions_spread = 0.0011;
        assert_eq!(inconsistent.status(exact(100., 100.)), Status::Unstable);
        assert_eq!(exact(100., 100.).status(inconsistent), Status::Unstable);
        assert_eq!(Status::Fail.combine(Status::Unstable).exit_code(), 1);
    }

    #[test]
    fn cycles_only_warn() {
        assert!(exact(100., 103.01).cycles_warning(exact(100., 100.)));
        assert!(!exact(100., 103.).cycles_warning(exact(100., 100.)));
        assert_eq!(exact(100., 200.).status(exact(100., 100.)), Status::Pass);
    }

    #[test]
    fn listing_requires_references_and_library_paths() {
        assert!(listed("a/x/fold\na/x/reference\n").is_ok());
        assert!(listed("a/x/fold\n").is_err());
        assert!(listed("a/x/reference\n").is_err());
        assert!(listed("a/x/fold\na/x/fold\na/x/reference\n").is_err());
        assert!(listed("\n").is_err());
        assert!(listed("noslash\n").is_err());
    }

    #[test]
    fn records_become_per_call_medians_with_spread() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("results.jsonl");
        let mut cycles = vec![2000; BLOCKS];
        cycles[0] = 4000;
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                record(
                    "b/x/fold",
                    100,
                    [
                        vec![1000; BLOCKS],
                        cycles,
                        vec![10; BLOCKS],
                        vec![300; BLOCKS]
                    ]
                ),
                record(
                    "b/x/reference",
                    100,
                    [
                        vec![500; BLOCKS],
                        vec![1000; BLOCKS],
                        vec![0; BLOCKS],
                        vec![100; BLOCKS]
                    ]
                ),
            ),
        )?;
        let expected = BTreeSet::from(["b/x/fold".to_owned(), "b/x/reference".to_owned()]);
        let run = load(&path, &expected)?;
        let fold = run["b/x/fold"].counts;
        assert_eq!(fold.instructions, 10.);
        assert_eq!(fold.instructions_spread, 0.);
        assert_eq!(fold.cycles, 20.);
        assert_eq!((fold.cycles_min, fold.cycles_max), (20., 40.));
        assert_eq!(fold.branch_misses, 0.1);
        assert_eq!(fold.branches, 3.);
        assert!(load(&path, &BTreeSet::from(["b/x/fold".to_owned()])).is_err());
        let mut output = Vec::new();
        let other = load(&path, &expected)?;
        assert_eq!(print(&mut output, &[run, other])?, Status::Pass);
        let text = String::from_utf8(output)?;
        assert!(text.contains("PASS b/x/fold: instructions before=10.0 after=10.0 change=+0.00%"));
        assert!(text.contains("library/reference=2.000x"));
        assert!(
            text.ends_with("PASS: 1 PASS, 0 FAIL, 0 UNSTABLE library paths; 0 cycle warnings\n")
        );
        Ok(())
    }

    #[test]
    fn inconsistent_blocks_and_short_records_are_rejected() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("results.jsonl");
        let only = BTreeSet::from(["b/x/fold".to_owned()]);
        fs::write(
            &path,
            record(
                "b/x/fold",
                1,
                [
                    vec![1; BLOCKS - 1],
                    vec![1; BLOCKS],
                    vec![0; BLOCKS],
                    vec![0; BLOCKS],
                ],
            ),
        )?;
        assert!(load(&path, &only).is_err());
        let mut drift = vec![1000; BLOCKS];
        drift[BLOCKS - 1] = 1002;
        fs::write(
            &path,
            record(
                "b/x/fold",
                1,
                [drift, vec![1; BLOCKS], vec![0; BLOCKS], vec![0; BLOCKS]],
            ),
        )?;
        let run = load(&path, &only)?;
        assert!(!run["b/x/fold"].counts.consistent());
        Ok(())
    }
}

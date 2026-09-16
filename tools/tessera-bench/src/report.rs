//! Per-call counter statistics from the raw block readings of one or more
//! processes per side, and the project limits. No time is read anywhere:
//! instructions decide, cycles warn and fail only for clear slowdowns of
//! long operations that run in a single mode on both sides.

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
pub const INSTRUCTION_LIMIT: f64 = 0.01;
/// Blocks of one process, and processes of one side, must agree on
/// instructions per call this closely.
pub const INSTRUCTION_SPREAD_LIMIT: f64 = 0.001;
/// Minimum cycles per call growing beyond this fraction warn, for operations
/// of at least [`SHORT_CYCLES`] cycles per call, when the median grows
/// beyond it too: a real slowdown moves the whole distribution, while the
/// minimum alone is one lucky block, and identical binaries have shown the
/// minimum 3-17% apart with medians within 1.5%.
pub const CYCLE_WARNING: f64 = 0.03;
/// Shorter operations warn on absolute growth instead: a percentage is a
/// fraction of a cycle there.
pub const SHORT_CYCLES: f64 = 200.;
pub const SHORT_CYCLE_WARNING: f64 = 4.;
/// Minimum cycles per call growing beyond this fraction fail, for operations
/// of at least [`CYCLE_FAIL_CYCLES`], when the median confirms the growth
/// beyond [`CYCLE_WARNING`]. The minimum over all blocks of a side is the
/// cost in the best core state seen; with three processes it stayed within
/// 3% between identical binaries on long operations, bistable ones included.
pub const CYCLE_FAIL: f64 = 0.10;
pub const CYCLE_FAIL_CYCLES: f64 = 500.;
/// An operation is bistable when the medians of its processes, or the blocks
/// of one process, differ by more than this ratio and by more than
/// [`MODES_CYCLES`] cycles per call. This is reported, not judged: it is a
/// property of the code, and the minimum is still compared.
pub const MODES_LIMIT: f64 = 1.10;
/// Below this spread the modes are a few cycles on a short operation, not a
/// second cost worth fixing.
pub const MODES_CYCLES: f64 = 10.;

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
    #[serde(default)]
    cpus: Vec<[usize; 2]>,
    #[serde(default)]
    retries: u32,
}

/// One process's per-call statistics of one operation.
#[derive(Clone, Debug)]
pub struct Process {
    pub instructions: f64,
    pub instructions_spread: f64,
    pub cycles: Vec<f64>,
    pub branch_misses: f64,
    pub branches: f64,
    pub cpus: BTreeSet<usize>,
    pub retries: u32,
    pub zero_blocks: usize,
}

#[derive(Clone)]
pub struct Entry {
    pub group: String,
    pub path: String,
    pub process: Process,
}
pub type Run = BTreeMap<String, Entry>;

/// One side's statistics over all of its processes.
#[derive(Clone, Debug)]
pub struct Side {
    pub processes: usize,
    pub instructions: f64,
    pub instructions_spread: f64,
    pub cycles_min: f64,
    pub cycles_median: f64,
    pub modes: f64,
    pub branch_misses: f64,
    pub branches: f64,
    pub cpus: BTreeSet<usize>,
    pub retries: u32,
    pub zero_blocks: usize,
}

impl Side {
    pub fn consistent(&self) -> bool {
        self.zero_blocks == 0 && self.instructions_spread <= INSTRUCTION_SPREAD_LIMIT
    }
    pub fn bistable(&self) -> bool {
        self.modes > MODES_LIMIT && (self.modes - 1.) * self.cycles_min > MODES_CYCLES
    }
}

pub struct SideEntry {
    pub group: String,
    pub path: String,
    pub side: Side,
}
pub type Aggregate = BTreeMap<String, SideEntry>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    FailInstructions,
    FailCycles,
    Unstable,
}

impl Verdict {
    pub fn status(self) -> Status {
        match self {
            Self::Pass => Status::Pass,
            Self::FailInstructions | Self::FailCycles => Status::Fail,
            Self::Unstable => Status::Unstable,
        }
    }
}

pub fn verdict(new: &Side, old: &Side) -> Verdict {
    if !new.consistent() || !old.consistent() {
        Verdict::Unstable
    } else if new.instructions > old.instructions * (1. + INSTRUCTION_LIMIT) {
        Verdict::FailInstructions
    } else if old.cycles_min >= CYCLE_FAIL_CYCLES
        && new.cycles_min > old.cycles_min * (1. + CYCLE_FAIL)
        && grew(new.cycles_median, old.cycles_median, old)
    {
        Verdict::FailCycles
    } else {
        Verdict::Pass
    }
}

/// A statistic grew beyond the warning threshold of an operation as short as
/// `old`'s minimum.
fn grew(new: f64, old: f64, side: &Side) -> bool {
    if side.cycles_min < SHORT_CYCLES {
        new - old > SHORT_CYCLE_WARNING
    } else {
        new > old * (1. + CYCLE_WARNING)
    }
}

/// The minimum and the median both grew beyond the warning threshold.
pub fn cycles_warning(new: &Side, old: &Side) -> bool {
    grew(new.cycles_min, old.cycles_min, old) && grew(new.cycles_median, old.cycles_median, old)
}

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

fn spread(values: &[f64]) -> f64 {
    let (min, max) = values
        .iter()
        .fold((f64::INFINITY, 0_f64), |(min, max), &v| {
            (min.min(v), max.max(v))
        });
    let mid = median(values);
    if mid > 0. { (max - min) / mid } else { 0. }
}

fn ratio(values: &[f64]) -> f64 {
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(0., f64::max);
    if min > 0. { max / min } else { f64::INFINITY }
}

fn per_call(values: &[u64], iters: u64, what: &str, id: &str) -> Result<Vec<f64>> {
    ensure!(
        values.len() == BLOCKS,
        "{id}: expected {BLOCKS} {what} blocks, got {}",
        values.len()
    );
    Ok(values.iter().map(|&v| v as f64 / iters as f64).collect())
}

fn process(record: &Record) -> Result<Process> {
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
    let zero_blocks = instructions
        .iter()
        .zip(&cycles)
        .filter(|(i, c)| **i == 0. || **c == 0.)
        .count();
    Ok(Process {
        instructions: median(&instructions),
        instructions_spread: spread(&instructions),
        cycles,
        branch_misses: median(&branch_misses),
        branches: median(&branches),
        cpus: record.cpus.iter().flatten().copied().collect(),
        retries: record.retries,
        zero_blocks,
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
        // Any operation name is a library path; `reference` is the reference.
        let (group, function) = record.id.rsplit_once('/').context("invalid benchmark id")?;
        let entry = Entry {
            group: group.to_owned(),
            path: function.to_owned(),
            process: process(&record)?,
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

/// Combine the processes of one side: instructions must agree across them,
/// cycles are compared by their minimum, and disagreeing process medians or
/// blocks mark the operation as bistable.
pub fn aggregate(runs: &[Run]) -> Result<Aggregate> {
    ensure!(!runs.is_empty(), "no processes measured");
    let names: Vec<_> = runs[0].keys().collect();
    ensure!(
        runs.iter()
            .all(|run| run.keys().collect::<Vec<_>>() == names),
        "process case sets differ"
    );
    let mut aggregate = Aggregate::new();
    for name in names {
        let entries: Vec<_> = runs.iter().map(|run| &run[name]).collect();
        let processes: Vec<_> = entries.iter().map(|entry| &entry.process).collect();
        ensure!(
            entries
                .iter()
                .all(|entry| entry.group == entries[0].group && entry.path == entries[0].path),
            "identity changed between processes"
        );
        let instructions: Vec<_> = processes.iter().map(|p| p.instructions).collect();
        let all_cycles: Vec<_> = processes
            .iter()
            .flat_map(|p| p.cycles.iter().copied())
            .collect();
        let process_medians: Vec<_> = processes.iter().map(|p| median(&p.cycles)).collect();
        let within = processes
            .iter()
            .map(|p| ratio(&p.cycles))
            .fold(0., f64::max);
        let side = Side {
            processes: processes.len(),
            instructions: median(&instructions),
            instructions_spread: processes
                .iter()
                .map(|p| p.instructions_spread)
                .fold(spread(&instructions), f64::max),
            cycles_min: all_cycles.iter().copied().fold(f64::INFINITY, f64::min),
            cycles_median: median(&all_cycles),
            modes: ratio(&process_medians).max(within),
            branch_misses: median(
                &processes
                    .iter()
                    .map(|p| p.branch_misses)
                    .collect::<Vec<_>>(),
            ),
            branches: median(&processes.iter().map(|p| p.branches).collect::<Vec<_>>()),
            cpus: processes
                .iter()
                .flat_map(|p| p.cpus.iter().copied())
                .collect(),
            retries: processes.iter().map(|p| p.retries).sum(),
            zero_blocks: processes.iter().map(|p| p.zero_blocks).sum(),
        };
        aggregate.insert(
            name.clone(),
            SideEntry {
                group: entries[0].group.clone(),
                path: entries[0].path.clone(),
                side,
            },
        );
    }
    Ok(aggregate)
}

fn change(new: f64, old: f64) -> f64 {
    (new / old - 1.) * 100.
}

/// Rows of the case: its first all-digit path segment, when there is one.
fn rows(group: &str) -> Option<f64> {
    group
        .split('/')
        .find(|segment| !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|segment| segment.parse().ok())
        .filter(|&rows: &f64| rows > 0.)
}

fn cores(cpus: &BTreeSet<usize>) -> String {
    cpus.iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Sides are before and after. Instructions decide; cycles fail only for
/// long single-mode operations and warn otherwise; references inform.
pub fn print(out: &mut impl Write, sides: &[Aggregate; 2]) -> Result<Status> {
    let names: Vec<_> = sides[0].keys().collect();
    ensure!(
        !names.is_empty() && sides[1].keys().collect::<Vec<_>>() == names,
        "run case sets differ"
    );
    writeln!(
        out,
        "Per call over {BLOCKS} blocks per process. FAIL: instructions +{:.0}%, or minimum cycles +{:.0}% on operations of at least {:.0} cycles. WARNING: minimum cycles +{:.0}% (or +{:.0} cycles below {:.0}). Cycle verdicts need the median to grow past the warning threshold too. MODES: process medians or blocks differ by more than {:.2}x and {:.0} cycles. UNSTABLE: zero readings or instructions disagreeing beyond {:.1}%.",
        INSTRUCTION_LIMIT * 100.,
        CYCLE_FAIL * 100.,
        CYCLE_FAIL_CYCLES,
        CYCLE_WARNING * 100.,
        SHORT_CYCLE_WARNING,
        SHORT_CYCLES,
        MODES_LIMIT,
        MODES_CYCLES,
        INSTRUCTION_SPREAD_LIMIT * 100.
    )?;
    let mut status = Status::Pass;
    let mut counts = [0; 3];
    let (mut instruction_fails, mut cycle_fails, mut warnings, mut bistable) = (0, 0, 0, 0);
    for name in names {
        let old = &sides[0][name];
        let new = &sides[1][name];
        ensure!(
            old.group == new.group && old.path == new.path,
            "identity changed between sides"
        );
        if old.path == "reference" {
            continue;
        }
        let reference = format!("{}/reference", old.group);
        let old_reference = &sides[0].get(&reference).context("missing reference")?.side;
        let new_reference = &sides[1].get(&reference).context("missing reference")?.side;
        let (o, n) = (&old.side, &new.side);
        let outcome = verdict(n, o);
        status = status.combine(outcome.status());
        counts[outcome.status().exit_code() as usize] += 1;
        match outcome {
            Verdict::FailInstructions => instruction_fails += 1,
            Verdict::FailCycles => cycle_fails += 1,
            _ => {}
        }
        writeln!(
            out,
            "{} {name}: instructions before={:.1} after={:.1} change={:+.2}%; cycles min before={:.1} after={:.1} change={:+.2}% (median {:.1}/{:.1}, modes {:.2}/{:.2}, {} processes); branch misses before={:.3} after={:.3} of {:.1} branches; cores {}",
            outcome.status().label(),
            o.instructions,
            n.instructions,
            change(n.instructions, o.instructions),
            o.cycles_min,
            n.cycles_min,
            change(n.cycles_min, o.cycles_min),
            o.cycles_median,
            n.cycles_median,
            o.modes,
            n.modes,
            n.processes,
            o.branch_misses,
            n.branch_misses,
            n.branches,
            cores(&n.cpus),
        )?;
        writeln!(
            out,
            "  REFERENCE instructions before={:.1} after={:.1}; library/reference={:.3}x, cycles {:.3}x (informational)",
            old_reference.instructions,
            new_reference.instructions,
            n.instructions / new_reference.instructions,
            n.cycles_min / new_reference.cycles_min,
        )?;
        // Rates for judging the code itself: cost per row and instructions
        // per cycle in the best mode. Informational.
        match rows(&old.group) {
            Some(rows) => writeln!(
                out,
                "  PER ROW ({rows:.0} rows): instructions before={:.2} after={:.2}; cycles min before={:.2} after={:.2}; IPC before={:.2} after={:.2}",
                o.instructions / rows,
                n.instructions / rows,
                o.cycles_min / rows,
                n.cycles_min / rows,
                o.instructions / o.cycles_min,
                n.instructions / n.cycles_min,
            )?,
            None => writeln!(
                out,
                "  IPC before={:.2} after={:.2}",
                o.instructions / o.cycles_min,
                n.instructions / n.cycles_min,
            )?,
        }
        for (label, side) in [("before", o), ("after", n)] {
            if side.zero_blocks > 0 {
                writeln!(
                    out,
                    "  UNSTABLE {label}: {} zero counter readings after {} repeats",
                    side.zero_blocks, side.retries
                )?;
            } else if side.instructions_spread > INSTRUCTION_SPREAD_LIMIT {
                writeln!(
                    out,
                    "  UNSTABLE {label}: blocks or processes disagree on instructions per call by {:.3}%",
                    side.instructions_spread * 100.
                )?;
            }
        }
        if outcome == Verdict::FailCycles {
            writeln!(
                out,
                "  FAIL cycles: minimum +{:.2}% and median +{:.2}% on an operation of {:.0} cycles; instructions {:+.2}%, branch misses {:+.3} per call",
                change(n.cycles_min, o.cycles_min),
                change(n.cycles_median, o.cycles_median),
                o.cycles_min,
                change(n.instructions, o.instructions),
                n.branch_misses - o.branch_misses,
            )?;
        } else if cycles_warning(n, o) {
            warnings += 1;
            writeln!(
                out,
                "  WARNING cycles: minimum {:+.2}% ({:+.1} cycles) and median {:+.2}% with instructions {:+.2}% and branch misses {:+.3} per call; review layout, predictor or dependency chains",
                change(n.cycles_min, o.cycles_min),
                n.cycles_min - o.cycles_min,
                change(n.cycles_median, o.cycles_median),
                change(n.instructions, o.instructions),
                n.branch_misses - o.branch_misses,
            )?;
        }
        // A zero reading makes the modes ratio infinite; that is UNSTABLE, not modes.
        if outcome != Verdict::Unstable && (o.bistable() || n.bistable()) {
            bistable += 1;
            writeln!(
                out,
                "  MODES before={:.2}x after={:.2}x: processes or blocks run in different modes; the minimum above is the best mode seen",
                o.modes, n.modes
            )?;
        }
        if n.instructions < o.instructions && cycles_warning(n, o) {
            writeln!(
                out,
                "  SLOWER-WITH-FEWER-INSTRUCTIONS: a longer dependency chain or worse prediction outweighs the saved instructions"
            )?;
        }
    }
    ensure!(
        counts.iter().sum::<usize>() != 0,
        "no library paths measured"
    );
    writeln!(
        out,
        "{}: {} PASS, {} FAIL ({instruction_fails} instructions, {cycle_fails} cycles), {} UNSTABLE library paths; {warnings} cycle warnings; {bistable} bistable",
        status.label(),
        counts[0],
        counts[1],
        counts[2],
    )?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(instructions: f64, cycles_min: f64) -> Side {
        Side {
            processes: 1,
            instructions,
            instructions_spread: 0.,
            cycles_min,
            cycles_median: cycles_min,
            modes: 1.,
            branch_misses: 0.,
            branches: 0.,
            cpus: BTreeSet::new(),
            retries: 0,
            zero_blocks: 0,
        }
    }

    fn record(id: &str, iters: u64, values: [Vec<u64>; 4], cpus: Vec<[usize; 2]>) -> String {
        let [instructions, cycles, branch_misses, branches] = values;
        serde_json::json!({
            "id": id, "iters": iters, "instructions": instructions, "cycles": cycles,
            "branch_misses": branch_misses, "branches": branches, "cpus": cpus, "retries": 1,
        })
        .to_string()
    }

    #[test]
    fn instructions_decide_first_and_the_limit_is_inclusive() {
        assert_eq!(verdict(&side(101., 100.), &side(100., 100.)), Verdict::Pass);
        assert_eq!(
            verdict(&side(101.01, 100.), &side(100., 100.)),
            Verdict::FailInstructions
        );
        // An instruction regression is reported as such even with a cycle regression.
        assert_eq!(
            verdict(&side(102., 2000.), &side(100., 1000.)),
            Verdict::FailInstructions
        );
        let mut inconsistent = side(100., 100.);
        inconsistent.instructions_spread = 0.0011;
        assert_eq!(verdict(&inconsistent, &side(100., 100.)), Verdict::Unstable);
        assert_eq!(verdict(&side(100., 100.), &inconsistent), Verdict::Unstable);
        let mut zero = side(100., 100.);
        zero.zero_blocks = 1;
        assert_eq!(verdict(&side(100., 100.), &zero), Verdict::Unstable);
        assert_eq!(Verdict::FailCycles.status().exit_code(), 1);
    }

    #[test]
    fn cycles_fail_only_long_operations_and_bistability_is_reported_not_judged() {
        assert_eq!(
            verdict(&side(100., 1100.), &side(100., 1000.)),
            Verdict::Pass
        );
        assert_eq!(
            verdict(&side(100., 1100.01), &side(100., 1000.)),
            Verdict::FailCycles
        );
        assert_eq!(
            verdict(&side(50., 1100.01), &side(100., 1000.)),
            Verdict::FailCycles
        );
        assert_eq!(verdict(&side(100., 600.), &side(100., 499.)), Verdict::Pass);
        let mut bistable = side(100., 1000.);
        bistable.modes = 1.11;
        assert!(bistable.bistable());
        assert_eq!(verdict(&side(100., 1200.), &bistable), Verdict::FailCycles);
        // A few cycles between modes of a short operation are not modes.
        let mut short = side(100., 25.);
        short.modes = 1.20;
        assert!(!short.bistable());
        short.cycles_min = 101.;
        assert!(short.bistable());
    }

    #[test]
    fn cycle_verdicts_need_the_median_to_confirm_the_minimum() {
        // One lucky block on the old side: the minimum moves, the median does not.
        let mut lucky = side(100., 449.);
        lucky.cycles_median = 480.;
        let mut later = side(100., 482.);
        later.cycles_median = 486.;
        assert!(!cycles_warning(&later, &lucky));
        let mut old = side(100., 1000.);
        old.cycles_median = 1200.;
        let mut new = side(100., 1101.);
        new.cycles_median = 1230.;
        assert_eq!(verdict(&new, &old), Verdict::Pass);
        new.cycles_median = 1236.01;
        assert_eq!(verdict(&new, &old), Verdict::FailCycles);
        // Short operations confirm by absolute growth as well.
        let mut short_old = side(100., 90.);
        short_old.cycles_median = 120.;
        let mut short_new = side(100., 95.);
        short_new.cycles_median = 124.;
        assert!(!cycles_warning(&short_new, &short_old));
        short_new.cycles_median = 124.01;
        assert!(cycles_warning(&short_new, &short_old));
    }

    #[test]
    fn cycle_warnings_are_relative_for_long_and_absolute_for_short_operations() {
        assert!(cycles_warning(&side(100., 1030.01), &side(100., 1000.)));
        assert!(!cycles_warning(&side(100., 1030.), &side(100., 1000.)));
        assert!(cycles_warning(&side(100., 30.01), &side(100., 26.)));
        assert!(!cycles_warning(&side(100., 30.), &side(100., 26.)));
        assert!(!cycles_warning(&side(100., 27.), &side(100., 26.)));
    }

    #[test]
    fn rows_come_from_the_first_numeric_segment() {
        assert_eq!(
            rows("column_reader/dense/words/1024/all/nulls-mixed/ready"),
            Some(1024.)
        );
        assert_eq!(
            rows("filter_int32/datum/bytes-7/65/all/nulls-mixed/partial"),
            Some(65.)
        );
        assert_eq!(
            rows("column_reader/dense/words/0/all/nulls-mixed/ready"),
            None
        );
        assert_eq!(rows("b/x"), None);
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

    fn write_run(dir: &Path, name: &str, fold_cycles: Vec<u64>) -> Result<Run> {
        let path = dir.join(name);
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                record(
                    "b/x/fold",
                    100,
                    [
                        vec![1000; BLOCKS],
                        fold_cycles,
                        vec![10; BLOCKS],
                        vec![300; BLOCKS]
                    ],
                    vec![[12, 12]; BLOCKS],
                ),
                record(
                    "b/x/reference",
                    100,
                    [
                        vec![500; BLOCKS],
                        vec![1000; BLOCKS],
                        vec![0; BLOCKS],
                        vec![100; BLOCKS]
                    ],
                    vec![[15, 16]; BLOCKS],
                ),
            ),
        )?;
        load(
            &path,
            &BTreeSet::from(["b/x/fold".to_owned(), "b/x/reference".to_owned()]),
        )
    }

    #[test]
    fn processes_aggregate_into_minimum_cycles_and_modes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut first = vec![2000; BLOCKS];
        first[0] = 4000;
        let one = write_run(dir.path(), "one.jsonl", first)?;
        let two = write_run(dir.path(), "two.jsonl", vec![2600; BLOCKS])?;
        assert_eq!(one["b/x/fold"].process.cpus, BTreeSet::from([12]));
        assert_eq!(one["b/x/fold"].process.retries, 1);
        let sides = aggregate(&[one, two])?;
        let fold = &sides["b/x/fold"].side;
        assert_eq!(fold.processes, 2);
        assert_eq!(fold.instructions, 10.);
        assert_eq!(fold.instructions_spread, 0.);
        assert_eq!(fold.cycles_min, 20.);
        // Nine blocks of 20, ten of 26 and one of 40: the median is 26.
        assert_eq!(fold.cycles_median, 26.);
        // Process medians 20 and 26 differ by 1.3x; blocks of the first by 2x.
        assert_eq!(fold.modes, 2.);
        assert!(fold.bistable());
        assert_eq!(fold.branch_misses, 0.1);
        assert_eq!(fold.retries, 2);
        assert_eq!(fold.cpus, BTreeSet::from([12]));
        let reference = &sides["b/x/reference"].side;
        assert_eq!(reference.modes, 1.);
        assert_eq!(reference.cpus, BTreeSet::from([15, 16]));
        Ok(())
    }

    #[test]
    fn report_prints_verdicts_notes_and_a_summary() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let before = aggregate(&[write_run(dir.path(), "before1.jsonl", vec![2000; BLOCKS])?])?;
        let after = aggregate(&[write_run(dir.path(), "after1.jsonl", vec![2000; BLOCKS])?])?;
        let mut output = Vec::new();
        assert_eq!(print(&mut output, &[before, after])?, Status::Pass);
        let text = String::from_utf8(output)?;
        assert!(text.contains("PASS b/x/fold: instructions before=10.0 after=10.0 change=+0.00%; cycles min before=20.0 after=20.0"));
        assert!(text.contains("library/reference=2.000x"));
        assert!(text.contains("  IPC before=0.50 after=0.50\n"));
        assert!(text.contains("cores 12"));
        assert!(text.ends_with(
            "PASS: 1 PASS, 0 FAIL (0 instructions, 0 cycles), 0 UNSTABLE library paths; 0 cycle warnings; 0 bistable\n"
        ));
        Ok(())
    }

    #[test]
    fn any_operation_name_is_a_library_path_and_rates_are_per_row() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("results.jsonl");
        let ids = BTreeSet::from([
            "r/dense/1024/all/sum-neon".to_owned(),
            "r/dense/1024/all/reference".to_owned(),
        ]);
        let blocks = |instructions: u64, cycles: u64| {
            [
                vec![instructions; BLOCKS],
                vec![cycles; BLOCKS],
                vec![0; BLOCKS],
                vec![0; BLOCKS],
            ]
        };
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                record(
                    "r/dense/1024/all/sum-neon",
                    10,
                    blocks(20480, 10240),
                    Vec::new()
                ),
                record(
                    "r/dense/1024/all/reference",
                    10,
                    blocks(40960, 40960),
                    Vec::new()
                ),
            ),
        )?;
        let run = load(&path, &ids)?;
        assert_eq!(run["r/dense/1024/all/sum-neon"].path, "sum-neon");
        let sides = [aggregate(std::slice::from_ref(&run))?, aggregate(&[run])?];
        let mut output = Vec::new();
        assert_eq!(print(&mut output, &sides)?, Status::Pass);
        let text = String::from_utf8(output)?;
        assert!(
            text.contains(
                "PASS r/dense/1024/all/sum-neon: instructions before=2048.0 after=2048.0"
            )
        );
        assert!(text.contains(
            "  PER ROW (1024 rows): instructions before=2.00 after=2.00; cycles min before=1.00 after=1.00; IPC before=2.00 after=2.00\n"
        ));
        assert!(text.contains("1 PASS, 0 FAIL"));
        Ok(())
    }

    #[test]
    fn zero_readings_and_short_records_are_rejected() -> Result<()> {
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
                Vec::new(),
            ),
        )?;
        assert!(load(&path, &only).is_err());
        let mut zero = vec![1000; BLOCKS];
        zero[3] = 0;
        fs::write(
            &path,
            record(
                "b/x/fold",
                1,
                [vec![1000; BLOCKS], zero, vec![0; BLOCKS], vec![0; BLOCKS]],
                Vec::new(),
            ),
        )?;
        let run = load(&path, &only)?;
        assert_eq!(run["b/x/fold"].process.zero_blocks, 1);
        assert!(run["b/x/fold"].process.cpus.is_empty());
        let side = &aggregate(&[run])?["b/x/fold"].side;
        assert!(!side.consistent());
        Ok(())
    }
}

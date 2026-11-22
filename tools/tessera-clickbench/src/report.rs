//! What a run keeps in its directory, `target/bench-runs/clickbench-<id>/`,
//! and the report of one run or of several side by side: each query's hot
//! time in each, the ratio to the first run, and the geometric means over
//! the queries every run timed; of counting runs, each query's median
//! cycles and instructions, their ratios and sums.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::check::QueryCheck;
use crate::measure::{QueryTimes, geometric_mean};

/// What a run was run on and with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    /// `check` or `run`.
    pub command: String,
    /// The modes of a check, the one mode of a run.
    pub modes: Vec<String>,
    pub files: Vec<String>,
    /// The foreign table's rows, as its statistics give them.
    pub rows: i64,
    pub workers: u32,
    pub work_mem: String,
    pub timeout_s: u64,
    pub deadline_s: u64,
    pub tries: u32,
    /// The commit, with `+` when the tree had changes.
    pub head: String,
    pub postgres: String,
    pub duckdb: Option<String>,
    pub started: String,
    pub power: String,
    /// Whether the run counted with pg_pmu instead of timing.
    #[serde(default)]
    pub counters: bool,
    /// DuckDB's threads in the mode duckdb; none in a run recorded before
    /// the tool set them, which ran with DuckDB's default.
    #[serde(default)]
    pub duckdb_threads: Option<u32>,
}

impl Meta {
    /// A short name for a column of the report.
    pub fn label(&self) -> String {
        let threads = match self.duckdb_threads {
            _ if !self.modes.iter().any(|mode| mode == "duckdb") => String::new(),
            Some(threads) => format!(" t{threads}"),
            None => " t default".into(),
        };
        format!(
            "{} w{}{threads} ({} file{}){}",
            self.modes.join(","),
            self.workers,
            self.files.len(),
            if self.files.len() == 1 { "" } else { "s" },
            if self.counters { ", counts" } else { "" }
        )
    }
}

/// run.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub meta: Meta,
    #[serde(default)]
    pub checks: Vec<QueryCheck>,
    #[serde(default)]
    pub times: Vec<QueryTimes>,
}

pub struct RunDir {
    pub path: PathBuf,
    pub id: String,
}

impl RunDir {
    pub fn create(runs: &Path) -> Result<RunDir> {
        loop {
            let id = tessera_pgtool::run::identifier();
            let path = runs.join(format!("clickbench-{id}"));
            if !path.exists() {
                fs::create_dir_all(&path)?;
                return Ok(RunDir { path, id });
            }
        }
    }

    pub fn write(&self, name: &str, text: &str) -> Result<()> {
        let path = self.path.join(name);
        fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))
    }
}

pub fn load(dir: &Path) -> Result<Run> {
    let path = dir.join("run.json");
    let text =
        fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
}

fn seconds(time: Option<f64>) -> String {
    time.map_or("-".into(), |time| format!("{time:.3}"))
}

/// The tries of one run, a line a query, and the mean of its hot times;
/// of a counting run, its counts.
pub fn single(run: &Run) -> String {
    if run.meta.counters {
        return single_counts(run);
    }
    let mut text = format!("{}\n", run.meta.label());
    let tries = run.meta.tries as usize;
    let _ = write!(text, "{:<5}", "query");
    for index in 1..=tries {
        let _ = write!(text, " {:>9}", format!("try {index}"));
    }
    let _ = writeln!(text, " {:>9}", "hot");
    for query in &run.times {
        let _ = write!(text, "Q{:<4}", query.query);
        for one in &query.tries {
            let shown = match one.seconds {
                Some(time) => format!("{time:.3}"),
                None => one
                    .outcome
                    .split(':')
                    .next()
                    .unwrap_or_default()
                    .to_string(),
            };
            let _ = write!(text, " {shown:>9}");
        }
        let _ = writeln!(text, " {:>9}", seconds(query.hot()));
    }
    let hot: Vec<f64> = run.times.iter().filter_map(QueryTimes::hot).collect();
    let _ = writeln!(
        text,
        "geometric mean of the hot times plus 10 ms: {} s over {} of {} queries",
        seconds(geometric_mean(&hot)),
        hot.len(),
        run.times.len()
    );
    text
}

/// Billions, as counts are read.
fn billions(count: Option<u64>) -> String {
    count.map_or("-".into(), |count| format!("{:.3}", count as f64 / 1e9))
}

/// The cycles of one query's tries, their least and greatest over the
/// median, as a share.
fn spread(times: &QueryTimes) -> String {
    let cycles: Option<Vec<u64>> = times.tries.iter().map(|one| one.cycles).collect();
    match (cycles, times.median(|one| one.cycles)) {
        (Some(cycles), Some(median)) if median > 0 && !cycles.is_empty() => {
            let (low, high) = (cycles.iter().min(), cycles.iter().max());
            match (low, high) {
                (Some(&low), Some(&high)) => {
                    format!("{:.1} %", (high - low) as f64 * 100.0 / median as f64)
                }
                _ => "-".into(),
            }
        }
        _ => "-".into(),
    }
}

/// A counting run: each query's median cycles and instructions, in
/// billions, with the spread of its cycles and its workers' share, and
/// their sums.
fn single_counts(run: &Run) -> String {
    let mut text = format!("{}\n", run.meta.label());
    let _ = writeln!(
        text,
        "{:<5} {:>10} {:>12} {:>8} {:>9}",
        "query", "cycles G", "instructions G", "spread", "workers"
    );
    let (mut cycles, mut instructions, mut summed) = (0u64, 0u64, 0);
    for times in &run.times {
        let median = times.median(|one| one.cycles);
        let counted = times.median(|one| one.instructions);
        let workers = match (times.median(|one| one.workers_cycles), median) {
            (Some(workers), Some(all)) if all > 0 => {
                format!("{:.0} %", workers as f64 * 100.0 / all as f64)
            }
            _ => "-".into(),
        };
        let _ = writeln!(
            text,
            "Q{:<4} {:>10} {:>14} {:>8} {:>9}",
            times.query,
            billions(median),
            billions(counted),
            spread(times),
            workers
        );
        if let (Some(median), Some(counted)) = (median, counted) {
            cycles += median;
            instructions += counted;
            summed += 1;
        }
    }
    let _ = writeln!(
        text,
        "sum   {:>10} {:>14}   over {summed} of {} queries; medians of {} readings a query, \
         each after one that warms up",
        billions(Some(cycles)),
        billions(Some(instructions)),
        run.times.len(),
        run.meta.tries
    );
    text
}

/// Counting runs side by side: each query's median cycles and
/// instructions in each, the ratio to the first run's, and the sums over
/// the queries every run counted.
fn side_by_side_counts(runs: &[Run]) -> String {
    let mut queries: Vec<usize> = runs
        .iter()
        .flat_map(|run| run.times.iter().map(|times| times.query))
        .collect();
    queries.sort_unstable();
    queries.dedup();
    let mut text = String::new();
    for (index, run) in runs.iter().enumerate() {
        let _ = writeln!(
            text,
            "{}: {} {}",
            letter(index),
            run.meta.id,
            run.meta.label()
        );
    }
    type Count = fn(&crate::measure::Try) -> Option<u64>;
    let metrics: [(&str, Count); 2] = [
        ("cycles", |one| one.cycles),
        ("instructions", |one| one.instructions),
    ];
    for (name, count) in metrics {
        let _ = write!(text, "\n{name}, billions\n{:<5} {:>9}", "query", letter(0));
        for index in 1..runs.len() {
            let _ = write!(
                text,
                " {:>9} {:>7}",
                letter(index),
                format!("{}/A", letter(index))
            );
        }
        let _ = writeln!(text);
        let mut sums = vec![0u64; runs.len()];
        let mut common = 0;
        for &query in &queries {
            let counts: Vec<Option<u64>> = runs
                .iter()
                .map(|run| {
                    run.times
                        .iter()
                        .find(|times| times.query == query)
                        .and_then(|times| times.median(count))
                })
                .collect();
            let _ = write!(text, "Q{query:<4} {:>9}", billions(counts[0]));
            for one in &counts[1..] {
                let ratio = match (counts[0], one) {
                    (Some(base), Some(one)) if base > 0 => {
                        format!("{:.2}", *one as f64 / base as f64)
                    }
                    _ => "-".into(),
                };
                let _ = write!(text, " {:>9} {ratio:>7}", billions(*one));
            }
            let _ = writeln!(text);
            if counts.iter().all(Option::is_some) {
                common += 1;
                for (sum, one) in sums.iter_mut().zip(&counts) {
                    *sum += one.unwrap_or(0);
                }
            }
        }
        let _ = write!(text, "{:<5} {:>9}", "sum", billions(Some(sums[0])));
        for sum in &sums[1..] {
            let ratio = if sums[0] > 0 {
                format!("{:.2}", *sum as f64 / sums[0] as f64)
            } else {
                "-".into()
            };
            let _ = write!(text, " {:>9} {ratio:>7}", billions(Some(*sum)));
        }
        let _ = writeln!(text, "\nsums over the {common} queries every run counted");
    }
    text
}

/// Several runs side by side: the hot time of each query in each, and the
/// ratio of each run's to the first's; the geometric means over the
/// queries that every run timed. Counting runs, by their counts.
pub fn side_by_side(runs: &[Run]) -> Result<String> {
    let Some(first) = runs.first() else {
        bail!("no run to report");
    };
    if runs
        .iter()
        .any(|run| run.meta.counters != first.meta.counters)
    {
        bail!("runs that time and runs that count are not reported together");
    }
    if first.meta.counters {
        let mut text = side_by_side_counts(runs);
        if runs.iter().any(|run| run.meta.files != first.meta.files) {
            let _ = writeln!(text, "warning: the runs read different files");
        }
        return Ok(text);
    }
    let mut queries: Vec<usize> = runs
        .iter()
        .flat_map(|run| run.times.iter().map(|times| times.query))
        .collect();
    queries.sort_unstable();
    queries.dedup();
    let hot = |run: &Run, query: usize| {
        run.times
            .iter()
            .find(|times| times.query == query)
            .and_then(QueryTimes::hot)
    };
    let mut text = String::new();
    for (index, run) in runs.iter().enumerate() {
        let _ = writeln!(
            text,
            "{}: {} {}",
            letter(index),
            run.meta.id,
            run.meta.label()
        );
    }
    let _ = write!(text, "{:<5} {:>9}", "query", letter(0));
    for index in 1..runs.len() {
        let _ = write!(
            text,
            " {:>9} {:>7}",
            letter(index),
            format!("{}/A", letter(index))
        );
    }
    let _ = writeln!(text);
    let mut common: Vec<Vec<f64>> = vec![Vec::new(); runs.len()];
    for &query in &queries {
        let times: Vec<Option<f64>> = runs.iter().map(|run| hot(run, query)).collect();
        let _ = write!(text, "Q{query:<4} {:>9}", seconds(times[0]));
        for time in &times[1..] {
            let ratio = match (times[0], time) {
                (Some(base), Some(time)) => format!("{:.2}", (time + 0.01) / (base + 0.01)),
                _ => "-".into(),
            };
            let _ = write!(text, " {:>9} {ratio:>7}", seconds(*time));
        }
        let _ = writeln!(text);
        if times.iter().all(Option::is_some) {
            for (column, time) in common.iter_mut().zip(&times) {
                column.extend(*time);
            }
        }
    }
    let means: Vec<Option<f64>> = common.iter().map(|times| geometric_mean(times)).collect();
    let _ = write!(text, "{:<5} {:>9}", "mean", seconds(means[0]));
    for mean in &means[1..] {
        let ratio = match (means[0], mean) {
            (Some(base), Some(mean)) => format!("{:.2}", mean / base),
            _ => "-".into(),
        };
        let _ = write!(text, " {:>9} {ratio:>7}", seconds(*mean));
    }
    let _ = writeln!(
        text,
        "\nmeans: geometric, of the hot times plus 10 ms, over the {} queries every run timed; \
         a ratio is plus 10 ms too, below 1 faster than A",
        common[0].len()
    );
    if runs.iter().any(|run| run.meta.files != first.meta.files) {
        let _ = writeln!(text, "warning: the runs read different files");
    }
    Ok(text)
}

fn letter(index: usize) -> char {
    (b'A' + (index % 26) as u8) as char
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::measure::Try;

    fn run(id: &str, mode: &str, hot: &[(usize, Option<f64>)]) -> Run {
        Run {
            meta: Meta {
                id: id.into(),
                command: "run".into(),
                modes: vec![mode.into()],
                files: vec!["/data/hits.parquet".into()],
                rows: 100,
                workers: 0,
                work_mem: "64MB".into(),
                timeout_s: 60,
                deadline_s: 300,
                tries: 1,
                head: "abc".into(),
                postgres: "PostgreSQL 19".into(),
                duckdb: None,
                started: "now".into(),
                power: "AC Power".into(),
                counters: false,
                duckdb_threads: None,
            },
            checks: Vec::new(),
            times: hot
                .iter()
                .map(|&(query, seconds)| QueryTimes {
                    query,
                    tries: vec![Try {
                        seconds,
                        outcome: if seconds.is_some() { "ok" } else { "timeout" }.into(),
                        rows: None,
                        instructions: None,
                        cycles: None,
                        workers_cycles: None,
                    }],
                })
                .collect(),
        }
    }

    #[test]
    fn runs_are_reported_side_by_side() {
        let a = run(
            "aaaaaa",
            "off",
            &[(1, Some(0.99)), (2, Some(0.09)), (3, Some(1.0))],
        );
        let b = run(
            "bbbbbb",
            "on",
            &[(1, Some(0.49)), (2, Some(0.04)), (3, None)],
        );
        let text = side_by_side(&[a, b]).unwrap();
        assert!(text.contains("A: aaaaaa off w0 (1 file)"), "{text}");
        assert!(text.contains("Q1        0.990     0.490    0.50"), "{text}");
        assert!(text.contains("Q3        1.000         -       -"), "{text}");
        // Q3 is left out of the means: B did not time it.
        assert!(text.contains("over the 2 queries"), "{text}");
        assert!(text.contains("mean      0.316     0.158    0.50"), "{text}");
        assert!(side_by_side(&[]).is_err());
    }

    #[test]
    fn a_single_run_shows_its_tries() {
        let text = single(&run("aaaaaa", "on", &[(1, Some(0.5)), (2, None)]));
        assert!(text.contains("Q1        0.500     0.500"), "{text}");
        assert!(text.contains("Q2      timeout         -"), "{text}");
        assert!(text.contains("over 1 of 2 queries"), "{text}");
    }

    fn counted(id: &str, mode: &str, cycles: &[(usize, &[u64])]) -> Run {
        let mut run = run(id, mode, &[]);
        run.meta.counters = true;
        run.meta.tries = 3;
        run.times = cycles
            .iter()
            .map(|&(query, cycles)| QueryTimes {
                query,
                tries: cycles
                    .iter()
                    .map(|&cycles| Try {
                        seconds: Some(0.1),
                        outcome: "ok".into(),
                        rows: None,
                        instructions: Some(cycles * 2),
                        cycles: Some(cycles),
                        workers_cycles: Some(cycles / 2),
                    })
                    .collect(),
            })
            .collect();
        run
    }

    #[test]
    fn counting_runs_are_reported_by_their_medians() {
        let a = counted(
            "aaaaaa",
            "on",
            &[
                (1, &[2_000_000_000, 1_000_000_000, 3_000_000_000]),
                (2, &[500_000_000; 3]),
            ],
        );
        let text = single(&a);
        assert!(text.contains("on w0 (1 file), counts"), "{text}");
        assert!(
            text.contains("Q1         2.000          4.000  100.0 %      50 %"),
            "{text}"
        );
        assert!(
            text.contains("sum        2.500          5.000   over 2 of 2"),
            "{text}"
        );
        let mut short = a.clone();
        short.times[1].tries[0].cycles = None;
        assert!(
            single(&short).contains("over 1 of 2 queries"),
            "{}",
            single(&short)
        );
        let mut b = counted(
            "bbbbbb",
            "duckdb",
            &[(1, &[1_000_000_000; 3]), (2, &[1_000_000_000; 3])],
        );
        b.meta.duckdb_threads = Some(9);
        let both = side_by_side(&[a.clone(), b.clone()]).unwrap();
        assert!(
            both.contains("B: bbbbbb duckdb w0 t9 (1 file), counts"),
            "{both}"
        );
        b.meta.duckdb_threads = None;
        assert!(b.meta.label().contains(" t default"), "{}", b.meta.label());
        assert!(both.contains("Q1        2.000     1.000    0.50"), "{both}");
        assert!(both.contains("sum       2.500     2.000    0.80"), "{both}");
        // A timing run and a counting run are not reported together.
        assert!(side_by_side(&[a, run("cccccc", "on", &[(1, Some(1.0))])]).is_err());
    }

    #[test]
    fn duckdb_runs_are_labelled_by_their_threads() {
        let mut duckdb = run("aaaaaa", "duckdb", &[]);
        assert_eq!(duckdb.meta.label(), "duckdb w0 t default (1 file)");
        duckdb.meta.duckdb_threads = Some(9);
        assert_eq!(duckdb.meta.label(), "duckdb w0 t9 (1 file)");
        let mut on = run("bbbbbb", "on", &[]);
        on.meta.duckdb_threads = Some(1);
        assert_eq!(on.meta.label(), "on w0 (1 file)");
    }

    #[test]
    fn a_run_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let saved = run("aaaaaa", "on", &[(1, Some(0.5))]);
        fs::write(
            dir.path().join("run.json"),
            serde_json::to_string(&saved).unwrap(),
        )
        .unwrap();
        let read = load(dir.path()).unwrap();
        assert_eq!(read.meta, saved.meta);
        assert_eq!(read.times, saved.times);
        assert!(load(&dir.path().join("none")).is_err());
    }
}

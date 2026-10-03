//! The timings of bench/pg runs, and two revisions' runs compared case by
//! case against the control without Tessera.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use anyhow::{Context, Result, bail};

/// The mode a family times with Tessera off: the control of mode `on`.
pub const CONTROL: &str = "off";

/// The milliseconds of each case and mode of one run, as its
/// `timings.csv` (`test,mode,run,milliseconds`) lists them.
pub type Timings = BTreeMap<(String, String), Vec<f64>>;

/// A run's timings, and how many readings it left out: a time not above
/// zero, which only the wall clock the families read, stepped back during
/// a case, gives.
#[derive(Debug, Default)]
pub struct Run {
    pub timings: Timings,
    pub dropped: usize,
}

/// Parse a run's `timings.csv`.
pub fn parse(csv: &str) -> Result<Run> {
    let mut lines = csv.lines();
    let header: Vec<&str> = lines
        .next()
        .context("timings.csv is empty")?
        .split(',')
        .collect();
    let column = |name: &str| {
        header
            .iter()
            .position(|column| *column == name)
            .with_context(|| format!("timings.csv has no column {name}"))
    };
    let (test, mode, milliseconds) = (column("test")?, column("mode")?, column("milliseconds")?);
    let mut run = Run::default();
    for line in lines.filter(|line| !line.is_empty()) {
        let fields: Vec<&str> = line.split(',').collect();
        let field = |at: usize| {
            fields
                .get(at)
                .copied()
                .with_context(|| format!("a short line of timings.csv: {line}"))
        };
        let value: f64 = field(milliseconds)?
            .parse()
            .with_context(|| format!("not a time: {line}"))?;
        if value <= 0.0 {
            run.dropped += 1;
            continue;
        }
        run.timings
            .entry((field(test)?.to_owned(), field(mode)?.to_owned()))
            .or_default()
            .push(value);
    }
    if run.timings.is_empty() {
        bail!("timings.csv has no timings");
    }
    Ok(run)
}

/// One side's time of a case and mode over its runs: the least of any run,
/// which is the time in the best state the machine was in, and the mean of
/// the runs' medians, which is the usual time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    pub min: f64,
    pub median: f64,
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        (sorted[middle - 1] + sorted[middle]) / 2.
    }
}

/// The summary of each case and mode over one side's runs; a case a run
/// lacks counts in the others.
pub fn summarize(runs: &[Timings]) -> BTreeMap<(String, String), Summary> {
    let mut per_run: BTreeMap<(String, String), Vec<(f64, f64)>> = BTreeMap::new();
    for run in runs {
        for (key, values) in run.iter().filter(|(_, values)| !values.is_empty()) {
            let least = values.iter().copied().fold(f64::INFINITY, f64::min);
            per_run
                .entry(key.clone())
                .or_default()
                .push((least, median(values)));
        }
    }
    per_run
        .into_iter()
        .map(|(key, runs)| {
            let min = runs.iter().map(|run| run.0).fold(f64::INFINITY, f64::min);
            #[expect(
                clippy::cast_precision_loss,
                reason = "a handful of runs, exactly a float"
            )]
            let median = runs.iter().map(|run| run.1).sum::<f64>() / runs.len() as f64;
            (key, Summary { min, median })
        })
        .collect()
}

/// What a case did between the revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Within the threshold, or moved by one measure only.
    Same,
    /// Slower past the threshold by its minimum and by its median beyond
    /// the control's move.
    Slower,
    /// Faster in the same sense.
    Faster,
}

/// A case and mode of both sides: the candidate's time over the base's.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub case: String,
    pub mode: String,
    pub base: Summary,
    pub candidate: Summary,
    /// The ratios of the minimums and of the medians.
    pub min: f64,
    pub median: f64,
    /// The ratio of the medians of the control, for mode `on`.
    pub control: Option<f64>,
    pub verdict: Verdict,
}

/// Compare two sides' runs case by case. `threshold` is a fraction (0.02
/// for 2 %). The control's move is taken off the median's, so that a
/// machine slower in the candidate's runs does not make a change slower.
pub fn compare(base: &[Timings], candidate: &[Timings], threshold: f64) -> Vec<Row> {
    let (base, candidate) = (summarize(base), summarize(candidate));
    let mut rows = Vec::new();
    for (key, b) in &base {
        let (case, mode) = key;
        if mode == CONTROL && base.contains_key(&(case.clone(), "on".to_owned())) {
            continue;
        }
        let Some(c) = candidate.get(key) else {
            continue;
        };
        let control = (mode == "on")
            .then(|| {
                let key = (case.clone(), CONTROL.to_owned());
                Some(candidate.get(&key)?.median / base.get(&key)?.median)
            })
            .flatten();
        let min = c.min / b.min;
        let median = c.median / b.median;
        let beyond = median - control.unwrap_or(1.);
        let verdict = if min > 1. + threshold && beyond > threshold {
            Verdict::Slower
        } else if min < 1. - threshold && beyond < -threshold {
            Verdict::Faster
        } else {
            Verdict::Same
        };
        rows.push(Row {
            case: case.clone(),
            mode: mode.clone(),
            base: *b,
            candidate: *c,
            min,
            median,
            control,
            verdict,
        });
    }
    rows
}

/// The rows as a Markdown table.
pub fn table(rows: &[Row]) -> String {
    let modes: BTreeSet<&str> = rows.iter().map(|row| row.mode.as_str()).collect();
    let show_mode = modes.len() > 1 || modes.first().is_some_and(|mode| *mode != "on");
    let mut out = String::new();
    let mode_header = if show_mode { " mode |" } else { "" };
    let mode_rule = if show_mode { "---|" } else { "" };
    let _ = writeln!(
        out,
        "| case |{mode_header} base, ms | candidate, ms | min | median | control | |"
    );
    let _ = writeln!(out, "|---|{mode_rule}---:|---:|---:|---:|---:|---|");
    for row in rows {
        let mode = if show_mode {
            format!(" {} |", row.mode)
        } else {
            String::new()
        };
        let control = row
            .control
            .map_or_else(|| "–".to_owned(), |ratio| format!("{ratio:.3}"));
        let verdict = match row.verdict {
            Verdict::Same => "",
            Verdict::Slower => "**slower**",
            Verdict::Faster => "faster",
        };
        let _ = writeln!(
            out,
            "| {} |{mode} {:.3} | {:.3} | {:.3} | {:.3} | {control} | {verdict} |",
            row.case, row.base.median, row.candidate.median, row.min, row.median
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(rows: &[(&str, &str, &[f64])]) -> Timings {
        rows.iter()
            .map(|(case, mode, values)| ((case.to_string(), mode.to_string()), values.to_vec()))
            .collect()
    }

    #[test]
    fn timings_parse_by_their_header() -> Result<()> {
        let run = parse(
            "test,mode,run,milliseconds\nsum,on,1,5.5\nsum,on,2,5.25\nsum,off,1,10\nsum,on,3,-113\n",
        )?;
        assert_eq!(run.timings[&("sum".into(), "on".into())], [5.5, 5.25]);
        assert_eq!(run.timings[&("sum".into(), "off".into())], [10.]);
        assert_eq!(run.dropped, 1, "a reading of a clock stepped back");
        assert!(parse("test,mode,run\n").is_err());
        assert!(parse("test,mode,run,milliseconds\nsum,on,1,fast\n").is_err());
        Ok(())
    }

    #[test]
    fn a_side_takes_the_least_time_and_the_mean_of_medians() {
        let summary = summarize(&[
            run(&[("a", "on", &[3., 1., 2.])]),
            run(&[("a", "on", &[4., 5., 6., 7.])]),
        ]);
        assert_eq!(
            summary[&("a".into(), "on".into())],
            Summary {
                min: 1.,
                median: 3.75
            }
        );
    }

    #[test]
    fn a_case_is_slower_past_the_threshold_beyond_the_control() {
        let base = [run(&[("a", "on", &[5.]), ("a", "off", &[10.])])];
        // 5 % slower, the control still: slower.
        let slower = [run(&[("a", "on", &[5.25]), ("a", "off", &[10.])])];
        let rows = compare(&base, &slower, 0.02);
        assert_eq!(rows.len(), 1, "the control has no row of its own");
        assert_eq!(rows[0].verdict, Verdict::Slower);
        assert_eq!(rows[0].control, Some(1.));
        // 5 % slower with the control 4 % slower: the machine, not the change.
        let drift = [run(&[("a", "on", &[5.25]), ("a", "off", &[10.4])])];
        assert_eq!(compare(&base, &drift, 0.02)[0].verdict, Verdict::Same);
        // Faster in the same sense.
        let faster = [run(&[("a", "on", &[4.5]), ("a", "off", &[10.])])];
        assert_eq!(compare(&base, &faster, 0.02)[0].verdict, Verdict::Faster);
        // A mode without a control is judged by its own times.
        let base = [run(&[("p", "2", &[5.])])];
        let slower = [run(&[("p", "2", &[5.5])])];
        let rows = compare(&base, &slower, 0.02);
        assert_eq!((rows[0].control, rows[0].verdict), (None, Verdict::Slower));
        assert!(table(&rows).contains("| p | 2 |"));
    }
}

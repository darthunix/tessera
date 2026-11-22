//! The timings of bench/pg runs, and two revisions' runs compared case by
//! case against the control without Tessera.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use anyhow::{Context, Result, bail};

/// The mode a family times with Tessera off: the control of mode `on`.
pub const CONTROL: &str = "off";

/// The readings of each case and mode of one run, as its `timings.csv`
/// (`test,mode,run,reading`) lists them: times in milliseconds, or the
/// counts of instructions of a run with `METRIC=instructions`. A file of
/// a calibration names the column `milliseconds`.
pub type Timings = BTreeMap<(String, String), Vec<f64>>;

/// A run's timings, and how many readings it left out: a time not above
/// zero, which only the wall clock the families read, stepped back during
/// a case, gives. A run that counts also lists, by the columns `disk` and
/// `plan`, what the EXPLAIN of each count showed.
#[derive(Debug, Default)]
pub struct Run {
    pub timings: Timings,
    /// The kB each count's plan reported written to disk.
    pub disk: Timings,
    /// The shapes of the plans each case and mode was counted in.
    pub plans: BTreeMap<(String, String), BTreeSet<String>>,
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
    let reading = column("reading").or_else(|_| column("milliseconds"))?;
    let (test, mode) = (column("test")?, column("mode")?);
    let (disk, plan) = (column("disk").ok(), column("plan").ok());
    let mut run = Run::default();
    for line in lines.filter(|line| !line.is_empty()) {
        let fields: Vec<&str> = line.split(',').collect();
        let field = |at: usize| {
            fields
                .get(at)
                .copied()
                .with_context(|| format!("a short line of timings.csv: {line}"))
        };
        let value: f64 = field(reading)?
            .parse()
            .with_context(|| format!("not a reading: {line}"))?;
        if value <= 0.0 {
            run.dropped += 1;
            continue;
        }
        let key = (field(test)?.to_owned(), field(mode)?.to_owned());
        // A time leaves both empty.
        if let Some(kb) = disk.map(field).transpose()?.filter(|kb| !kb.is_empty()) {
            let kb = kb
                .parse()
                .with_context(|| format!("not a disk in kB: {line}"))?;
            run.disk.entry(key.clone()).or_default().push(kb);
        }
        if let Some(shape) = plan
            .map(field)
            .transpose()?
            .filter(|shape| !shape.is_empty())
        {
            run.plans
                .entry(key.clone())
                .or_default()
                .insert(shape.to_owned());
        }
        run.timings.entry(key).or_default().push(value);
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
pub fn summarize<'a>(
    runs: impl IntoIterator<Item = &'a Timings>,
) -> BTreeMap<(String, String), Summary> {
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
    /// By counts, run in plans of another shape on the two sides, or in
    /// more than one: the case's own (`case`), its control's, or both.
    /// The count then compares other nodes and their count of rows, not
    /// the change, so it is not judged.
    Replanned { case: bool, control: bool },
}

/// The kB a case's plan wrote to disk, the median of each side's counts,
/// when either side wrote any.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Disk {
    pub base: f64,
    pub candidate: f64,
    /// Whether every count of each side wrote the same within the
    /// threshold. With parallel workers it moves from run to run, and is
    /// then not judged.
    pub settled: bool,
    /// `Slower` when the candidate writes more past the threshold,
    /// `Faster` when less; `Same` when not settled.
    pub verdict: Verdict,
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
    /// By counts: what it wrote to disk.
    pub disk: Option<Disk>,
}

/// Compare two sides' runs case by case. `threshold` is a fraction (0.02
/// for 2 %). The control's move is taken off the median's, so that a
/// machine slower in the candidate's runs does not make a change slower.
pub fn compare(base_runs: &[Run], candidate_runs: &[Run], threshold: f64) -> Vec<Row> {
    let summary = |runs: &[Run], disk: bool| {
        summarize(
            runs.iter()
                .map(|run| if disk { &run.disk } else { &run.timings }),
        )
    };
    let (base, candidate) = (summary(base_runs, false), summary(candidate_runs, false));
    let (base_disk, candidate_disk) = (summary(base_runs, true), summary(candidate_runs, true));
    let plans = |runs: &[Run], key: &(String, String)| -> BTreeSet<String> {
        runs.iter()
            .filter_map(|run| run.plans.get(key))
            .flatten()
            .cloned()
            .collect()
    };
    // A count is compared only where every reading of both sides ran
    // one shape: a time records none.
    let replanned = |key: &(String, String)| {
        let base = plans(base_runs, key);
        base.len() > 1 || base != plans(candidate_runs, key)
    };
    // Every count of a side within the threshold of its largest.
    let settled = |runs: &[Run], key: &(String, String)| {
        let values = runs
            .iter()
            .filter_map(|run| run.disk.get(key))
            .flatten()
            .copied();
        let (least, most) = values.fold((f64::INFINITY, 0_f64), |(least, most), value| {
            (least.min(value), most.max(value))
        });
        most - least <= threshold * most
    };
    let moved = |ratio: f64| {
        if ratio > 1. + threshold {
            Verdict::Slower
        } else if ratio < 1. - threshold {
            Verdict::Faster
        } else {
            Verdict::Same
        }
    };
    let mut rows = Vec::new();
    for (key, b) in &base {
        let (case, mode) = key;
        if mode == CONTROL && base.contains_key(&(case.clone(), "on".to_owned())) {
            continue;
        }
        let Some(c) = candidate.get(key) else {
            continue;
        };
        let control_key = (case.clone(), CONTROL.to_owned());
        let control = (mode == "on")
            .then(|| Some(candidate.get(&control_key)?.median / base.get(&control_key)?.median))
            .flatten();
        let min = c.min / b.min;
        let median = c.median / b.median;
        let beyond = median - control.unwrap_or(1.);
        let disk = base_disk
            .get(key)
            .zip(candidate_disk.get(key))
            .filter(|(b, c)| b.median > 0. || c.median > 0.)
            .map(|(b, c)| {
                let settled = settled(base_runs, key) && settled(candidate_runs, key);
                Disk {
                    base: b.median,
                    candidate: c.median,
                    settled,
                    verdict: if settled {
                        moved(c.median / b.median)
                    } else {
                        Verdict::Same
                    },
                }
            });
        let (case_replanned, control_replanned) =
            (replanned(key), mode == "on" && replanned(&control_key));
        let verdict = if case_replanned || control_replanned {
            Verdict::Replanned {
                case: case_replanned,
                control: control_replanned,
            }
        } else if min > 1. + threshold && beyond > threshold {
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
            disk,
        });
    }
    rows
}

/// What the readings of a table are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Milliseconds,
    /// Shown in millions.
    Instructions,
    /// Shown in millions.
    Cycles,
}

/// The rows as a Markdown table, the readings in `unit`.
pub fn table(rows: &[Row], unit: Unit) -> String {
    let (label, scale) = match unit {
        Unit::Milliseconds => ("ms", 1.),
        Unit::Instructions => ("M instructions", 1e-6),
        Unit::Cycles => ("M cycles", 1e-6),
    };
    let modes: BTreeSet<&str> = rows.iter().map(|row| row.mode.as_str()).collect();
    let show_mode = modes.len() > 1 || modes.first().is_some_and(|mode| *mode != "on");
    let show_disk = rows.iter().any(|row| row.disk.is_some());
    let mut out = String::new();
    let mode_header = if show_mode { " mode |" } else { "" };
    let mode_rule = if show_mode { "---|" } else { "" };
    let (disk_header, disk_rule) = if show_disk {
        (" disk |", "---:|")
    } else {
        ("", "")
    };
    let _ = writeln!(
        out,
        "| case |{mode_header} base, {label} | candidate, {label} | min | median | control |{disk_header} |"
    );
    let _ = writeln!(
        out,
        "|---|{mode_rule}---:|---:|---:|---:|---:|{disk_rule}---|"
    );
    for row in rows {
        let mode = if show_mode {
            format!(" {} |", row.mode)
        } else {
            String::new()
        };
        let ratio = |ratio: f64, replanned: bool| {
            if replanned {
                "–".to_owned()
            } else {
                format!("{ratio:.3}")
            }
        };
        let (case_replanned, control_replanned) = match row.verdict {
            Verdict::Replanned { case, control } => (case, control),
            _ => (false, false),
        };
        let control = row.control.map_or_else(
            || "–".to_owned(),
            |control| ratio(control, control_replanned),
        );
        let (min, median) = (
            ratio(row.min, case_replanned),
            ratio(row.median, case_replanned),
        );
        let verdict = match row.verdict {
            Verdict::Slower => "**slower**",
            Verdict::Faster => "faster",
            Verdict::Replanned { case: true, .. } => "**plan differs**",
            Verdict::Replanned { case: false, .. } => "**control's plan differs**",
            Verdict::Same => "",
        };
        let (disk, disk_verdict) = match row.disk {
            None => ("–".to_owned(), ""),
            Some(disk) => (
                if disk.base > 0. {
                    format!("{:.3}", disk.candidate / disk.base)
                } else {
                    "new".to_owned()
                },
                match disk.verdict {
                    _ if !disk.settled => "disk varies",
                    Verdict::Slower => "**more disk**",
                    Verdict::Faster => "less disk",
                    _ => "",
                },
            ),
        };
        let disk = if show_disk {
            format!(" {disk} |")
        } else {
            String::new()
        };
        let verdict = [verdict, disk_verdict]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "| {} |{mode} {:.3} | {:.3} | {min} | {median} | {control} |{disk} {verdict} |",
            row.case,
            row.base.median * scale,
            row.candidate.median * scale,
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

    /// A side's run of those readings, with no disk and no plans.
    fn side(rows: &[(&str, &str, &[f64])]) -> Run {
        Run {
            timings: run(rows),
            ..Run::default()
        }
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
        // The families' column, in any place: a time or a count.
        let counts = parse("reading,test,mode,run\n3000000,sum,on,1\n")?;
        assert_eq!(counts.timings[&("sum".into(), "on".into())], [3e6]);
        // A count's disk and plan; a time leaves them empty.
        let counted = parse(
            "test,mode,run,reading,disk,plan\n\
             sum,on,1,3000000,5195,TessAgg(TessHeapScan)\n\
             sum,on,2,3000001,5195,TessAgg(TessHeapScan)\n\
             sum,off,1,9000000,0,Aggregate[Plain](Seq Scan)\n",
        )?;
        let key = ("sum".to_owned(), "on".to_owned());
        assert_eq!(counted.disk[&key], [5195., 5195.]);
        assert_eq!(
            counted.plans[&key],
            BTreeSet::from(["TessAgg(TessHeapScan)".to_owned()])
        );
        let timed = parse("test,mode,run,reading,disk,plan\nsum,on,1,5.5,,\n")?;
        assert!(timed.disk.is_empty() && timed.plans.is_empty());
        assert!(parse("test,mode,run,reading,disk,plan\nsum,on,1,5,lots,\n").is_err());
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
        let base = [side(&[("a", "on", &[5.]), ("a", "off", &[10.])])];
        // 5 % slower, the control still: slower.
        let slower = [side(&[("a", "on", &[5.25]), ("a", "off", &[10.])])];
        let rows = compare(&base, &slower, 0.02);
        assert_eq!(rows.len(), 1, "the control has no row of its own");
        assert_eq!(rows[0].verdict, Verdict::Slower);
        assert_eq!(rows[0].control, Some(1.));
        // 5 % slower with the control 4 % slower: the machine, not the change.
        let drift = [side(&[("a", "on", &[5.25]), ("a", "off", &[10.4])])];
        assert_eq!(compare(&base, &drift, 0.02)[0].verdict, Verdict::Same);
        // Faster in the same sense.
        let faster = [side(&[("a", "on", &[4.5]), ("a", "off", &[10.])])];
        assert_eq!(compare(&base, &faster, 0.02)[0].verdict, Verdict::Faster);
        // A mode without a control is judged by its own times.
        let base = [side(&[("p", "2", &[5.])])];
        let slower = [side(&[("p", "2", &[5.5])])];
        let rows = compare(&base, &slower, 0.02);
        assert_eq!((rows[0].control, rows[0].verdict), (None, Verdict::Slower));
        assert!(table(&rows, Unit::Milliseconds).contains("| p | 2 |"));
    }

    #[test]
    fn a_table_of_counts_shows_millions_of_instructions() {
        let base = [side(&[("a", "on", &[3_000_000.]), ("a", "off", &[9e6])])];
        let candidate = [side(&[("a", "on", &[3_150_000.]), ("a", "off", &[9e6])])];
        let rows = compare(&base, &candidate, 0.01);
        assert_eq!(rows[0].verdict, Verdict::Slower);
        let table = table(&rows, Unit::Instructions);
        assert!(table.contains("base, M instructions"), "{table}");
        assert!(
            table.contains("| a | 3.000 | 3.150 | 1.050 | 1.050 | 1.000 |"),
            "{table}"
        );
        let cycles = super::table(&rows, Unit::Cycles);
        assert!(cycles.contains("base, M cycles"), "{cycles}");
    }

    #[test]
    fn a_count_is_not_judged_where_the_plans_differ() {
        let counted = |reading: f64, on: &str, off: &str| {
            let mut run = side(&[("a", "on", &[reading]), ("a", "off", &[9e6])]);
            for (mode, plan) in [("on", on), ("off", off)] {
                run.plans
                    .insert(("a".into(), mode.into()), BTreeSet::from([plan.to_owned()]));
            }
            run
        };
        let base = [counted(3e6, "TessAgg(TessHeapScan)", "Aggregate(Seq Scan)")];
        let same = [counted(
            3.3e6,
            "TessAgg(TessHeapScan)",
            "Aggregate(Seq Scan)",
        )];
        assert_eq!(compare(&base, &same, 0.01)[0].verdict, Verdict::Slower);
        // Another plan in mode on: no ratio, no verdict.
        let on = [counted(
            2e6,
            "Aggregate(TessHeapScan)",
            "Aggregate(Seq Scan)",
        )];
        let rows = compare(&base, &on, 0.01);
        assert_eq!(
            rows[0].verdict,
            Verdict::Replanned {
                case: true,
                control: false
            }
        );
        assert!(
            table_of(&rows).contains("| – | – | 1.000 | **plan differs** |"),
            "{}",
            table_of(&rows)
        );
        // Another plan in the control alone: the case's ratio, no control.
        let off = [counted(3e6, "TessAgg(TessHeapScan)", "Hash(Seq Scan)")];
        let rows = compare(&base, &off, 0.01);
        assert!(
            table_of(&rows).contains("| 1.000 | 1.000 | – | **control's plan differs** |"),
            "{}",
            table_of(&rows)
        );
        // The same two plans on both sides, in other shares: not compared.
        let mixed = |reading: f64| {
            let mut run = counted(reading, "TessAgg(TessHeapScan)", "Aggregate(Seq Scan)");
            run.plans
                .get_mut(&("a".to_owned(), "on".to_owned()))
                .map(|plans| plans.insert("Aggregate(TessHeapScan)".to_owned()));
            run
        };
        let rows = compare(&[mixed(3e6)], &[mixed(5e6)], 0.01);
        assert!(matches!(
            rows[0].verdict,
            Verdict::Replanned { case: true, .. }
        ));
    }

    #[test]
    fn a_count_shows_the_disk_it_wrote() {
        let wrote = |reading: f64, kb: f64| {
            let mut run = side(&[("a", "on", &[reading]), ("a", "off", &[9e6])]);
            run.disk.insert(("a".into(), "on".into()), vec![kb]);
            run.disk.insert(("a".into(), "off".into()), vec![0.]);
            run
        };
        let base = [wrote(3e6, 1000.)];
        let rows = compare(&base, &[wrote(3e6, 1200.)], 0.01);
        assert_eq!(rows[0].disk.map(|disk| disk.verdict), Some(Verdict::Slower));
        let table = table(&rows, Unit::Instructions);
        assert!(
            table.contains("| 1.000 | 1.200 | **more disk** |"),
            "{table}"
        );
        // Less disk with more instructions: both are said.
        let rows = compare(&base, &[wrote(3.3e6, 500.)], 0.01);
        assert!(
            table_of(&rows).contains("| 0.500 | **slower**, less disk |"),
            "{}",
            table_of(&rows)
        );
        // With workers the disk moves from count to count: not judged.
        let varied = |kb: &[f64]| {
            let mut run = wrote(3e6, 0.);
            run.disk.insert(("a".into(), "on".into()), kb.to_vec());
            run
        };
        let rows = compare(
            &[varied(&[0., 240., 0.])],
            &[varied(&[208., 0., 216.])],
            0.01,
        );
        assert_eq!(rows[0].disk.map(|disk| disk.verdict), Some(Verdict::Same));
        assert!(
            table_of(&rows).contains("| new | disk varies |"),
            "{}",
            table_of(&rows)
        );
        let rows = compare(
            &[varied(&[24496., 28634., 26536.])],
            &[varied(&[26521., 28592., 26554.])],
            0.01,
        );
        assert!(
            table_of(&rows).contains("disk varies"),
            "{}",
            table_of(&rows)
        );
        // A spill the base did not have, and no column where none wrote.
        assert!(
            table_of(&compare(&[wrote(3e6, 0.)], &[wrote(3e6, 64.)], 0.01)).contains("| new |")
        );
        let none = compare(&[wrote(3e6, 0.)], &[wrote(3e6, 0.)], 0.01);
        assert_eq!(none[0].disk, None);
        assert!(!table_of(&none).contains("disk"));
    }

    fn table_of(rows: &[Row]) -> String {
        table(rows, Unit::Instructions)
    }
}

//! What a run shows: a table of the queries in the console, summary.md in
//! its directory, and the comparison of two runs, all from run.json.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use comfy_table::{Cell, CellAlignment, Color, ContentArrangement, Table, presets};
use console::style;

use crate::check::{QueryCheck, Verdict};
use crate::measure;
use crate::participation::QueryParticipation;
use crate::queries;
use crate::rundir::Run;
use crate::stats::{self, Outcome, Summary, Totals};

/// Reads run.json from a run's directory, or from the file named.
pub fn load(path: &Path) -> Result<(PathBuf, Run)> {
    let file = if path.is_dir() {
        path.join("run.json")
    } else {
        path.to_path_buf()
    };
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("cannot read {}", file.display()))?;
    let run =
        serde_json::from_str(&text).with_context(|| format!("{} is not a run", file.display()))?;
    Ok((file.parent().unwrap_or(Path::new(".")).to_path_buf(), run))
}

/// The verdict a reader needs first: wrong against the published answer,
/// else on against off.
fn status(check: &QueryCheck) -> &Verdict {
    match &check.reference {
        Some(reference) if *reference != Verdict::Same => reference,
        _ => &check.on_off,
    }
}

fn participation(run: &Run, query: u8) -> Option<&QueryParticipation> {
    run.participation.iter().find(|p| p.query == query)
}

fn with_tessera(run: &Run, query: u8) -> bool {
    participation(run, query)
        .and_then(|p| p.on.as_ref())
        .is_some_and(|on| on.tessera_nodes() > 0)
}

/// The run's settings in a line.
pub fn heading(run: &Run) -> String {
    let meta = &run.meta;
    let mut text = format!(
        "SF {}, schema {}, {}, work_mem {}, jit {}",
        meta.sf,
        meta.schema,
        if meta.workers == 0 {
            "serial".to_string()
        } else {
            format!("{} workers", meta.workers)
        },
        meta.work_mem,
        if meta.jit { "on" } else { "off" }
    );
    if meta.pairs > 0 {
        let _ = write!(
            text,
            ", {} pairs ABBA after {} warm-up",
            meta.pairs, meta.warmups
        );
    }
    let _ = write!(
        text,
        "; {} at {}, {}, {}",
        meta.head, meta.started, meta.postgres, meta.power
    );
    text
}

/// One row of the table, as text, with how its ratio came out.
struct Line {
    cells: [String; 8],
    outcome: Option<Outcome>,
    status: Verdict,
}

fn lines(run: &Run, summaries: &[Summary]) -> Vec<Line> {
    let threshold = run.meta.threshold;
    run.checks
        .iter()
        .map(|check| {
            let query = queries::get(check.query);
            let on = participation(run, check.query).and_then(|p| p.on.as_ref());
            let summary = summaries
                .iter()
                .find(|summary| summary.query == check.query);
            let failure = run
                .failures
                .iter()
                .find(|failure| failure.query == check.query);
            let status = match failure {
                Some(failure) => Verdict::Error(failure.reason.clone()),
                None => status(check).clone(),
            };
            let ms = |value: f64| format!("{value:.1}");
            Line {
                cells: [
                    format!("{} {}", query.name(), query.title),
                    status.label().to_string(),
                    on.map_or("-".into(), |on| on.tessera_nodes().to_string()),
                    on.map_or("-".into(), |on| format!("{:.0} %", on.share())),
                    summary.map_or("-".into(), |s| ms(s.off_median)),
                    summary.map_or("-".into(), |s| ms(s.on_median)),
                    summary.map_or("-".into(), |s| format!("{:.3}", s.ratio)),
                    summary.map_or("-".into(), |s| format!("{:.3}–{:.3}", s.low, s.high)),
                ],
                outcome: summary.map(|summary| summary.outcome(threshold)),
                status,
            }
        })
        .collect()
}

/// The columns shown: the check's four without timings, else all.
fn columns(summaries: &[Summary]) -> usize {
    if summaries.is_empty() {
        4
    } else {
        HEADER.len()
    }
}

const HEADER: [&str; 8] = [
    "query",
    "answer",
    "Tessera nodes",
    "rows via Tessera",
    "off, ms",
    "on, ms",
    "on/off",
    "95 % interval",
];

/// The table for the console: the ratio green where Tessera is faster by
/// more than the threshold, red where it is slower, statuses other than
/// `same` in red; no colour when the output is not a terminal.
pub fn table(run: &Run, summaries: &[Summary]) -> Table {
    let width = columns(summaries);
    let mut table = Table::new();
    table
        .load_style(presets::UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(&HEADER[..width]);
    for line in lines(run, summaries) {
        let mut cells: Vec<Cell> = line.cells[..width].iter().map(Cell::new).collect();
        for cell in cells.iter_mut().skip(2) {
            *cell = cell.clone().set_alignment(CellAlignment::Right);
        }
        if line.status != Verdict::Same {
            cells[1] = cells[1].clone().fg(Color::Red);
        }
        match line.outcome {
            Some(Outcome::Faster) if width > 6 => cells[6] = cells[6].clone().fg(Color::Green),
            Some(Outcome::Slower) if width > 6 => cells[6] = cells[6].clone().fg(Color::Red),
            _ => {}
        }
        table.add_row(cells);
    }
    table
}

/// `1 query`, `2 queries`.
fn query_count(count: usize) -> String {
    format!("{count} {}", if count == 1 { "query" } else { "queries" })
}

/// The run's totals in lines, or nothing for a run without timings.
pub fn totals(run: &Run, summaries: &[Summary]) -> Vec<String> {
    let checks = &run.checks;
    let same = checks
        .iter()
        .filter(|check| *status(check) == Verdict::Same)
        .count();
    let late: Vec<String> = checks
        .iter()
        .filter(|check| matches!(status(check), Verdict::Timeout(_)))
        .map(|check| queries::get(check.query).name())
        .collect();
    let mut answers = format!(
        "answers: {same} of {} the same{}",
        checks.len(),
        if checks.iter().any(|check| check.reference.is_some()) {
            " as the published ones and in both modes"
        } else {
            " in both modes (no published answers at this scale)"
        }
    );
    if !late.is_empty() {
        let _ = write!(answers, "; out of time: {}", late.join(", "));
    }
    let mut lines = vec![answers];
    if summaries.is_empty() {
        return lines;
    }
    let totals = Totals::new(
        summaries,
        |query| with_tessera(run, query),
        run.meta.threshold,
    );
    let mean = |value: Option<f64>| value.map_or("-".to_string(), |value| format!("{value:.3}"));
    lines.push(format!(
        "geometric mean of on/off: {} over {}, {} over the {} with Tessera nodes",
        mean(totals.geomean),
        query_count(totals.queries),
        mean(totals.geomean_tessera),
        totals.tessera
    ));
    lines.push(format!(
        "sum of the medians: off {:.2} s, on {:.2} s",
        totals.off_ms / 1e3,
        totals.on_ms / 1e3
    ));
    lines.push(format!(
        "beyond {} %: {} faster, {} slower; {} even",
        run.meta.threshold, totals.faster, totals.slower, totals.even
    ));
    lines
}

/// Prints a run: its settings, the table and the totals.
pub fn print(run: &Run) -> Result<()> {
    let summaries = measure::summaries(&run.samples, &run.failures)?;
    println!("{}", style(heading(run)).bold());
    println!("{}", table(run, &summaries));
    for line in totals(run, &summaries) {
        println!("{line}");
    }
    Ok(())
}

/// The disclaimer every published number carries.
pub const DISCLAIMER: &str = "Derived from TPC-H: the queries and data of the TPC Benchmark H, \
run with fixed validation parameters on data from tpchgen rather than DBGen, without the refresh \
functions and the throughput test. The results are not comparable to published TPC-H results.";

/// summary.md: the run in Markdown, for the run's directory.
pub fn markdown(run: &Run) -> Result<String> {
    let summaries = measure::summaries(&run.samples, &run.failures)?;
    let mut text = format!("# Queries derived from TPC-H, run {}\n\n", run.meta.id);
    let _ = writeln!(text, "{}\n", heading(run));
    let width = columns(&summaries);
    let _ = writeln!(text, "| {} |", HEADER[..width].join(" | "));
    let _ = writeln!(text, "|---|---|{}", "--:|".repeat(width - 2));
    for line in lines(run, &summaries) {
        let _ = writeln!(text, "| {} |", line.cells[..width].join(" | "));
    }
    text.push('\n');
    for line in totals(run, &summaries) {
        let _ = writeln!(text, "- {line}");
    }
    let _ = writeln!(
        text,
        "\nThe ratio is the median time with Tessera on over the median with it off: \
below one Tessera is faster. The interval is the 95 % bootstrap interval of the ratio \
over the pairs. Rows via Tessera is the share of the rows read from the tables that \
TessHeapScan read.\n\n{DISCLAIMER}"
    );
    Ok(text)
}

/// One query in two runs.
struct Compared {
    query: u8,
    a: Summary,
    b: Summary,
    /// Medians of B over A with Tessera on and off, and the interval of
    /// the first.
    on: f64,
    off: f64,
    low: f64,
    high: f64,
}

fn compared(a: &Run, b: &Run) -> Result<Vec<Compared>> {
    let (summaries_a, summaries_b) = (
        measure::summaries(&a.samples, &a.failures)?,
        measure::summaries(&b.samples, &b.failures)?,
    );
    let times = |run: &Run, query: u8, mode: &str| -> Vec<f64> {
        run.samples
            .iter()
            .filter(|sample| sample.query == query && sample.mode == mode)
            .map(|sample| sample.ms)
            .collect()
    };
    Ok(summaries_a
        .into_iter()
        .filter_map(|sa| {
            let sb = summaries_b.iter().find(|sb| sb.query == sa.query)?.clone();
            let (low, high) =
                stats::unpaired_interval(&times(a, sa.query, "on"), &times(b, sa.query, "on"));
            Some(Compared {
                query: sa.query,
                on: sb.on_median / sa.on_median,
                off: sb.off_median / sa.off_median,
                low,
                high,
                a: sa,
                b: sb,
            })
        })
        .collect())
}

/// The settings two runs must share to be compared.
fn settings(run: &Run) -> (&str, &str, u32, &str, bool) {
    let meta = &run.meta;
    (
        &meta.sf,
        &meta.schema,
        meta.workers,
        &meta.work_mem,
        meta.jit,
    )
}

/// Prints two runs side by side: Tessera on in B against A, which
/// compares two builds of Tessera, with off in B against A as the control
/// of the machine, since the core is the same in both.
pub fn compare(a: &Run, b: &Run) -> Result<()> {
    println!("{} {}", style("A").bold(), heading(a));
    println!("{} {}", style("B").bold(), heading(b));
    if settings(a) != settings(b) {
        println!(
            "{}",
            style("warning: the runs differ in scale, schema, workers, work_mem or jit").red()
        );
    }
    let rows = compared(a, b)?;
    let mut table = Table::new();
    table
        .load_style(presets::UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header([
            "query",
            "on A, ms",
            "on B, ms",
            "B/A on",
            "95 % interval",
            "B/A off",
            "on/off A",
            "on/off B",
        ]);
    let threshold = if b.meta.threshold > 0.0 {
        b.meta.threshold
    } else {
        3.0
    };
    for row in &rows {
        let mut ratio = Cell::new(format!("{:.3}", row.on)).set_alignment(CellAlignment::Right);
        if row.on < 1.0 - threshold / 100.0 {
            ratio = ratio.fg(Color::Green);
        } else if row.on > 1.0 + threshold / 100.0 {
            ratio = ratio.fg(Color::Red);
        }
        let right = |text: String| Cell::new(text).set_alignment(CellAlignment::Right);
        table.add_row(vec![
            Cell::new(format!(
                "{} {}",
                queries::get(row.query).name(),
                queries::get(row.query).title
            )),
            right(format!("{:.1}", row.a.on_median)),
            right(format!("{:.1}", row.b.on_median)),
            ratio,
            right(format!("{:.3}–{:.3}", row.low, row.high)),
            right(format!("{:.3}", row.off)),
            right(format!("{:.3}", row.a.ratio)),
            right(format!("{:.3}", row.b.ratio)),
        ]);
    }
    println!("{table}");
    let mean = |values: Vec<f64>| {
        stats::geometric_mean(values).map_or("-".to_string(), |v| format!("{v:.3}"))
    };
    println!(
        "geometric mean over {}: B/A on {}, B/A off {} (the control: the core is the same)",
        query_count(rows.len()),
        mean(rows.iter().map(|row| row.on).collect()),
        mean(rows.iter().map(|row| row.off).collect())
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::measure::Sample;
    use crate::participation::Participation;
    use crate::rundir::Meta;

    fn meta() -> Meta {
        Meta {
            id: "abcdef".into(),
            command: "run".into(),
            sf: "1".into(),
            schema: "pk".into(),
            workers: 0,
            work_mem: "256MB".into(),
            jit: false,
            timeout_s: 30,
            head: "0123456789ab".into(),
            postgres: "PostgreSQL 20devel".into(),
            started: "2026-10-02 08:00:00+07".into(),
            power: "AC Power".into(),
            pairs: 2,
            warmups: 1,
            threshold: 3.0,
        }
    }

    fn run(on: f64) -> Run {
        let check = |query| QueryCheck {
            query,
            rows: Some(1),
            reference: Some(Verdict::Same),
            on_off: Verdict::Same,
            off_ms: Some(100.0),
            on_ms: Some(on),
        };
        let mut nodes = std::collections::BTreeMap::new();
        nodes.insert("TessHeapScan".to_string(), 1);
        let sample = |query, pair, mode: &str, ms| Sample {
            query,
            pair,
            mode: mode.into(),
            position: 0,
            ms,
            rows: 1,
        };
        Run {
            meta: meta(),
            checks: vec![check(6), check(17)],
            participation: vec![QueryParticipation {
                query: 6,
                on: Some(Participation {
                    nodes,
                    plan_nodes: 3,
                    scanned: 100,
                    tessera_scanned: 100,
                    ..Participation::default()
                }),
                off: Some(Participation::default()),
            }],
            samples: vec![
                sample(6, 0, "on", on),
                sample(6, 0, "off", 100.0),
                sample(6, 1, "off", 100.0),
                sample(6, 1, "on", on),
            ],
            failures: Vec::new(),
        }
    }

    #[test]
    fn the_table_shows_status_participation_and_times() {
        let run = run(50.0);
        let summaries = measure::summaries(&run.samples, &run.failures).unwrap();
        let lines = lines(&run, &summaries);
        assert_eq!(
            lines[0].cells,
            [
                "Q06 forecasting revenue change",
                "same",
                "1",
                "100 %",
                "100.0",
                "50.0",
                "0.500",
                "0.500–0.500"
            ]
            .map(String::from)
        );
        assert_eq!(lines[0].outcome, Some(Outcome::Faster));
        assert_eq!(lines[1].cells[2..].to_vec(), vec!["-"; 6]);
        let check_only = Run {
            samples: Vec::new(),
            ..run.clone()
        };
        let text = table(&check_only, &[]).to_string();
        assert!(
            text.contains("rows via Tessera") && !text.contains("on/off"),
            "{text}"
        );
        let table = table(&run, &summaries).to_string();
        assert!(table.contains("Q06 forecasting revenue change"), "{table}");
        let totals = totals(&run, &summaries);
        assert_eq!(
            totals[0],
            "answers: 2 of 2 the same as the published ones and in both modes"
        );
        assert!(totals[1].starts_with("geometric mean of on/off: 0.500 over 1 query,"));
    }

    #[test]
    fn markdown_carries_the_table_and_the_disclaimer() {
        let text = markdown(&run(50.0)).unwrap();
        assert!(text.starts_with("# Queries derived from TPC-H, run abcdef\n"));
        assert!(text.contains(
            "| Q06 forecasting revenue change | same | 1 | 100 % | 100.0 | 50.0 | 0.500 |"
        ));
        assert!(text.contains("not comparable to published TPC-H results"));
    }

    #[test]
    fn runs_compare_query_by_query() {
        let rows = compared(&run(50.0), &run(40.0)).unwrap();
        assert_eq!(rows.len(), 1);
        assert!((rows[0].on - 0.8).abs() < 1e-12);
        assert!((rows[0].off - 1.0).abs() < 1e-12);
    }

    #[test]
    fn runs_load_from_their_directory() {
        let dir = tempfile::tempdir().unwrap();
        let run = run(50.0);
        std::fs::write(
            dir.path().join("run.json"),
            serde_json::to_string(&run).unwrap(),
        )
        .unwrap();
        let (path, loaded) = load(dir.path()).unwrap();
        assert_eq!(path, dir.path());
        assert_eq!(loaded.meta, run.meta);
        assert!(load(&dir.path().join("none")).is_err());
    }
}

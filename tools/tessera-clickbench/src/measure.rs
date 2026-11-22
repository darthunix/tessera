//! The timing of a run: each query a few times in one mode, as ClickBench
//! times it, every try on a new connection and timed from sending the
//! query to its last row; the run under a deadline, which also bounds
//! each try's statement_timeout, so that a query that does not finish in
//! time is reported as such, untimed.
//!
//! A counting run reads instead the instructions and cycles pg_pmu
//! counts while a function of `bench/clickbench/count.sql` runs the
//! query, its planning, execution and parallel workers and DuckDB's
//! threads included: each query on one connection, once to warm it up,
//! DuckDB's instance started, then a try a reading.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cluster::Cluster;
use crate::queries::Query;
use crate::session::{Mode, Outcome, Session, Settings};

/// A try that ran to its end has its seconds; any other, how it ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Try {
    pub seconds: Option<f64>,
    /// `ok`, `timeout`, `error: …`, or `deadline` for a try the deadline
    /// left no time for.
    pub outcome: String,
    pub rows: Option<usize>,
    /// What pg_pmu counted in a counting run: the execution's
    /// instructions and cycles, and the cycles of its parallel workers,
    /// which are among them.
    #[serde(default)]
    pub instructions: Option<u64>,
    #[serde(default)]
    pub cycles: Option<u64>,
    #[serde(default)]
    pub workers_cycles: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryTimes {
    pub query: usize,
    pub tries: Vec<Try>,
}

impl QueryTimes {
    /// The hot time, as ClickBench reads it: the least of the tries after
    /// the first, or the first when it is the only one. None when one of
    /// them did not finish.
    pub fn hot(&self) -> Option<f64> {
        let hot = if self.tries.len() > 1 {
            &self.tries[1..]
        } else {
            &self.tries[..]
        };
        hot.iter()
            .map(|one| one.seconds)
            .collect::<Option<Vec<f64>>>()
            .and_then(|seconds| seconds.into_iter().reduce(f64::min))
    }

    /// The median of a count over the tries, None when one of them did
    /// not count; of an even number, the lower of the middle two.
    pub fn median(&self, count: impl Fn(&Try) -> Option<u64>) -> Option<u64> {
        let mut counts = self.tries.iter().map(count).collect::<Option<Vec<u64>>>()?;
        counts.sort_unstable();
        counts.get(counts.len().saturating_sub(1) / 2).copied()
    }
}

/// What a run times.
pub struct Plan<'a> {
    pub mode: Mode,
    pub settings: &'a Settings,
    pub tries: u32,
    /// A try's statement_timeout, at most what is left of the deadline.
    pub timeout: Duration,
    /// The run's end.
    pub deadline: Instant,
    /// Whether the run counts with pg_pmu instead of timing.
    pub counters: bool,
}

/// Times `queries`, calling `each` after each query.
pub fn run(
    cluster: &Cluster,
    plan: &Plan<'_>,
    queries: &[&Query],
    mut each: impl FnMut(&Query, &QueryTimes),
) -> Vec<QueryTimes> {
    let mut all = Vec::new();
    for query in queries {
        let mut tries = Vec::new();
        if plan.counters {
            tries = counted(cluster, plan, query);
        } else {
            for _ in 0..plan.tries {
                tries.push(one_try(cluster, plan, query));
            }
        }
        let times = QueryTimes {
            query: query.number,
            tries,
        };
        each(query, &times);
        all.push(times);
    }
    all
}

/// What is left of the deadline, or None below a second.
fn left(deadline: Instant) -> Option<Duration> {
    Some(deadline.saturating_duration_since(Instant::now())).filter(|left| left.as_secs() >= 1)
}

fn passed() -> Try {
    failed("deadline".into())
}

fn failed(outcome: String) -> Try {
    Try {
        seconds: None,
        outcome,
        rows: None,
        instructions: None,
        cycles: None,
        workers_cycles: None,
    }
}

/// The statement that counts `query`: pg_pmu's EXPLAIN around the
/// function that reads it, a cursor for pg_duckdb, an EXECUTE, which may
/// have parallel workers, for Tessera.
pub fn counting_statement(mode: Mode, query: &Query) -> String {
    let function = match mode {
        Mode::Duckdb => "bench_rows",
        Mode::On | Mode::Off => "bench_exec",
    };
    format!(
        "EXPLAIN (ANALYZE, PMU, VERBOSE, TIMING off, BUFFERS off, SUMMARY off, FORMAT JSON) \
         SELECT public.{function}($q${}$q$)",
        query.text
    )
}

/// A reading of what pg_pmu printed in EXPLAIN's JSON.
pub fn reading(json: &str, seconds: f64) -> Try {
    let value: serde_json::Value = match serde_json::from_str(json) {
        Ok(value) => value,
        Err(error) => return failed(format!("error: EXPLAIN's JSON: {error}")),
    };
    let count = |name: &str| value[0][name].as_u64();
    match (count("Execution Instructions"), count("Execution Cycles")) {
        (Some(instructions), Some(cycles)) => Try {
            seconds: Some(seconds),
            outcome: "ok".into(),
            rows: None,
            instructions: Some(instructions),
            cycles: Some(cycles),
            workers_cycles: count("Workers Cycles"),
        },
        _ => failed("error: pg_pmu printed no execution counts".into()),
    }
}

/// The readings of a query: `next` gives one, the first warming the
/// connection up and not kept. A warm-up that gave no reading fails every
/// try with its outcome, so that no reading runs unwarmed.
fn readings(tries: u32, mut next: impl FnMut() -> Try) -> Vec<Try> {
    let warm = next();
    if warm.outcome != "ok" {
        let outcome = if warm.outcome == "deadline" {
            warm.outcome
        } else {
            format!("warm-up: {}", warm.outcome)
        };
        return (0..tries).map(|_| failed(outcome.clone())).collect();
    }
    (0..tries).map(|_| next()).collect()
}

/// The readings of a query on one connection, after one that warms it up.
fn counted(cluster: &Cluster, plan: &Plan<'_>, query: &Query) -> Vec<Try> {
    let statement = counting_statement(plan.mode, query);
    let Some(left_now) = left(plan.deadline) else {
        return (0..plan.tries).map(|_| passed()).collect();
    };
    let mut session = match Session::open(
        cluster,
        plan.settings,
        plan.mode,
        plan.timeout.min(left_now),
    ) {
        Ok(session) => session,
        Err(error) => {
            let outcome = format!("error: {error:#}");
            return (0..plan.tries).map(|_| failed(outcome.clone())).collect();
        }
    };
    readings(plan.tries, || {
        let Some(left_now) = left(plan.deadline) else {
            return passed();
        };
        if let Err(error) = session.set_timeout(plan.timeout.min(left_now)) {
            return failed(format!("error: {error:#}"));
        }
        let execution = session.run(&statement);
        match execution.outcome {
            Outcome::Rows(rows) => {
                let json = rows
                    .first()
                    .and_then(|row| row[0].clone())
                    .unwrap_or_default();
                reading(&json, execution.elapsed.as_secs_f64())
            }
            Outcome::Timeout => failed("timeout".into()),
            Outcome::Error(error) => failed(format!("error: {error}")),
        }
    })
}

/// A try on a connection of its own, its statement_timeout set again
/// once the connection is made, from what is left then.
fn one_try(cluster: &Cluster, plan: &Plan<'_>, query: &Query) -> Try {
    let Some(left_now) = left(plan.deadline) else {
        return passed();
    };
    let opened = Session::open(
        cluster,
        plan.settings,
        plan.mode,
        plan.timeout.min(left_now),
    );
    let mut session = match opened {
        Ok(session) => session,
        Err(error) => return failed(format!("error: {error:#}")),
    };
    let Some(left_now) = left(plan.deadline) else {
        return passed();
    };
    if let Err(error) = session.set_timeout(plan.timeout.min(left_now)) {
        return failed(format!("error: {error:#}"));
    }
    let execution = session.run(&query.text);
    match execution.outcome {
        Outcome::Rows(rows) => Try {
            rows: Some(rows.len()),
            seconds: Some(execution.elapsed.as_secs_f64()),
            outcome: "ok".into(),
            ..failed(String::new())
        },
        Outcome::Timeout => failed("timeout".into()),
        Outcome::Error(error) => failed(format!("error: {error}")),
    }
}

/// ClickBench's mean of times: the geometric mean of each time plus
/// 10 ms, so that a query of a millisecond does not decide it.
pub fn geometric_mean(seconds: &[f64]) -> Option<f64> {
    if seconds.is_empty() {
        return None;
    }
    let sum: f64 = seconds.iter().map(|time| (time + 0.01).ln()).sum();
    Some((sum / seconds.len() as f64).exp())
}

/// times.csv: `query,try,seconds,instructions,cycles,outcome`, a value
/// empty where the try did not give it.
pub fn csv(times: &[QueryTimes]) -> String {
    let mut text = String::from("query,try,seconds,instructions,cycles,outcome\n");
    let count = |count: Option<u64>| count.map_or(String::new(), |count| count.to_string());
    for query in times {
        for (index, one) in query.tries.iter().enumerate() {
            let _ = writeln!(
                text,
                "{},{},{},{},{},{}",
                query.query,
                index + 1,
                one.seconds
                    .map_or(String::new(), |seconds| format!("{seconds:.6}")),
                count(one.instructions),
                count(one.cycles),
                one.outcome.replace(['\n', ','], " ")
            );
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tried(seconds: &[Option<f64>]) -> QueryTimes {
        QueryTimes {
            query: 1,
            tries: seconds
                .iter()
                .map(|&seconds| Try {
                    seconds,
                    outcome: if seconds.is_some() { "ok" } else { "timeout" }.into(),
                    rows: seconds.map(|_| 1),
                    ..failed(String::new())
                })
                .collect(),
        }
    }

    #[test]
    fn the_hot_time_is_the_least_after_the_first() {
        assert_eq!(tried(&[Some(3.0), Some(2.0), Some(1.5)]).hot(), Some(1.5));
        assert_eq!(tried(&[Some(1.0), Some(2.0), Some(2.5)]).hot(), Some(2.0));
        assert_eq!(tried(&[Some(4.0)]).hot(), Some(4.0));
        assert_eq!(tried(&[Some(1.0), None, Some(2.0)]).hot(), None);
        assert_eq!(tried(&[]).hot(), None);
    }

    #[test]
    fn the_mean_adds_ten_milliseconds() {
        assert_eq!(geometric_mean(&[]), None);
        let mean = geometric_mean(&[0.09, 0.99]).unwrap();
        assert!((mean - (0.1f64 * 1.0).sqrt()).abs() < 1e-12, "{mean}");
    }

    #[test]
    fn the_csv_leaves_unfinished_tries_empty() {
        let text = csv(&[tried(&[Some(0.5), None])]);
        assert_eq!(
            text,
            "query,try,seconds,instructions,cycles,outcome\n1,1,0.500000,,,ok\n1,2,,,,timeout\n"
        );
    }

    #[test]
    fn a_reading_is_what_pg_pmu_printed() {
        let one = reading(
            r#"[{"Plan": {}, "Execution Instructions": 1200, "Execution Cycles": 900,
                 "Workers Cycles": 300}]"#,
            0.5,
        );
        assert_eq!(
            (one.instructions, one.cycles, one.workers_cycles),
            (Some(1200), Some(900), Some(300))
        );
        assert_eq!(one.outcome, "ok");
        let serial = reading(
            r#"[{"Execution Instructions": 5, "Execution Cycles": 7}]"#,
            0.1,
        );
        assert_eq!(serial.workers_cycles, None);
        assert!(
            reading(r#"[{"Plan": {}}]"#, 0.1)
                .outcome
                .starts_with("error")
        );
        assert!(reading("not json", 0.1).outcome.starts_with("error"));
    }

    #[test]
    fn a_warm_up_that_fails_fails_every_reading() {
        let ok = |cycles| Try {
            outcome: "ok".into(),
            cycles: Some(cycles),
            ..failed(String::new())
        };
        let mut given = vec![ok(9), ok(1), ok(2)].into_iter();
        let kept = readings(2, || given.next().unwrap());
        assert_eq!(
            kept.iter().map(|one| one.cycles).collect::<Vec<_>>(),
            vec![Some(1), Some(2)]
        );
        let mut given = vec![failed("timeout".into()), ok(1)].into_iter();
        let kept = readings(2, || given.next().unwrap());
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|one| one.outcome == "warm-up: timeout"));
        let kept = readings(3, passed);
        assert!(kept.iter().all(|one| one.outcome == "deadline"));
        // A reading that fails after the warm-up is kept as it is.
        let mut given = vec![ok(9), failed("error: x".into()), ok(3)].into_iter();
        let kept = readings(2, || given.next().unwrap());
        assert_eq!(kept[0].outcome, "error: x");
        assert_eq!(kept[1].cycles, Some(3));
    }

    #[test]
    fn the_median_of_counts() {
        let counted = |cycles: &[Option<u64>]| QueryTimes {
            query: 1,
            tries: cycles
                .iter()
                .map(|&cycles| Try {
                    cycles,
                    ..failed(String::new())
                })
                .collect(),
        };
        let cycles = |one: &Try| one.cycles;
        assert_eq!(
            counted(&[Some(5), Some(1), Some(3)]).median(cycles),
            Some(3)
        );
        assert_eq!(counted(&[Some(4), Some(2)]).median(cycles), Some(2));
        assert_eq!(counted(&[Some(4), None]).median(cycles), None);
        assert_eq!(counted(&[]).median(cycles), None);
    }

    #[test]
    fn duckdb_is_read_through_a_cursor_and_tessera_through_execute() {
        let query =
            crate::queries::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
                .unwrap()
                .remove(28);
        let duckdb = counting_statement(Mode::Duckdb, &query);
        assert!(duckdb.starts_with("EXPLAIN (ANALYZE, PMU, VERBOSE"));
        assert!(duckdb.contains("SELECT public.bench_rows($q$SELECT REGEXP_REPLACE"));
        assert!(duckdb.ends_with("LIMIT 25$q$)"));
        assert!(counting_statement(Mode::On, &query).contains("public.bench_exec($q$"));
    }
}

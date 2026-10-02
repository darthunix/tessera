//! The timing: the queries that answered the same in both modes, through
//! the connections and prepared statements the check used, with the data
//! in shared buffers, in pairs of one execution per mode whose order
//! alternates (on, off, then off, on: ABBA), so that a drift of the
//! machine, its heat or its power, falls on both modes alike.

use std::fmt::Write as _;

use anyhow::{Result, bail};
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};

use crate::check::{QueryCheck, Sessions, Verdict, statement};
use crate::queries;
use crate::session::{Outcome, Session};
use crate::stats::Summary;

/// One timed execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub query: u8,
    pub pair: u32,
    /// `on` or `off`.
    pub mode: String,
    /// 0 for the first execution of the pair, 1 for the second.
    pub position: u8,
    pub ms: f64,
    pub rows: usize,
}

/// How many executions and in which order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plan {
    pub warmups: u32,
    pub pairs: u32,
}

impl Plan {
    /// The modes of a pair in order: on first in the even pairs.
    pub fn order(pair: u32) -> [bool; 2] {
        if pair.is_multiple_of(2) {
            [true, false]
        } else {
            [false, true]
        }
    }

    /// Roughly how long the timing takes, from the times of the check.
    pub fn estimate_seconds(&self, checks: &[QueryCheck]) -> f64 {
        let executions = f64::from(self.warmups + self.pairs);
        checks
            .iter()
            .filter(|check| timeable(check))
            .map(|check| {
                (check.off_ms.unwrap_or(0.0) + check.on_ms.unwrap_or(0.0)) * executions / 1e3
            })
            .sum()
    }
}

/// Whether the timing runs a query: the same answer in both modes.
pub fn timeable(check: &QueryCheck) -> bool {
    check.on_off == Verdict::Same && !check.failed()
}

/// Reads every table and index into shared buffers and returns the share
/// of their blocks there, in percent. pg_prewarm reads past the ring
/// buffer that a sequential scan of a table larger than a quarter of
/// shared buffers would use, which keeps such a table out of them.
pub fn prewarm(client: &mut postgres::Client) -> Result<(f64, i64)> {
    client.batch_execute(
        "SELECT pg_prewarm(c.oid) FROM pg_class c
          WHERE c.relnamespace = 'public'::regnamespace AND c.relkind IN ('r', 'i')",
    )?;
    let row = client.query_one(
        "WITH relations AS (
             SELECT pg_relation_filenode(c.oid) AS filenode,
                    pg_relation_size(c.oid) / current_setting('block_size')::int8 AS blocks
               FROM pg_class c
              WHERE c.relnamespace = 'public'::regnamespace AND c.relkind IN ('r', 'i'))
         SELECT sum(blocks)::int8,
                (SELECT count(*) FROM pg_buffercache b JOIN relations r ON b.relfilenode = r.filenode
                  WHERE b.reldatabase = (SELECT oid FROM pg_database WHERE datname = current_database())
                    AND b.relforknumber = 0)::int8,
                sum(blocks)::int8 * current_setting('block_size')::int8
           FROM relations",
        &[],
    )?;
    let (blocks, cached, bytes): (i64, i64, i64) = (row.get(0), row.get(1), row.get(2));
    Ok((100.0 * cached as f64 / blocks.max(1) as f64, bytes))
}

/// Times one execution; the error says what went wrong.
fn time(session: &mut Session<'_>, number: u8) -> Result<(f64, usize), String> {
    let execution = session.execute(&statement(number));
    match execution.outcome {
        Outcome::Rows(rows) => Ok((execution.elapsed.as_secs_f64() * 1e3, rows.len())),
        Outcome::Timeout => Err("ran out of time".into()),
        Outcome::Error(error) => Err(error),
    }
}

/// A query's timing that went wrong.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub query: u8,
    pub reason: String,
}

/// Times the queries the check found timeable, and reports each as it is
/// done.
pub fn run(
    sessions: &mut Sessions<'_>,
    checks: &[QueryCheck],
    plan: Plan,
    mut done: impl FnMut(&Summary),
) -> Result<(Vec<Sample>, Vec<Failure>)> {
    let timed: Vec<&QueryCheck> = checks.iter().filter(|check| timeable(check)).collect();
    let bar = ProgressBar::new(timed.len() as u64 * u64::from(plan.warmups + plan.pairs) * 2);
    bar.set_style(ProgressStyle::with_template(
        "{prefix:>4} [{bar:30}] {pos}/{len} executions, {elapsed} of about {duration}",
    )?);
    let mut samples = Vec::new();
    let mut failures = Vec::new();
    'queries: for check in timed {
        let number = check.query;
        let name = queries::get(number).name();
        bar.set_prefix(name.clone());
        let mut pairs = Vec::new();
        let fail = |reason: String| Failure {
            query: number,
            reason: format!("{name}: {reason}"),
        };
        for _ in 0..plan.warmups {
            for tessera in [true, false] {
                let session = if tessera {
                    &mut sessions.on
                } else {
                    &mut sessions.off
                };
                if let Err(error) = time(session, number) {
                    failures.push(fail(error));
                    continue 'queries;
                }
                bar.inc(1);
            }
        }
        for pair in 0..plan.pairs {
            let mut times = [0.0; 2];
            for (position, tessera) in Plan::order(pair).into_iter().enumerate() {
                let session = if tessera {
                    &mut sessions.on
                } else {
                    &mut sessions.off
                };
                let (ms, rows) = match time(session, number) {
                    Ok(time) => time,
                    Err(error) => {
                        failures.push(fail(error));
                        continue 'queries;
                    }
                };
                if Some(rows) != check.rows {
                    failures.push(fail(format!("{rows} rows, the check had {:?}", check.rows)));
                    continue 'queries;
                }
                times[usize::from(!tessera)] = ms;
                samples.push(Sample {
                    query: number,
                    pair,
                    mode: if tessera { "on" } else { "off" }.into(),
                    position: position as u8,
                    ms,
                    rows,
                });
                bar.inc(1);
            }
            pairs.push((times[0], times[1]));
        }
        bar.suspend(|| done(&Summary::new(number, &pairs)));
    }
    bar.finish_and_clear();
    Ok((samples, failures))
}

/// The pairs `(on, off)` of a query's samples, in their order.
pub fn pairs(samples: &[Sample], query: u8) -> Result<Vec<(f64, f64)>> {
    let mut pairs: Vec<(Option<f64>, Option<f64>)> = Vec::new();
    for sample in samples.iter().filter(|sample| sample.query == query) {
        let index = sample.pair as usize;
        if pairs.len() <= index {
            pairs.resize(index + 1, (None, None));
        }
        match sample.mode.as_str() {
            "on" => pairs[index].0 = Some(sample.ms),
            "off" => pairs[index].1 = Some(sample.ms),
            mode => bail!("a sample of mode {mode:?}"),
        }
    }
    Ok(pairs
        .into_iter()
        .filter_map(|(on, off)| Some((on?, off?)))
        .collect())
}

/// The summary of every query timed without a failure, in the order of
/// the samples.
pub fn summaries(samples: &[Sample], failures: &[Failure]) -> Result<Vec<Summary>> {
    let mut queries: Vec<u8> = samples.iter().map(|sample| sample.query).collect();
    queries.dedup();
    queries
        .into_iter()
        .filter(|query| failures.iter().all(|failure| failure.query != *query))
        .map(|query| Ok(Summary::new(query, &pairs(samples, query)?)))
        .collect()
}

/// timings.csv: every timed execution.
pub fn csv(samples: &[Sample]) -> String {
    let mut text = String::from("query,pair,mode,position,ms,rows\n");
    for sample in samples {
        let _ = writeln!(
            text,
            "{},{},{},{},{:.3},{}",
            queries::get(sample.query).name(),
            sample.pair,
            sample.mode,
            sample.position,
            sample.ms,
            sample.rows
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_alternate_their_order() {
        assert_eq!(Plan::order(0), [true, false]);
        assert_eq!(Plan::order(1), [false, true]);
        assert_eq!(Plan::order(2), [true, false]);
    }

    fn check(query: u8, on_off: Verdict, off_ms: f64, on_ms: f64) -> QueryCheck {
        QueryCheck {
            query,
            rows: Some(1),
            reference: Some(Verdict::Same),
            on_off,
            off_ms: Some(off_ms),
            on_ms: Some(on_ms),
        }
    }

    #[test]
    fn only_the_same_answers_are_timed() {
        let checks = [
            check(1, Verdict::Same, 1000.0, 500.0),
            check(2, Verdict::Mismatch("row 1".into()), 1000.0, 500.0),
            check(3, Verdict::Timeout("on and off".into()), 1000.0, 500.0),
        ];
        assert!(timeable(&checks[0]));
        assert!(!timeable(&checks[1]) && !timeable(&checks[2]));
        let plan = Plan {
            warmups: 1,
            pairs: 11,
        };
        assert_eq!(plan.estimate_seconds(&checks), 18.0);
    }

    #[test]
    fn samples_make_pairs_and_csv() {
        let sample = |pair, mode: &str, position, ms| Sample {
            query: 6,
            pair,
            mode: mode.into(),
            position,
            ms,
            rows: 1,
        };
        let samples = [
            sample(0, "on", 0, 50.0),
            sample(0, "off", 1, 100.0),
            sample(1, "off", 0, 101.0),
            sample(1, "on", 1, 51.0),
            sample(2, "on", 0, 52.0),
        ];
        assert_eq!(
            pairs(&samples, 6).unwrap(),
            vec![(50.0, 100.0), (51.0, 101.0)]
        );
        assert!(pairs(&samples, 1).unwrap().is_empty());
        let summaries = summaries(&samples, &[]).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].pairs, 2);
        let failed = Failure {
            query: 6,
            reason: "Q06: ran out of time".into(),
        };
        assert!(super::summaries(&samples, &[failed]).unwrap().is_empty());
        let text = csv(&samples[..2]);
        assert_eq!(
            text,
            "query,pair,mode,position,ms,rows\nQ06,0,on,0,50.000,1\nQ06,0,off,1,100.000,1\n"
        );
    }
}

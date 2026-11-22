//! The timing of a run: each query a few times in one mode, as ClickBench
//! times it, every try on a new connection and timed from sending the
//! query to its last row; the run under a deadline, which also bounds
//! each try's statement_timeout, so that a query that does not finish in
//! time is reported as such, untimed.

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
        for _ in 0..plan.tries {
            tries.push(one_try(cluster, plan, query));
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
    Try {
        seconds: None,
        outcome: "deadline".into(),
        rows: None,
    }
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
        Err(error) => {
            return Try {
                seconds: None,
                outcome: format!("error: {error:#}"),
                rows: None,
            };
        }
    };
    let Some(left_now) = left(plan.deadline) else {
        return passed();
    };
    if let Err(error) = session.set_timeout(plan.timeout.min(left_now)) {
        return Try {
            seconds: None,
            outcome: format!("error: {error:#}"),
            rows: None,
        };
    }
    let execution = session.run(&query.text);
    match execution.outcome {
        Outcome::Rows(rows) => Try {
            seconds: Some(execution.elapsed.as_secs_f64()),
            outcome: "ok".into(),
            rows: Some(rows.len()),
        },
        Outcome::Timeout => Try {
            seconds: None,
            outcome: "timeout".into(),
            rows: None,
        },
        Outcome::Error(error) => Try {
            seconds: None,
            outcome: format!("error: {error}"),
            rows: None,
        },
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

/// times.csv: `query,try,seconds,outcome`, seconds empty where the try
/// did not finish.
pub fn csv(times: &[QueryTimes]) -> String {
    let mut text = String::from("query,try,seconds,outcome\n");
    for query in times {
        for (index, one) in query.tries.iter().enumerate() {
            let _ = writeln!(
                text,
                "{},{},{},{}",
                query.query,
                index + 1,
                one.seconds
                    .map_or(String::new(), |seconds| format!("{seconds:.6}")),
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
            "query,try,seconds,outcome\n1,1,0.500000,ok\n1,2,,timeout\n"
        );
    }
}

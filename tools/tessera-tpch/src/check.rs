//! The check: every query once in each mode, from the same prepared
//! statements the timing runs. Tessera off is compared with the published
//! answer (at SF 1, the only scale it exists for), Tessera on with off.
//! Until the answers agree, a time means nothing.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::answers;
use crate::compare::{Precision, compare};
use crate::config::Scale;
use crate::queries;
use crate::session::{Execution, Outcome, Session};

/// The result of a comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "detail", rename_all = "lowercase")]
pub enum Verdict {
    Same,
    /// The first difference.
    Mismatch(String),
    /// The mode and the server's error.
    Error(String),
    /// Which modes exceeded statement_timeout.
    Timeout(String),
}

impl Verdict {
    /// As results.txt and the table print it.
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Same => "same",
            Verdict::Mismatch(_) => "MISMATCH",
            Verdict::Error(_) => "ERROR",
            Verdict::Timeout(_) => "TIMEOUT",
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Verdict::Same => None,
            Verdict::Mismatch(detail) | Verdict::Error(detail) | Verdict::Timeout(detail) => {
                Some(detail)
            }
        }
    }
}

/// A query's check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryCheck {
    pub query: u8,
    /// The rows of the answer, of off where it has one.
    pub rows: Option<usize>,
    /// Tessera off against the published answer; none at a scale without
    /// one.
    pub reference: Option<Verdict>,
    /// Tessera on against off.
    pub on_off: Verdict,
    /// The time of the one execution in each mode, cold or nearly so.
    pub off_ms: Option<f64>,
    pub on_ms: Option<f64>,
}

impl QueryCheck {
    /// Whether the query answered rightly in both modes; a time limit
    /// exceeded is not a wrong answer, but leaves the query untimed.
    pub fn failed(&self) -> bool {
        let wrong = |verdict: &Verdict| matches!(verdict, Verdict::Mismatch(_) | Verdict::Error(_));
        wrong(&self.on_off) || self.reference.as_ref().is_some_and(wrong)
    }

    /// The first thing worth reading about it.
    pub fn detail(&self) -> Option<String> {
        let reference = self
            .reference
            .as_ref()
            .and_then(Verdict::detail)
            .map(|detail| format!("reference: {detail}"));
        reference.or_else(|| {
            self.on_off
                .detail()
                .map(|detail| format!("on/off: {detail}"))
        })
    }
}

/// The two connections of a run.
pub struct Sessions<'c> {
    pub off: Session<'c>,
    pub on: Session<'c>,
}

/// The name of a query's prepared statement.
pub fn statement(number: u8) -> String {
    format!("q{number:02}")
}

/// Prepares the selected queries in both connections and checks them one
/// by one, handing each result to `progress` as it comes.
pub fn run(
    sessions: &mut Sessions<'_>,
    root: &Path,
    scale: &Scale,
    selection: &[u8],
    mut progress: impl FnMut(&QueryCheck),
) -> Result<Vec<QueryCheck>> {
    let mut checks = Vec::new();
    for &number in selection {
        let query = queries::get(number);
        let text = query.text(root, scale)?;
        let name = statement(number);
        let prepared = sessions
            .off
            .prepare(&name, &text)
            .and_then(|()| sessions.on.prepare(&name, &text));
        let check = match prepared {
            Err(error) => QueryCheck {
                query: number,
                rows: None,
                reference: None,
                on_off: Verdict::Error(format!("PREPARE: {error}")),
                off_ms: None,
                on_ms: None,
            },
            Ok(()) => {
                let off = sessions.off.execute(&name);
                let on = sessions.on.execute(&name);
                judge(number, scale, &off, &on)
            }
        };
        progress(&check);
        checks.push(check);
    }
    Ok(checks)
}

/// The verdicts of one query's executions.
fn judge(number: u8, scale: &Scale, off: &Execution, on: &Execution) -> QueryCheck {
    let shape = queries::get(number).shape();
    let reference = (scale.factor() == 1.0)
        .then(|| answers::sf1(number))
        .flatten()
        .map(|answer| match &off.outcome {
            Outcome::Rows(rows) => verdict(compare(
                &answer,
                &answers::as_published(number, rows),
                &shape,
                Precision::Specification,
            )),
            Outcome::Timeout => Verdict::Timeout("off".into()),
            Outcome::Error(error) => Verdict::Error(format!("off: {error}")),
        });
    let on_off = match (&off.outcome, &on.outcome) {
        (Outcome::Error(error), _) => Verdict::Error(format!("off: {error}")),
        (_, Outcome::Error(error)) => Verdict::Error(format!("on: {error}")),
        (Outcome::Timeout, Outcome::Timeout) => Verdict::Timeout("on and off".into()),
        (Outcome::Timeout, _) => Verdict::Timeout("off".into()),
        (_, Outcome::Timeout) => Verdict::Timeout("on".into()),
        (Outcome::Rows(off), Outcome::Rows(on)) => {
            verdict(compare(off, on, &shape, Precision::Exact))
        }
    };
    let rows = |execution: &Execution| match &execution.outcome {
        Outcome::Rows(rows) => Some(rows.len()),
        _ => None,
    };
    let ms = |execution: &Execution| {
        matches!(execution.outcome, Outcome::Rows(_)).then(|| execution.elapsed.as_secs_f64() * 1e3)
    };
    QueryCheck {
        query: number,
        rows: rows(off).or_else(|| rows(on)),
        reference,
        on_off,
        off_ms: ms(off),
        on_ms: ms(on),
    }
}

fn verdict(result: Result<(), String>) -> Verdict {
    match result {
        Ok(()) => Verdict::Same,
        Err(difference) => Verdict::Mismatch(difference),
    }
}

/// results.txt: a line per query.
pub fn results(checks: &[QueryCheck], heading: &str) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "# {heading}");
    let _ = writeln!(
        text,
        "# reference: the published answer at SF 1 against Tessera off, by the precision of clause 2.1.3.5"
    );
    let _ = writeln!(
        text,
        "# on/off: Tessera on against off, value for value; rows equal in the sort keys in any order"
    );
    let _ = writeln!(
        text,
        "{:<5} {:>6}  {:<9} {:<9} {:>10} {:>10}  detail",
        "query", "rows", "reference", "on/off", "off ms", "on ms"
    );
    let number = |value: Option<f64>| value.map_or("-".to_string(), |value| format!("{value:.1}"));
    for check in checks {
        let _ = writeln!(
            text,
            "{:<5} {:>6}  {:<9} {:<9} {:>10} {:>10}  {}",
            queries::get(check.query).name(),
            check.rows.map_or("-".to_string(), |rows| rows.to_string()),
            check.reference.as_ref().map_or("-", Verdict::label),
            check.on_off.label(),
            number(check.off_ms),
            number(check.on_ms),
            check.detail().unwrap_or_default()
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn execution(outcome: Outcome) -> Execution {
        Execution {
            outcome,
            elapsed: Duration::from_millis(1500),
        }
    }

    fn rows(values: &[&str]) -> Outcome {
        Outcome::Rows(
            values
                .iter()
                .map(|value| vec![Some(value.to_string())])
                .collect(),
        )
    }

    #[test]
    fn verdicts_of_the_modes() {
        let sf10 = Scale::parse("10").unwrap();
        let same = judge(
            6,
            &sf10,
            &execution(rows(&["1.00"])),
            &execution(rows(&["1.00"])),
        );
        assert_eq!(same.on_off, Verdict::Same);
        assert_eq!(same.reference, None);
        assert_eq!(same.rows, Some(1));
        assert_eq!(same.off_ms, Some(1500.0));

        let differs = judge(
            6,
            &sf10,
            &execution(rows(&["1.00"])),
            &execution(rows(&["1.0"])),
        );
        assert_eq!(differs.on_off.label(), "MISMATCH");
        assert!(differs.failed());

        let late = judge(
            6,
            &sf10,
            &execution(Outcome::Timeout),
            &execution(rows(&["1"])),
        );
        assert_eq!(late.on_off, Verdict::Timeout("off".into()));
        assert_eq!(late.off_ms, None);
        assert!(!late.failed());

        let broken = judge(
            6,
            &sf10,
            &execution(Outcome::Timeout),
            &execution(Outcome::Error("XX000: oops".into())),
        );
        assert_eq!(broken.on_off, Verdict::Error("on: XX000: oops".into()));
        assert!(broken.failed());
    }

    #[test]
    fn at_sf1_off_is_held_to_the_published_answer() {
        let sf1 = Scale::parse("1").unwrap();
        let right = judge(
            6,
            &sf1,
            &execution(rows(&["123141078.2283"])),
            &execution(rows(&["123141078.2283"])),
        );
        assert_eq!(right.reference, Some(Verdict::Same));
        let wrong = judge(
            6,
            &sf1,
            &execution(rows(&["123141278.23"])),
            &execution(rows(&["123141278.23"])),
        );
        assert_eq!(
            wrong.reference.as_ref().map(Verdict::label),
            Some("MISMATCH")
        );
        assert!(wrong.failed());
        assert!(wrong.detail().unwrap().starts_with("reference: row 1"));
    }

    #[test]
    fn results_have_a_line_per_query() {
        let sf1 = Scale::parse("1").unwrap();
        let check = judge(
            6,
            &sf1,
            &execution(rows(&["123141078.23"])),
            &execution(Outcome::Timeout),
        );
        let text = results(&[check], "check");
        let line = text.lines().last().unwrap();
        assert_eq!(
            line,
            "Q06        1  same      TIMEOUT       1500.0          -  on/off: on"
        );
    }
}

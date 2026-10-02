//! The oracle: the same query with Tessera on and off, in one connection,
//! as the SQL suites' `agg_same` runs it. The rows are compared as a
//! multiset (aggregated in a sorted array of their text). When both modes
//! fail, they agree whatever the SQLSTATEs, unless one is an internal
//! error (class XX): a batch evaluates a condition for its 64 rows before
//! it computes their outputs, so of two rows that would each raise an
//! error the batch can meet the other first. A lost connection is a crash
//! of the backend; the server restarts every backend after one, and the
//! oracle connects again.
//!
//! A data exception (class 22: an overflow, a division by zero) in one
//! mode only is a row that mode's plan evaluates and the other's does not:
//! rows past a LIMIT in a batch of 64 or in a parallel worker, a group the
//! core's sorted grouping never reaches under a limit, a join condition
//! one plan hashes on and the other checks after a match. Expressions
//! carry no promise of which rows they are evaluated for, in the core
//! either, so that is agreement when the error is the data's: the mode
//! that returned rows raises a data exception too on the relaxed query (no
//! LIMIT, no DISTINCT, joins that read every row) or on a probe of one
//! table (every row of it, through each largest part of the query's
//! expressions that reads it alone). Otherwise the error is one mode's
//! own, an overflow that is none, or one missed.

use std::fmt::Write;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use postgres::SimpleQueryMessage;
use postgres::error::SqlState;
use tessera_pgtool::Cluster;

use crate::ast::Query;

/// How long one mode of a query may run.
pub const STATEMENT_TIMEOUT: Duration = Duration::from_secs(5);

/// How one mode of a query ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The rows' text, sorted, in one array; `None` without rows.
    Rows(Option<String>),
    /// An error: its SQLSTATE and message.
    Error { code: String, message: String },
    /// statement_timeout cancelled it.
    Timeout,
    /// The connection was lost: the backend crashed.
    Lost(String),
}

impl Outcome {
    fn short(&self) -> String {
        let text = match self {
            Outcome::Rows(None) => "no rows".to_string(),
            Outcome::Rows(Some(rows)) => format!("rows {rows}"),
            Outcome::Error { code, message } => format!("ERROR {code}: {message}"),
            Outcome::Timeout => "timeout".to_string(),
            Outcome::Lost(message) => format!("connection lost: {message}"),
        };
        let mut short: String = text.chars().take(400).collect();
        if short.len() < text.len() {
            short.push('…');
        }
        short
    }
}

impl Outcome {
    fn is_data_exception(&self) -> bool {
        matches!(self, Outcome::Error { code, .. } if code.starts_with("22"))
    }
}

/// What the two modes of a query did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub on: Outcome,
    pub off: Outcome,
    /// After a data exception in one mode only: the relaxed query in the
    /// mode that returned rows.
    pub relaxed: Option<Outcome>,
}

impl Verdict {
    /// The mode that returned rows where the other raised a data
    /// exception: true for on.
    fn one_sided(&self) -> Option<bool> {
        match (&self.on, &self.off) {
            (Outcome::Rows(_), off) if off.is_data_exception() => Some(true),
            (on, Outcome::Rows(_)) if on.is_data_exception() => Some(false),
            _ => None,
        }
    }

    /// Whether the modes disagree, or one crashed: a finding.
    pub fn is_finding(&self) -> bool {
        self.class().is_some()
    }

    /// What kind of finding this is, if it is one; shrinking keeps it.
    pub fn class(&self) -> Option<&'static str> {
        match (&self.on, &self.off) {
            (Outcome::Lost(_), _) | (_, Outcome::Lost(_)) => Some("crash"),
            (Outcome::Rows(on), Outcome::Rows(off)) => (on != off).then_some("rows differ"),
            (Outcome::Error { code: on, .. }, Outcome::Error { code: off, .. }) => {
                (on.starts_with("XX") || off.starts_with("XX")).then_some("internal error")
            }
            (Outcome::Timeout, Outcome::Timeout) => None,
            (Outcome::Timeout, _) | (_, Outcome::Timeout) => Some("timeout in one mode"),
            (Outcome::Error { code, .. }, _) | (_, Outcome::Error { code, .. })
                if code.starts_with("XX") =>
            {
                Some("internal error")
            }
            _ if self.skipped() => None,
            (Outcome::Error { .. }, _) => Some("error with Tessera only"),
            _ => Some("error without Tessera only"),
        }
    }

    /// A data exception in one mode only, which the relaxed query in the
    /// other raises too: rows one plan skips.
    fn skipped(&self) -> bool {
        self.one_sided().is_some()
            && self
                .relaxed
                .as_ref()
                .is_some_and(Outcome::is_data_exception)
    }

    /// The kind of the outcome, for the counts of a run.
    pub fn kind(&self) -> &'static str {
        if let Some(class) = self.class() {
            return class;
        }
        match (&self.on, &self.off) {
            _ if self.skipped() => "error of a row one plan skips",
            (Outcome::Rows(_), _) => "same rows",
            (Outcome::Error { code: on, .. }, Outcome::Error { code: off, .. }) if on == off => {
                "same error"
            }
            (Outcome::Error { .. }, _) => "both failed",
            _ => "both timed out",
        }
    }

    pub fn describe(&self) -> String {
        let mut text = format!("on: {}\noff: {}", self.on.short(), self.off.short());
        if let (Some(on), Some(relaxed)) = (self.one_sided(), &self.relaxed) {
            let mode = if on { "on" } else { "off" };
            let _ = write!(text, "\nrelaxed, {mode}: {}", relaxed.short());
        }
        text
    }
}

pub struct Oracle<'c> {
    cluster: &'c Cluster,
    client: postgres::Client,
}

impl<'c> Oracle<'c> {
    pub fn connect(cluster: &'c Cluster) -> Result<Oracle<'c>> {
        Ok(Oracle {
            cluster,
            client: cluster.connect()?,
        })
    }

    /// Runs the SQL that creates and fills the tables, in one piece.
    pub fn load(&mut self, setup: &str) -> Result<()> {
        self.reconnect()?;
        self.client
            .batch_execute(setup)
            .context("cannot create the tables")
    }

    /// Runs `query` in both modes under its settings.
    pub fn check(&mut self, query: &Query) -> Result<Verdict> {
        let sql = query.sql();
        let wrapped = format!("SELECT array_agg(q::text ORDER BY q::text) FROM ({sql}) AS q");
        let prelude = format!(
            "{} SET statement_timeout = {};",
            query.settings.statements(),
            STATEMENT_TIMEOUT.as_millis()
        );
        let mut run = |mode: &str| -> Result<Outcome> {
            self.reconnect()?;
            self.client
                .batch_execute(&format!("{prelude} SET tessera.enable = {mode};"))?;
            Ok(self.run(&wrapped))
        };
        let on = run("on")?;
        let off = run("off")?;
        let mut verdict = Verdict {
            on,
            off,
            relaxed: None,
        };
        if let Some(on) = verdict.one_sided() {
            verdict.relaxed = Some(self.relaxed(query, &prelude, on)?);
        }
        Ok(verdict)
    }

    /// The relaxed query in one mode, then the probes of its tables, in a
    /// transaction whose settings make joins read every row of both
    /// sides: no nested loop or merge join, no index, and with Tessera on
    /// the core's hash join over Tessera's scans and filters (the least
    /// join cost unit makes the node's join cost thousands of times the
    /// core's). A hash join still skips its inner side after an empty
    /// outer one, in the core and in Tessera alike; the probes do not.
    fn relaxed(&mut self, query: &Query, prelude: &str, on: bool) -> Result<Outcome> {
        let sql = query.relaxed().sql();
        let wrapped = format!("SELECT array_agg(q::text ORDER BY q::text) FROM ({sql}) AS q");
        let mode = if on {
            "SET LOCAL tessera.enable = on; SET LOCAL tessera.join_cost_unit = 0.001;"
        } else {
            "SET LOCAL tessera.enable = off;"
        };
        self.reconnect()?;
        let settings = self.client.batch_execute(&format!(
            "{prelude} BEGIN; {mode} SET LOCAL enable_nestloop = off; \
             SET LOCAL enable_mergejoin = off; SET LOCAL enable_indexscan = off; \
             SET LOCAL enable_indexonlyscan = off; SET LOCAL enable_bitmapscan = off;"
        ));
        // Then each table's probe, until one raises a data exception.
        let outcome = settings.map(|()| {
            let mut outcome = self.run(&wrapped);
            for probe in query.probes() {
                if !matches!(outcome, Outcome::Rows(_)) {
                    break;
                }
                let probed = self.run(&probe);
                if !matches!(probed, Outcome::Rows(_)) {
                    outcome = probed;
                }
            }
            outcome
        });
        // The transaction ends whatever happened in it, the session's
        // settings with it.
        if !self.client.is_closed() {
            self.client.batch_execute("ROLLBACK")?;
        }
        Ok(outcome?)
    }

    /// Whether the plan of `query` with Tessera on has a node of Tessera.
    pub fn uses_tessera(&mut self, query: &Query) -> Result<bool> {
        self.reconnect()?;
        self.client.batch_execute(&format!(
            "{} SET tessera.enable = on;",
            query.settings.statements()
        ))?;
        let plan = match self
            .client
            .simple_query(&format!("EXPLAIN (FORMAT JSON) {}", query.sql()))
        {
            Ok(messages) => messages
                .into_iter()
                .find_map(|message| match message {
                    SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
                    _ => None,
                })
                .unwrap_or_default(),
            Err(_) => return Ok(false),
        };
        Ok(plan.contains("\"Custom Plan Provider\": \"Tess"))
    }

    fn run(&mut self, sql: &str) -> Outcome {
        match self.client.simple_query(sql) {
            Ok(messages) => Outcome::Rows(
                messages
                    .into_iter()
                    .find_map(|message| match message {
                        SimpleQueryMessage::Row(row) => Some(row.get(0).map(str::to_string)),
                        _ => None,
                    })
                    .flatten(),
            ),
            Err(error) if error.code() == Some(&SqlState::QUERY_CANCELED) => Outcome::Timeout,
            Err(error) => match error.as_db_error() {
                Some(db) if !self.client.is_closed() => Outcome::Error {
                    code: db.code().code().to_string(),
                    message: db.message().to_string(),
                },
                _ => Outcome::Lost(tessera_pgtool::message(&error)),
            },
        }
    }

    /// Connects again after a crash, as the server restarts its backends,
    /// within a minute.
    fn reconnect(&mut self) -> Result<()> {
        if !self.client.is_closed() {
            return Ok(());
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match self.cluster.connect() {
                Ok(client) => {
                    self.client = client;
                    return Ok(());
                }
                Err(error) if Instant::now() > deadline => return Err(error),
                Err(_) => std::thread::sleep(Duration::from_millis(500)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str) -> Outcome {
        Outcome::Rows(Some(text.into()))
    }

    fn error(code: &str) -> Outcome {
        Outcome::Error {
            code: code.into(),
            message: "m".into(),
        }
    }

    #[test]
    fn findings_are_disagreements_and_crashes() {
        let verdict = |on, off| Verdict {
            on,
            off,
            relaxed: None,
        };
        assert!(!verdict(rows("{1}"), rows("{1}")).is_finding());
        assert!(verdict(rows("{1}"), rows("{2}")).is_finding());
        assert!(!verdict(error("22012"), error("22012")).is_finding());
        assert!(!verdict(error("22012"), error("22003")).is_finding());
        assert!(verdict(error("XX000"), error("22003")).is_finding());
        assert_eq!(
            verdict(error("22012"), error("22003")).kind(),
            "both failed"
        );
        assert!(verdict(rows("{1}"), error("22012")).is_finding());
        assert!(!verdict(Outcome::Timeout, Outcome::Timeout).is_finding());
        assert!(verdict(Outcome::Timeout, rows("{1}")).is_finding());
        assert!(verdict(Outcome::Lost("x".into()), rows("{1}")).is_finding());
        assert_eq!(
            verdict(Outcome::Lost("x".into()), rows("{1}")).kind(),
            "crash"
        );
        assert_eq!(verdict(error("22012"), error("22012")).kind(), "same error");
        assert_eq!(
            verdict(Outcome::Timeout, rows("{1}")).kind(),
            "timeout in one mode"
        );
        assert_eq!(
            verdict(rows("{1}"), error("XX000")).kind(),
            "internal error"
        );
    }

    #[test]
    fn a_data_exception_in_one_mode_is_the_datas_when_the_relaxed_query_raises_one() {
        let verdict = |on, off, relaxed| Verdict { on, off, relaxed };
        let skipped = verdict(error("22012"), rows("{1}"), Some(error("22003")));
        assert!(!skipped.is_finding());
        assert_eq!(skipped.kind(), "error of a row one plan skips");
        assert!(skipped.describe().ends_with("relaxed, off: ERROR 22003: m"));
        let own = verdict(error("22003"), rows("{1}"), Some(rows("{1,2}")));
        assert_eq!(own.class(), Some("error with Tessera only"));
        let missed = verdict(rows("{1}"), error("22003"), Some(rows("{1}")));
        assert_eq!(missed.class(), Some("error without Tessera only"));
        assert!(skipped.describe().contains("on: ERROR 22012"));
        assert!(!verdict(rows("{1}"), error("22003"), Some(error("22003"))).is_finding());
        // Not a data exception: no relaxed run decides it.
        assert!(verdict(error("42883"), rows("{1}"), Some(error("22003"))).is_finding());
        assert!(verdict(error("22012"), rows("{1}"), Some(Outcome::Timeout)).is_finding());
    }
}

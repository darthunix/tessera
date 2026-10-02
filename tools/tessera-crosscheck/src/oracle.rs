//! The oracle: the same query with Tessera on and off, in one connection,
//! as the SQL suites' `agg_same` runs it. The rows are compared as a
//! multiset (aggregated in a sorted array of their text). When both modes
//! fail, they agree whatever the SQLSTATEs, unless one is an internal
//! error (class XX): a batch evaluates a condition for its 64 rows before
//! it computes their outputs, so of two rows that would each raise an
//! error the batch can meet the other first. A lost connection is a crash
//! of the backend; the server restarts every backend after one, and the
//! oracle connects again.

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

/// What the two modes of a query did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub on: Outcome,
    pub off: Outcome,
}

impl Verdict {
    /// Whether the modes disagree, or one crashed: a finding.
    pub fn is_finding(&self) -> bool {
        match (&self.on, &self.off) {
            (Outcome::Rows(on), Outcome::Rows(off)) => on != off,
            (Outcome::Error { code: on, .. }, Outcome::Error { code: off, .. }) => {
                on.starts_with("XX") || off.starts_with("XX")
            }
            (Outcome::Timeout, Outcome::Timeout) => false,
            _ => true,
        }
    }

    /// The kind of the outcome, for the counts of a run.
    pub fn kind(&self) -> &'static str {
        match (&self.on, &self.off) {
            (Outcome::Lost(_), _) | (_, Outcome::Lost(_)) => "crash",
            _ if self.is_finding() => "differ",
            (Outcome::Rows(_), _) => "same rows",
            (Outcome::Error { code: on, .. }, Outcome::Error { code: off, .. }) if on == off => {
                "same error"
            }
            (Outcome::Error { .. }, _) => "both failed",
            _ => "both timed out",
        }
    }

    pub fn describe(&self) -> String {
        format!("on: {}\noff: {}", self.on.short(), self.off.short())
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
        Ok(Verdict { on, off })
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
        let verdict = |on, off| Verdict { on, off };
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
    }
}

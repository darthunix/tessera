//! The modes a query runs in, and a connection set up for one: Tessera on
//! and off over the foreign table, and pg_duckdb over its view. The
//! settings never change inside a connection.

use std::fmt;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use postgres::SimpleQueryMessage;
use postgres::error::SqlState;
use serde::{Deserialize, Serialize};
use tessera_pgtool::compare::Row;

pub use tessera_pgtool::message;

use crate::cluster::Cluster;

/// Where a query runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The foreign table, Tessera's nodes and the wrapper's batch source.
    On,
    /// The foreign table, PostgreSQL's executor and the wrapper's rows.
    Off,
    /// pg_duckdb's view of read_parquet: DuckDB reads the files.
    Duckdb,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Off, Mode::On, Mode::Duckdb];

    pub fn name(self) -> &'static str {
        match self {
            Mode::On => "on",
            Mode::Off => "off",
            Mode::Duckdb => "duckdb",
        }
    }

    /// Parses `on,off,duckdb`, in the order of [`Mode::ALL`].
    pub fn parse_list(text: &str) -> Result<Vec<Mode>> {
        let mut modes = Vec::new();
        for word in text.split(',') {
            let mode = match word.trim() {
                "on" => Mode::On,
                "off" => Mode::Off,
                "duckdb" => Mode::Duckdb,
                other => bail!("a mode is on, off or duckdb, not {other:?}"),
            };
            modes.push(mode);
        }
        Ok(Mode::ALL
            .into_iter()
            .filter(|mode| modes.contains(mode))
            .collect())
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The settings a query runs under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// max_parallel_workers_per_gather.
    pub workers: u32,
    pub work_mem: String,
    /// Whether the cluster has pg_duckdb, whose setting a mode then sets.
    pub duckdb: bool,
    /// DuckDB's threads in the mode duckdb, set before its instance
    /// starts at the connection's first query of DuckDB.
    pub duckdb_threads: u32,
    /// Whether the run counts, reading its queries inside a function,
    /// which pg_duckdb then has to be allowed to run them in.
    pub counting: bool,
}

impl Settings {
    /// The statements that set a connection up for `mode`, with
    /// `timeout` as its statement_timeout.
    pub fn statements(&self, mode: Mode, timeout: Duration) -> String {
        let (path, tessera, duckdb) = match mode {
            Mode::On => ("fdw", "on", "off"),
            Mode::Off => ("fdw", "off", "off"),
            Mode::Duckdb => ("ddb, public", "off", "on"),
        };
        let mut text = format!(
            "SET search_path = {path}; SET tessera.enable = {tessera}; \
             SET max_parallel_workers_per_gather = {}; SET work_mem = '{}'; \
             SET statement_timeout = {}",
            self.workers,
            self.work_mem.replace('\'', "''"),
            timeout.as_millis().max(1)
        );
        if self.duckdb {
            text.push_str(&format!("; SET duckdb.force_execution = {duckdb}"));
        }
        if mode == Mode::Duckdb {
            text.push_str(&format!("; SET duckdb.threads = {}", self.duckdb_threads));
            if self.counting {
                // The functions of a counting run read the query inside them.
                text.push_str("; SET duckdb.unsafe_allow_execution_inside_functions = on");
            }
        }
        text
    }
}

/// How an execution ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Rows(Vec<Row>),
    /// statement_timeout cancelled it.
    Timeout,
    /// Any other error: SQLSTATE and message.
    Error(String),
}

/// An execution: how it ended, and the time from sending the query to
/// its last row, as ClickBench times a query with psql's `\timing`.
#[derive(Debug, Clone)]
pub struct Execution {
    pub outcome: Outcome,
    pub elapsed: Duration,
}

/// A connection set up for one mode.
pub struct Session {
    pub mode: Mode,
    client: postgres::Client,
}

impl Session {
    pub fn open(
        cluster: &Cluster,
        settings: &Settings,
        mode: Mode,
        timeout: Duration,
    ) -> Result<Session> {
        let mut client = cluster.connect()?;
        client.batch_execute(&settings.statements(mode, timeout))?;
        Ok(Session { mode, client })
    }

    /// Sets the connection's statement_timeout anew.
    pub fn set_timeout(&mut self, timeout: Duration) -> Result<()> {
        self.client.batch_execute(&format!(
            "SET statement_timeout = {}",
            timeout.as_millis().max(1)
        ))?;
        Ok(())
    }

    /// Runs a statement and reads every row as text.
    pub fn run(&mut self, sql: &str) -> Execution {
        let start = Instant::now();
        let result = self.client.simple_query(sql);
        let elapsed = start.elapsed();
        let outcome = match result {
            Ok(messages) => Outcome::Rows(
                messages
                    .iter()
                    .filter_map(|message| match message {
                        SimpleQueryMessage::Row(row) => Some(
                            (0..row.len())
                                .map(|i| row.get(i).map(str::to_string))
                                .collect(),
                        ),
                        _ => None,
                    })
                    .collect(),
            ),
            Err(error) if error.code() == Some(&SqlState::QUERY_CANCELED) => Outcome::Timeout,
            Err(error) => Outcome::Error(message(&error)),
        };
        Execution { outcome, elapsed }
    }

    /// The types of a query's output columns, by name, as the server
    /// prepares it.
    pub fn column_types(&mut self, sql: &str) -> Result<Vec<String>> {
        let statement = self.client.prepare(sql)?;
        Ok(statement
            .columns()
            .iter()
            .map(|column| column.type_().name().to_string())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_set_the_mode() {
        let settings = Settings {
            workers: 4,
            work_mem: "64MB".into(),
            duckdb: true,
            duckdb_threads: 5,
            counting: false,
        };
        let timeout = Duration::from_secs(60);
        assert_eq!(
            settings.statements(Mode::On, timeout),
            "SET search_path = fdw; SET tessera.enable = on; \
             SET max_parallel_workers_per_gather = 4; SET work_mem = '64MB'; \
             SET statement_timeout = 60000; SET duckdb.force_execution = off"
        );
        assert!(
            settings
                .statements(Mode::Duckdb, timeout)
                .starts_with("SET search_path = ddb, public; SET tessera.enable = off;")
        );
        assert!(
            settings
                .statements(Mode::Duckdb, timeout)
                .ends_with("SET duckdb.force_execution = on; SET duckdb.threads = 5")
        );
        let counting = Settings {
            counting: true,
            ..settings.clone()
        };
        assert!(counting.statements(Mode::Duckdb, timeout).ends_with(
            "SET duckdb.threads = 5; SET duckdb.unsafe_allow_execution_inside_functions = on"
        ));
        assert!(!counting.statements(Mode::On, timeout).contains("unsafe"));
        let plain = Settings {
            duckdb: false,
            ..settings
        };
        assert!(!plain.statements(Mode::Off, timeout).contains("duckdb"));
        assert!(
            plain
                .statements(Mode::Off, Duration::ZERO)
                .ends_with("SET statement_timeout = 1")
        );
    }

    #[test]
    fn modes_are_listed_in_one_order() {
        assert_eq!(
            Mode::parse_list("duckdb,on").unwrap(),
            vec![Mode::On, Mode::Duckdb]
        );
        assert_eq!(
            Mode::parse_list("on,off,duckdb").unwrap(),
            Mode::ALL.to_vec()
        );
        assert!(Mode::parse_list("on,fast").is_err());
    }
}

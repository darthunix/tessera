//! A connection in one mode, Tessera on or off, with the settings of the
//! run, running the queries as prepared statements: the same statements
//! are checked and timed. The setting never changes inside a connection,
//! as the specification asks of a run: its settings stay fixed.

use std::time::{Duration, Instant};

use anyhow::Result;
use postgres::SimpleQueryMessage;
use postgres::error::SqlState;

use crate::cluster::Cluster;
use crate::compare::Row;

/// The settings a query runs under.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSettings {
    /// max_parallel_workers_per_gather.
    pub workers: u32,
    pub work_mem: String,
    pub jit: bool,
    /// statement_timeout.
    pub timeout: Duration,
}

impl SessionSettings {
    /// The statements that set a connection up for a mode.
    pub fn statements(&self, tessera: bool) -> String {
        let on = |value: bool| if value { "on" } else { "off" };
        format!(
            "SET tessera.enable = {}; SET jit = {}; SET max_parallel_workers_per_gather = {}; \
             SET work_mem = '{}'; SET statement_timeout = {}",
            on(tessera),
            on(self.jit),
            self.workers,
            self.work_mem.replace('\'', "''"),
            self.timeout.as_millis()
        )
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

/// An execution: how it ended and the time from sending it to its last
/// row, as the specification times a query.
#[derive(Debug, Clone)]
pub struct Execution {
    pub outcome: Outcome,
    pub elapsed: Duration,
}

pub struct Session<'c> {
    cluster: &'c Cluster,
    settings: SessionSettings,
    pub tessera: bool,
    client: postgres::Client,
    /// The statements prepared so far, prepared again after a reconnect.
    prepared: Vec<(String, String)>,
}

impl<'c> Session<'c> {
    pub fn open(
        cluster: &'c Cluster,
        settings: &SessionSettings,
        tessera: bool,
    ) -> Result<Session<'c>> {
        let mut client = cluster.connect()?;
        client.batch_execute(&settings.statements(tessera))?;
        Ok(Session {
            cluster,
            settings: settings.clone(),
            tessera,
            client,
            prepared: Vec::new(),
        })
    }

    /// Prepares `text` as the statement `name`; the error is the server's.
    pub fn prepare(&mut self, name: &str, text: &str) -> Result<(), String> {
        self.recover().map_err(|error| error.to_string())?;
        self.client
            .batch_execute(&format!("PREPARE {name} AS {text}"))
            .map_err(|error| message(&error))?;
        self.prepared.push((name.to_string(), text.to_string()));
        Ok(())
    }

    /// Executes a prepared statement and reads every row as text.
    pub fn execute(&mut self, name: &str) -> Execution {
        self.run(&format!("EXECUTE {name}"))
    }

    /// Runs a statement and reads every row as text.
    pub fn run(&mut self, sql: &str) -> Execution {
        if let Err(error) = self.recover() {
            return Execution {
                outcome: Outcome::Error(error.to_string()),
                elapsed: Duration::ZERO,
            };
        }
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

    /// After the server lost the connection, as when a backend crashes and
    /// the postmaster restarts them all, connects again within a minute
    /// and prepares the statements again, so that one crash costs one
    /// query.
    fn recover(&mut self) -> Result<()> {
        if !self.client.is_closed() {
            return Ok(());
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        let client = loop {
            match self.cluster.connect() {
                Ok(client) => break client,
                Err(error) if Instant::now() > deadline => return Err(error),
                Err(_) => std::thread::sleep(Duration::from_millis(500)),
            }
        };
        self.client = client;
        self.client
            .batch_execute(&self.settings.statements(self.tessera))?;
        for (name, text) in &self.prepared {
            self.client
                .batch_execute(&format!("PREPARE {name} AS {text}"))?;
        }
        Ok(())
    }
}

pub use tessera_pgtool::message;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_set_the_mode_and_the_run() {
        let settings = SessionSettings {
            workers: 2,
            work_mem: "4MB".into(),
            jit: false,
            timeout: Duration::from_secs(30),
        };
        assert_eq!(
            settings.statements(true),
            "SET tessera.enable = on; SET jit = off; SET max_parallel_workers_per_gather = 2; \
             SET work_mem = '4MB'; SET statement_timeout = 30000"
        );
        assert!(
            settings
                .statements(false)
                .starts_with("SET tessera.enable = off;")
        );
    }
}

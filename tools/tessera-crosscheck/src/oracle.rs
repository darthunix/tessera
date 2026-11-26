//! The oracle: the same query with Tessera on and off, in one connection,
//! as the SQL suites' `agg_same` runs it. The rows are compared as a
//! multiset (aggregated in a sorted array of their text), and those of a
//! query ordered by every output column in their order. When both modes
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
//!
//! The table of strings has a Parquet twin, `words_pq`: its rows, read
//! back from the heap table, written by the reader's crate in three
//! files, of chunks through a dictionary, of plain chunks, and of chunks
//! whose dictionary fills up and falls back to plain pages. A query that
//! reads the table of strings, and whose modes agree, runs once more over
//! the twin with Tessera on, against the heap table with it off: a fault
//! of the reader, which both modes over the twin would share, shows there.

use std::fmt::Write;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parquet::basic::{Encoding, LogicalType};
use parquet::file::reader::{FileReader, SerializedFileReader};
use postgres::SimpleQueryMessage;
use postgres::error::SqlState;
use tessera_parquet::testing::{self, Column, Options, Values};
use tessera_pgtool::Cluster;

use crate::ast::{Query, quote};
use crate::schema::WORDS_ROWS;

/// How the table of strings is written in its query's SQL, and its twin.
const TWINNED: &str = " words AS ";
const TWIN: &str = " words_pq AS ";

/// Whether the SQL of a query reads the table of strings.
pub fn reads_twinned(sql: &str) -> bool {
    sql.contains(TWINNED)
}

/// The SQL with the table of strings read from its Parquet twin: the
/// generator writes a table as `name AS alias`, and no literal holds that.
pub fn twinned(sql: &str) -> String {
    sql.replace(TWINNED, TWIN)
}

/// The SQL that makes the Parquet twin over `files`.
pub fn twin_table(files: &[PathBuf]) -> String {
    let files: Vec<String> = files
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    format!(
        "CREATE SERVER IF NOT EXISTS twin FOREIGN DATA WRAPPER tessera_parquet; \
         DROP FOREIGN TABLE IF EXISTS words_pq; \
         CREATE FOREIGN TABLE words_pq (id int4, a int4, t text, u text) \
         SERVER twin OPTIONS (files {}); ANALYZE words_pq;",
        quote(&files.join(","))
    )
}

/// Whether every string chunk of a group of the file has a dictionary
/// page and data pages both through it and plain, as a writer falls back
/// when its dictionary fills up: the footer's mask of the data pages'
/// encodings, which the reader reads too.
fn falls_back(path: &Path) -> Result<bool> {
    let reader = SerializedFileReader::new(File::open(path)?)?;
    Ok(reader.metadata().row_groups().iter().any(|group| {
        group.columns()[2..].iter().all(|chunk| {
            let dictionary =
                |e: &Encoding| matches!(e, Encoding::PLAIN_DICTIONARY | Encoding::RLE_DICTIONARY);
            chunk.dictionary_page_offset().is_some()
                && chunk.page_encoding_stats_mask().is_some_and(|mask| {
                    mask.encodings().any(|e| dictionary(&e))
                        && mask.encodings().any(|e| !dictionary(&e))
                })
        })
    }))
}

/// Whether a verdict met while shrinking stands for `original`: the same
/// class of finding, by the same comparison, of two modes or over the
/// twin.
pub fn same_finding(original: &Verdict, verdict: &Verdict) -> bool {
    original.class().is_some()
        && verdict.class() == original.class()
        && verdict.twin == original.twin
}

/// How long one mode of a query may run. Only the query runs under it,
/// and after a query that failed in the relaxed query's transaction, the
/// ROLLBACK of that transaction: the oracle's own statements, its
/// settings, transactions and the data's setup, can stall on a slow
/// machine too, and that is no finding.
pub const STATEMENT_TIMEOUT: Duration = Duration::from_secs(5);

/// The outcomes of two modes. When one ran out of time and the other
/// returned its rows after half of it, both ran near the limit, and which
/// of them ends first is the machine's: the rows count as timed out too.
/// A crash or an error stays what it was.
fn near_limit(a: &(Outcome, Duration), b: &(Outcome, Duration)) -> (Outcome, Outcome) {
    let near = |(outcome, time): &(Outcome, Duration), other: &Outcome| match outcome {
        Outcome::Rows(_) if *other == Outcome::Timeout && *time > STATEMENT_TIMEOUT / 2 => {
            Outcome::Timeout
        }
        _ => outcome.clone(),
    };
    (near(a, &b.0), near(b, &a.0))
}

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
    /// Whether `on` is the query over the Parquet twin of the table of
    /// strings with Tessera on, and `off` the query over the heap table
    /// with it off.
    pub twin: bool,
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
        let mut text = String::new();
        if self.twin {
            text.push_str(
                "on: over the Parquet twin, with Tessera; off: over the heap table, without\n",
            );
        }
        let _ = write!(text, "on: {}\noff: {}", self.on.short(), self.off.short());
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
            .context("cannot create the tables")?;
        self.write_twin().context("cannot write the Parquet twin")
    }

    /// Writes the Parquet twin of the table of strings from its rows, in a
    /// file of chunks through a dictionary, one of plain chunks, and one
    /// of chunks whose dictionary falls back to plain pages, and makes the
    /// foreign table over them.
    pub fn twin_files(&self) -> Vec<PathBuf> {
        let directory = self.cluster.data.with_file_name("crosscheck-twin");
        (0..3)
            .map(|index| directory.join(format!("words-{index}.parquet")))
            .collect()
    }

    fn write_twin(&mut self) -> Result<()> {
        let directory = self.cluster.data.with_file_name("crosscheck-twin");
        fs::create_dir_all(&directory)?;
        let rows = self
            .client
            .query("SELECT id, a, t, u FROM words ORDER BY id", &[])?;
        let ints = |at: usize| -> Vec<Option<i32>> { rows.iter().map(|row| row.get(at)).collect() };
        let strings = |at: usize| -> Vec<Option<Vec<u8>>> {
            rows.iter()
                .map(|row| row.get::<_, Option<String>>(at).map(String::into_bytes))
                .collect()
        };
        let (id, a, t, u) = (ints(0), ints(1), strings(2), strings(3));
        // A group of the first and of the last file is longer than a
        // window of the reader, 1024 rows: the reader carries its forward
        // read from one window to the next, and windows share a dictionary.
        let (first, second) = (1300, 1800);
        anyhow::ensure!(rows.len() == WORDS_ROWS as usize, "words holds other rows");
        let paged = |group, dictionary| Options {
            page_rows: Some(64),
            ..Options::new(group, dictionary)
        };
        let mixed = Options {
            page_rows: Some(32),
            dictionary_bytes: Some(64),
            ..Options::new(rows.len() - second, true)
        };
        let parts = [
            (0..first, paged(first, true)),
            (first..second, paged(128, false)),
            (second..rows.len(), mixed),
        ];
        let files = self.twin_files();
        for ((range, options), path) in parts.into_iter().zip(&files) {
            let string = |name: &str, values: &[Option<Vec<u8>>]| {
                Column::new(name, Values::Bytes(values.to_vec())).logical(LogicalType::String)
            };
            let columns = [
                Column::new("id", Values::Int32(id[range.clone()].to_vec())),
                Column::new("a", Values::Int32(a[range.clone()].to_vec())),
                string("t", &t[range.clone()]),
                string("u", &u[range]),
            ];
            testing::write_with(path, &columns, options)?;
        }
        anyhow::ensure!(
            falls_back(&files[2])?,
            "the twin's last file has no chunk whose dictionary falls back to plain pages"
        );
        self.client.batch_execute(&twin_table(&files))?;
        Ok(())
    }

    /// Runs `query` in both modes under its settings.
    pub fn check(&mut self, query: &Query) -> Result<Verdict> {
        let sql = query.sql();
        // The subquery's order holds: a subquery with ORDER BY is not
        // pulled up, and the aggregate over it is not parallel.
        let wrapped = if query.ordered() {
            format!("SELECT array_agg(q::text) FROM ({sql}) AS q")
        } else {
            format!("SELECT array_agg(q::text ORDER BY q::text) FROM ({sql}) AS q")
        };
        let prelude = query.settings.statements();
        let mut run = |mode: &str, sql: &str| -> Result<(Outcome, Duration)> {
            self.reconnect()?;
            self.client
                .batch_execute(&format!("{prelude} SET tessera.enable = {mode};"))?;
            let start = Instant::now();
            let outcome = self.run(sql);
            Ok((outcome, start.elapsed()))
        };
        let on = run("on", &wrapped)?;
        let off = run("off", &wrapped)?;
        let twin = (reads_twinned(&sql))
            .then(|| run("on", &twinned(&wrapped)))
            .transpose()?;
        let (on_outcome, off_outcome) = near_limit(&on, &off);
        let mut verdict = Verdict {
            on: on_outcome,
            off: off_outcome,
            relaxed: None,
            twin: false,
        };
        if let Some(on) = verdict.one_sided() {
            verdict.relaxed = Some(self.relaxed(query, &prelude, on, false)?);
        }
        let Some(twin) = twin else {
            return Ok(verdict);
        };
        if verdict.is_finding() {
            return Ok(verdict);
        }
        let (twin, off) = near_limit(&twin, &off);
        let mut over_twin = Verdict {
            on: twin,
            off,
            relaxed: None,
            twin: true,
        };
        if let Some(on) = over_twin.one_sided() {
            over_twin.relaxed = Some(self.relaxed(query, &prelude, on, on)?);
        }
        Ok(if over_twin.is_finding() {
            over_twin
        } else {
            verdict
        })
    }

    /// The relaxed query in one mode, then the probes of its tables, in a
    /// transaction whose settings make joins read every row of both
    /// sides: no nested loop or merge join, no index, and with Tessera on
    /// the core's hash join over Tessera's scans and filters (the least
    /// join cost unit makes the node's join cost thousands of times the
    /// core's). A hash join still skips its inner side after an empty
    /// outer one, in the core and in Tessera alike; the probes do not.
    /// With `twin`, all of them over the Parquet twin.
    fn relaxed(&mut self, query: &Query, prelude: &str, on: bool, twin: bool) -> Result<Outcome> {
        let over = |sql: String| if twin { twinned(&sql) } else { sql };
        let sql = over(query.relaxed().sql());
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
                let probed = self.run(&over(probe));
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
        let Outcome::Rows(Some(plan)) = self.run(&format!("EXPLAIN (FORMAT JSON) {}", query.sql()))
        else {
            return Ok(false);
        };
        Ok(plan.contains("\"Custom Plan Provider\": \"Tess"))
    }

    /// Runs one query of a mode under the limit, set in the query's own
    /// message and reset at its end. The message is a transaction of its
    /// own, or a part of the relaxed query's, which keeps the limit to its
    /// ROLLBACK only after a query that failed and skipped the reset.
    fn run(&mut self, sql: &str) -> Outcome {
        // The reset also takes a cancel that the limit leaves pending when
        // it fires after the query's last check for interrupts, while its
        // rows go out: the query ran to the limit, and the message fails.
        // Without it the cancel would fail the session's next statement,
        // one of the oracle's own, before that statement runs.
        let sql = format!(
            "SET LOCAL statement_timeout = {}; {sql}; SET LOCAL statement_timeout = 0",
            STATEMENT_TIMEOUT.as_millis()
        );
        match self.client.simple_query(&sql) {
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
    fn a_finding_over_the_twin_stands_only_for_one_over_the_twin() {
        let verdict = |twin| Verdict {
            on: rows("{1}"),
            off: rows("{2}"),
            relaxed: None,
            twin,
        };
        assert!(same_finding(&verdict(true), &verdict(true)));
        assert!(!same_finding(&verdict(true), &verdict(false)));
        assert!(!same_finding(&verdict(false), &verdict(true)));
        let agree = Verdict {
            off: rows("{1}"),
            ..verdict(true)
        };
        assert!(!same_finding(&agree, &agree));
    }

    #[test]
    fn the_twin_takes_the_place_of_the_table_of_strings() {
        let sql = "SELECT t0.t AS c0 FROM words AS t0 JOIN fact AS t1 ON t1.t = t0.t \
                   WHERE (EXISTS (SELECT 1 FROM words AS t2 WHERE t2.id = t0.a))";
        assert!(reads_twinned(sql));
        assert_eq!(
            twinned(sql),
            "SELECT t0.t AS c0 FROM words_pq AS t0 JOIN fact AS t1 ON t1.t = t0.t \
             WHERE (EXISTS (SELECT 1 FROM words_pq AS t2 WHERE t2.id = t0.a))"
        );
        assert!(!reads_twinned("SELECT t0.a AS c0 FROM fact AS t0"));
    }

    #[test]
    fn findings_are_disagreements_and_crashes() {
        let verdict = |on, off| Verdict {
            on,
            off,
            relaxed: None,
            twin: false,
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
    fn rows_after_half_the_limit_beside_a_timeout_timed_out_too() {
        let half = STATEMENT_TIMEOUT / 2;
        let late = (rows("{1}"), half + Duration::from_millis(1));
        let early = (rows("{1}"), half);
        let out = (Outcome::Timeout, STATEMENT_TIMEOUT);
        let both = (Outcome::Timeout, Outcome::Timeout);
        assert_eq!(near_limit(&late, &out), both);
        assert_eq!(near_limit(&out, &late), both);
        assert_eq!(near_limit(&early, &out), (rows("{1}"), Outcome::Timeout));
        assert_eq!(near_limit(&out, &early), (Outcome::Timeout, rows("{1}")));
        // Without a timeout the times decide nothing.
        let slow = (rows("{2}"), STATEMENT_TIMEOUT);
        assert_eq!(near_limit(&late, &slow), (rows("{1}"), rows("{2}")));
        // A crash or an error near the limit stays a finding.
        for outcome in [Outcome::Lost("x".into()), error("XX000"), error("53200")] {
            let late = (outcome.clone(), STATEMENT_TIMEOUT);
            assert_eq!(near_limit(&late, &out), (outcome, Outcome::Timeout));
        }
    }

    #[test]
    fn a_data_exception_in_one_mode_is_the_datas_when_the_relaxed_query_raises_one() {
        let verdict = |on, off, relaxed| Verdict {
            on,
            off,
            relaxed,
            twin: false,
        };
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

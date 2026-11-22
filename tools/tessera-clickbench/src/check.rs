//! The check of the answers: every query once in each mode, Tessera off
//! the reference. Tessera on must give off's answer value for value,
//! pg_duckdb off's with its numbers close. Rows equal in their sort keys
//! may come in any order, and where a LIMIT or an OFFSET cuts such a run,
//! any of the run's rows: there the modes agree on the count of the rows
//! and on their keys, and the companion query checks that a mode's rows
//! are, with their repeats, among the rows of the query without its
//! LIMIT, which Tessera off computes.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use tessera_pgtool::compare::{Kind, Precision, Row, Shape, compare};

use crate::cluster::literal;
use crate::queries::Query;
use crate::session::{Mode, Outcome, Session};

/// The queries whose answer pg_duckdb gives by a rule of DuckDB's own,
/// why, and the change of the query's text that makes PostgreSQL follow
/// that rule: pg_duckdb's answer is compared with Tessera off's answer to
/// the changed query instead.
const DUCKDB_RULES: [(usize, &str, &str, &str); 1] = [(
    29,
    "DuckDB's regular expressions match no newline with '.', PostgreSQL's do, \
     and some values of Referer hold one",
    "/.*$'",
    "/[^' || chr(10) || ']*$'",
)];

/// How a mode's answer compared with Tessera off's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Tessera off's own answer, the reference.
    Reference,
    Same,
    Differs(String),
    /// pg_duckdb's answer to a query of [`DUCKDB_RULES`], the same as
    /// Tessera off's under DuckDB's rule: the rule.
    Known(String),
    /// Rows past a cut that the query without its LIMIT does not have.
    NotInFull(String),
    Timeout,
    Error(String),
    /// Tessera off gave no answer to compare with.
    NoReference,
}

impl Verdict {
    pub fn passed(&self) -> bool {
        matches!(self, Verdict::Reference | Verdict::Same | Verdict::Known(_))
    }

    pub fn label(&self) -> String {
        match self {
            Verdict::Reference => "reference".into(),
            Verdict::Same => "same".into(),
            Verdict::Differs(detail) => format!("differs: {detail}"),
            Verdict::Known(rule) => format!("same under DuckDB's rule: {rule}"),
            Verdict::NotInFull(detail) => format!("not in the full result: {detail}"),
            Verdict::Timeout => "timeout".into(),
            Verdict::Error(error) => format!("error: {error}"),
            Verdict::NoReference => "no reference".into(),
        }
    }
}

/// One mode's answer to a query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub mode: Mode,
    pub verdict: Verdict,
    pub rows: Option<usize>,
    pub seconds: f64,
    /// Whether the companion query checked its rows.
    pub contained: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryCheck {
    pub query: usize,
    pub answers: Vec<Answer>,
}

impl QueryCheck {
    pub fn passed(&self) -> bool {
        self.answers.iter().all(|answer| answer.verdict.passed())
    }
}

/// The precision a mode's answer is compared with.
fn precision(mode: Mode) -> Precision {
    match mode {
        Mode::Duckdb => Precision::Close,
        Mode::On | Mode::Off => Precision::Exact,
    }
}

/// The kind of each output column by its type: the averages and the
/// floating-point numbers, which pg_duckdb gives in `double precision`,
/// are compared close; every other column byte for byte.
fn kinds(types: &[String]) -> Vec<Kind> {
    types
        .iter()
        .map(|name| {
            if approximate(name) {
                Kind::Ratio
            } else {
                Kind::Exact
            }
        })
        .collect()
}

fn approximate(type_name: &str) -> bool {
    matches!(type_name, "numeric" | "float4" | "float8")
}

/// A number as the companion compares it from pg_duckdb: rounded to nine
/// significant digits, `NULL` kept.
fn rounded(value: &str) -> String {
    format!(
        "round(({value})::numeric, \
         coalesce((8 - floor(log(abs(nullif(({value})::numeric, 0)))))::int, 0))"
    )
}

/// The companion query: how many of `rows`, with their repeats, the query
/// `unordered` lacks, every column compared as text, or, where `round`
/// says so, as a number of nine significant digits.
pub fn companion(unordered: &str, rows: &[Row], round: &[bool]) -> String {
    let width = round.len();
    let mut values = String::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            values.push_str(", ");
        }
        values.push('(');
        for (column, value) in row.iter().enumerate() {
            if column > 0 {
                values.push_str(", ");
            }
            let value = match value {
                Some(text) => format!("{}::text", literal(text)),
                None => "NULL::text".into(),
            };
            if round[column] {
                values.push_str(&rounded(&value));
            } else {
                values.push_str(&value);
            }
        }
        values.push(')');
    }
    let aliases = (1..=width)
        .map(|column| format!("c{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    let selected = (0..width)
        .map(|column| {
            let value = format!("f.c{}", column + 1);
            if round[column] {
                rounded(&value)
            } else {
                format!("{value}::text")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT count(*) FROM (VALUES {values} EXCEPT ALL \
         SELECT {selected} FROM ({unordered}) AS f({aliases})) AS lacking"
    )
}

/// A mode's answer against the reference: the comparison, then, where it
/// passed with rows that differ from the reference's, the companion query
/// in Tessera off's session. Returns the verdict and whether the
/// companion ran.
fn judge(
    off: &mut Session,
    mode: Mode,
    query: &Query,
    kinds: &[Kind],
    expected: &[Row],
    actual: &[Row],
) -> (Verdict, bool) {
    let shape = Shape {
        kinds,
        keys: query.keys,
        limit: query.limit,
        offset: query.offset,
    };
    if let Err(detail) = compare(expected, actual, &shape, precision(mode)) {
        return (Verdict::Differs(detail), false);
    }
    let Some(unordered) = query.unordered() else {
        return (Verdict::Same, false);
    };
    if actual == expected {
        return (Verdict::Same, false);
    }
    let round: Vec<bool> = kinds
        .iter()
        .map(|&kind| mode == Mode::Duckdb && kind != Kind::Exact)
        .collect();
    (lacking(off, &companion(unordered, actual, &round)), true)
}

/// The rows a mode returned, where it returned them.
type Returned = Vec<Option<Vec<Row>>>;

/// A query's rows in every mode, judged against Tessera off's: for
/// pg_duckdb, against Tessera off's answer to the query under DuckDB's
/// rule where [`DUCKDB_RULES`] has one. Gives the rows too.
fn answers(sessions: &mut [Session], query: &Query) -> (Vec<Answer>, Returned) {
    let executions: Vec<_> = sessions
        .iter_mut()
        .map(|session| session.run(&query.text))
        .collect();
    let reference = match &executions[0].outcome {
        Outcome::Rows(rows) => Some(rows.clone()),
        _ => None,
    };
    let types = sessions[0].column_types(&query.text);
    let mut answers = Vec::new();
    for (index, execution) in executions.iter().enumerate() {
        let mode = sessions[index].mode;
        let rows = match &execution.outcome {
            Outcome::Rows(rows) => Some(rows.len()),
            _ => None,
        };
        let mut contained = false;
        let verdict = match (&execution.outcome, &reference, &types) {
            (Outcome::Timeout, ..) => Verdict::Timeout,
            (Outcome::Error(error), ..) => Verdict::Error(error.clone()),
            (Outcome::Rows(_), None, _) => Verdict::NoReference,
            (Outcome::Rows(_), Some(_), _) if index == 0 => Verdict::Reference,
            (Outcome::Rows(_), Some(_), Err(error)) => {
                Verdict::Error(format!("the types of the query's columns: {error:#}"))
            }
            (Outcome::Rows(actual), Some(expected), Ok(types)) => {
                let kinds = kinds(types);
                match rule(mode, query) {
                    Some((reason, ruled)) => match sessions[0].run(&ruled.text).outcome {
                        Outcome::Rows(expected) => {
                            let (verdict, companion) =
                                judge(&mut sessions[0], mode, &ruled, &kinds, &expected, actual);
                            contained = companion;
                            match verdict {
                                Verdict::Same => Verdict::Known(reason.into()),
                                other => other,
                            }
                        }
                        outcome => {
                            Verdict::Error(format!("the query under DuckDB's rule: {outcome:?}"))
                        }
                    },
                    None => {
                        let (verdict, companion) =
                            judge(&mut sessions[0], mode, query, &kinds, expected, actual);
                        contained = companion;
                        verdict
                    }
                }
            }
        };
        answers.push(Answer {
            mode,
            verdict,
            rows,
            seconds: execution.elapsed.as_secs_f64(),
            contained,
        });
    }
    let returned = executions
        .into_iter()
        .map(|execution| match execution.outcome {
            Outcome::Rows(rows) => Some(rows),
            _ => None,
        })
        .collect();
    (answers, returned)
}

/// Each mode's rows of a query that does not return its sort key, judged
/// with the key: each row `i` paired with the key of row `i` of Tessera
/// off's answer to the widened query, which returns it after the query's
/// own columns. A right answer's keys are those, in that order, whatever
/// rows of a tie it took; a row of a later key is not in the widened
/// query's full result beside an earlier key.
fn paired(sessions: &mut [Session], widened: &Query, returned: &Returned) -> Vec<Answer> {
    let reference = sessions[0].run(&widened.text).outcome;
    let types = sessions[0].column_types(&widened.text);
    let mut answers = Vec::new();
    for (index, rows) in returned.iter().enumerate() {
        let mode = sessions[index].mode;
        let mut contained = false;
        let verdict = match (rows, &reference, &types) {
            (None, ..) => Verdict::Same,
            (Some(_), Outcome::Rows(_), Err(error)) => Verdict::Error(format!(
                "the types of the widened query's columns: {error:#}"
            )),
            (Some(rows), Outcome::Rows(expected), Ok(types)) => {
                if rows.len() != expected.len() {
                    Verdict::Differs(format!("{} rows, expected {}", rows.len(), expected.len()))
                } else {
                    let actual = pair(rows, expected);
                    let (verdict, companion) = judge(
                        &mut sessions[0],
                        mode,
                        widened,
                        &kinds(types),
                        expected,
                        &actual,
                    );
                    contained = companion;
                    verdict
                }
            }
            (Some(_), outcome, _) => {
                Verdict::Error(format!("the widened query in Tessera off: {outcome:?}"))
            }
        };
        answers.push(Answer {
            mode,
            verdict,
            rows: None,
            seconds: 0.0,
            contained,
        });
    }
    answers
}

/// The rule of DuckDB pg_duckdb's answer to `query` follows, where it
/// has one, and the query under it.
fn rule(mode: Mode, query: &Query) -> Option<(&'static str, Query)> {
    let &(_, reason, from, to) = DUCKDB_RULES
        .iter()
        .find(|&&(number, ..)| mode == Mode::Duckdb && number == query.number)?;
    let mut ruled = query.clone();
    ruled.text = query.text.replace(from, to);
    ruled.full = query.full.as_ref().map(|full| full.replace(from, to));
    Some((reason, ruled))
}

/// Rows of a query paired, row by row, with the columns the widened
/// query's rows `keyed` return after them.
fn pair(rows: &[Row], keyed: &[Row]) -> Vec<Row> {
    rows.iter()
        .zip(keyed)
        .map(|(row, keyed)| {
            let mut row = row.clone();
            row.extend(keyed[row.len().min(keyed.len())..].iter().cloned());
            row
        })
        .collect()
}

/// Folds the answers to a query checked beside another into the latter's:
/// a failure of the former fails the latter, named by `what`.
fn fold(answers: &mut [Answer], more: Vec<Answer>, what: &str) {
    for (answer, extra) in answers.iter_mut().zip(more) {
        if answer.verdict.passed() && !extra.verdict.passed() {
            answer.verdict = match extra.verdict {
                Verdict::Differs(detail) => Verdict::Differs(format!("{what}: {detail}")),
                Verdict::NotInFull(detail) => Verdict::NotInFull(format!("{what}: {detail}")),
                Verdict::Error(error) => Verdict::Error(format!("{what}: {error}")),
                other => other,
            };
        }
        answer.contained |= extra.contained;
    }
}

/// Runs a companion query in Tessera off's session.
fn lacking(off: &mut Session, sql: &str) -> Verdict {
    match off.run(sql).outcome {
        Outcome::Rows(rows) => match rows.first().and_then(|row| row[0].as_deref()) {
            Some("0") => Verdict::Same,
            Some(count) => Verdict::NotInFull(format!("{count} rows")),
            None => Verdict::Error("the companion query returned no count".into()),
        },
        Outcome::Timeout => Verdict::Error("the companion query timed out".into()),
        Outcome::Error(error) => Verdict::Error(format!("the companion query: {error}")),
    }
}

/// EXPLAIN (COSTS OFF) of `query` in `session`, appended to `plan` under
/// a line that names them.
pub fn explain(session: &mut Session, query: &Query, plan: &mut String) {
    let _ = writeln!(plan, "@@ {} {}", query.name(), session.mode);
    match session
        .run(&format!("EXPLAIN (COSTS OFF) {}", query.text))
        .outcome
    {
        Outcome::Rows(rows) => {
            for row in rows {
                let _ = writeln!(plan, "{}", row[0].as_deref().unwrap_or_default());
            }
        }
        outcome => {
            let _ = writeln!(plan, "{outcome:?}");
        }
    }
}

/// Checks `query` in every session, `sessions[0]` Tessera off's; `plans`
/// gets the query's plan in each mode. The rows of a query whose sort key
/// it does not return are checked with the key too ([`paired`]); a query
/// whose OFFSET skips every row, without its LIMIT and OFFSET, so that
/// its rows are compared at all.
pub fn check(sessions: &mut [Session], query: &Query, plans: &mut [String]) -> QueryCheck {
    for (session, plan) in sessions.iter_mut().zip(plans.iter_mut()) {
        explain(session, query, plan);
    }
    let (mut checked, returned) = answers(sessions, query);
    if let Some(widened) = query.widened() {
        let more = paired(sessions, &widened, &returned);
        fold(&mut checked, more, "with its sort key");
    }
    if query.offset > 0
        && checked[0].rows == Some(0)
        && let Some(full) = &query.full
    {
        let whole = Query {
            text: full.clone(),
            limit: None,
            offset: 0,
            full: None,
            ..query.clone()
        };
        let (more, _) = answers(sessions, &whole);
        fold(&mut checked, more, "without its LIMIT");
    }
    QueryCheck {
        query: query.number,
        answers: checked,
    }
}

/// results.txt: a line per query and mode.
pub fn results(checks: &[QueryCheck], heading: &str) -> String {
    let mut text = format!("{heading}\n");
    for check in checks {
        for answer in &check.answers {
            let _ = writeln!(
                text,
                "Q{:<3} {:<7} {:>8} rows {:>9.3} s  {}{}",
                check.query,
                answer.mode.name(),
                answer.rows.map_or("-".into(), |rows| rows.to_string()),
                answer.seconds,
                answer.verdict.label(),
                if answer.contained {
                    ", rows past the cut among the full result's"
                } else {
                    ""
                }
            );
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_companion_compares_the_rows_as_text_or_rounded() {
        let rows = vec![
            vec![Some("it's".to_string()), Some("3.5".to_string())],
            vec![None, None],
        ];
        let sql = companion("SELECT a, avg(b) FROM t GROUP BY a", &rows, &[false, true]);
        let round = |value: &str| {
            format!(
                "round(({value})::numeric, \
                 coalesce((8 - floor(log(abs(nullif(({value})::numeric, 0)))))::int, 0))"
            )
        };
        assert_eq!(
            sql,
            format!(
                "SELECT count(*) FROM (VALUES ('it''s'::text, {}), (NULL::text, {}) EXCEPT ALL \
                 SELECT f.c1::text, {} FROM (SELECT a, avg(b) FROM t GROUP BY a) \
                 AS f(c1, c2)) AS lacking",
                round("'3.5'::text"),
                round("NULL::text"),
                round("f.c2")
            )
        );
    }

    #[test]
    fn rows_paired_with_the_keys_show_a_phrase_of_another_time() {
        let row = |values: &[&str]| -> Row { values.iter().map(|v| Some(v.to_string())).collect() };
        // Tessera off's widened answer: two phrases at second 1, one at 2,
        // and the cut at second 3, where more rows tie.
        let keyed = vec![
            row(&["a", "1"]),
            row(&["b", "1"]),
            row(&["c", "2"]),
            row(&["d", "3"]),
        ];
        let shape = Shape {
            kinds: &[Kind::Exact, Kind::Exact],
            keys: &[1],
            limit: Some(4),
            offset: 0,
        };
        let judged = |phrases: &[&str]| {
            let rows: Vec<Row> = phrases.iter().map(|p| row(&[p])).collect();
            compare(&keyed, &pair(&rows, &keyed), &shape, Precision::Exact)
        };
        // A tie in another order, and another row of the cut's tie, which
        // the companion then looks for: both pass the comparison.
        assert_eq!(judged(&["b", "a", "c", "d"]), Ok(()));
        assert_eq!(judged(&["a", "b", "c", "x"]), Ok(()));
        assert_eq!(pair(&[row(&["x"])], &keyed[3..]), vec![row(&["x", "3"])]);
        // A phrase of a later time before the cut, a repeat, and the order
        // across times: each fails.
        assert!(judged(&["a", "b", "d", "c"]).is_err());
        assert!(judged(&["a", "a", "c", "d"]).is_err());
        assert!(judged(&["c", "a", "b", "d"]).is_err());
    }

    #[test]
    fn averages_are_close_and_the_rest_exact() {
        let types: Vec<String> = ["text", "numeric", "int8", "float8"]
            .iter()
            .map(|name| name.to_string())
            .collect();
        assert_eq!(
            kinds(&types),
            vec![Kind::Exact, Kind::Ratio, Kind::Exact, Kind::Ratio]
        );
        assert_eq!(precision(Mode::On), Precision::Exact);
        assert_eq!(precision(Mode::Duckdb), Precision::Close);
    }

    #[test]
    fn duckdb_s_rule_changes_the_query_it_is_known_for() {
        let queries =
            crate::queries::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
                .unwrap();
        let (_, ruled) = rule(Mode::Duckdb, &queries[28]).unwrap();
        assert!(
            ruled.text.contains("/[^' || chr(10) || ']*$'"),
            "{}",
            ruled.text
        );
        assert!(!ruled.text.contains("/.*$'"));
        assert!(ruled.unordered().unwrap().contains("chr(10)"));
        assert!(rule(Mode::On, &queries[28]).is_none());
        assert!(rule(Mode::Duckdb, &queries[27]).is_none());
    }

    #[test]
    fn only_the_reference_the_same_and_the_known_pass() {
        assert!(Verdict::Reference.passed());
        assert!(Verdict::Same.passed());
        assert!(Verdict::Known("Q29".into()).passed());
        for verdict in [
            Verdict::Differs("row 1".into()),
            Verdict::NotInFull("2 rows".into()),
            Verdict::Timeout,
            Verdict::Error("XX000".into()),
            Verdict::NoReference,
        ] {
            assert!(!verdict.passed(), "{verdict:?}");
        }
    }
}

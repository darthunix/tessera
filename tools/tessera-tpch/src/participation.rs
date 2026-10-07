//! Where Tessera took part: the plan of every query in both modes, run
//! once more under EXPLAIN (ANALYZE, TIMING OFF), with the Tessera nodes
//! counted and the rows read from the tables split into those TessHeapScan
//! read and the rest. With Tessera off no Tessera node may run.
//!
//! The planner's choice, unlike a time, does not depend on the machine's
//! noise, so the plans of the queries at SF 1 are kept in the repository
//! (bench/tpch/participation-sf1.txt) and every check is compared with
//! them.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::check::{QueryCheck, Sessions, statement};
use crate::queries;
use crate::session::{Outcome, Session};

/// What a plan shows of Tessera.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Participation {
    /// Tessera's nodes by name, with how many of each the plan has.
    pub nodes: BTreeMap<String, u32>,
    /// Every node of the plan.
    pub plan_nodes: u32,
    /// Rows every scan read: what it returned and what its filter
    /// removed, times its loops.
    pub scanned: u64,
    /// Of them, rows TessHeapScan read.
    pub tessera_scanned: u64,
    /// Rows TessFilter removed by its batch clauses, by a join's key
    /// filter (its Bloom filter and NULL keys) and by its clauses left to
    /// the core.
    pub batch_removed: u64,
    pub bloom_removed: u64,
    pub residual_removed: u64,
    /// Materialize nodes over a Tessera node: the core puts one there for
    /// a backward scan or mark and restore, which Tessera's nodes lack
    /// (plan 8.9).
    pub materialize_over_tessera: u32,
}

impl Participation {
    /// The share of the rows read that TessHeapScan read, in percent.
    pub fn share(&self) -> f64 {
        if self.scanned == 0 {
            0.0
        } else {
            100.0 * self.tessera_scanned as f64 / self.scanned as f64
        }
    }

    pub fn tessera_nodes(&self) -> u32 {
        self.nodes.values().sum()
    }

    /// The nodes as `TessAgg, 2 TessHashJoin, 3 TessHeapScan`.
    pub fn node_list(&self) -> String {
        if self.nodes.is_empty() {
            return "-".into();
        }
        self.nodes
            .iter()
            .map(|(name, &count)| {
                if count == 1 {
                    name.clone()
                } else {
                    format!("{count} {name}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Reads the output of EXPLAIN (FORMAT JSON).
    pub fn from_explain(json: &str) -> Result<Participation> {
        let value: Value = serde_json::from_str(json).context("EXPLAIN did not return JSON")?;
        let plan = value
            .get(0)
            .and_then(|top| top.get("Plan"))
            .context("EXPLAIN returned no plan")?;
        let mut participation = Participation::default();
        participation.walk(plan, None);
        Ok(participation)
    }

    fn walk(&mut self, node: &Value, parent: Option<&str>) {
        let number = |key: &str| node.get(key).and_then(Value::as_f64).unwrap_or(0.0);
        // EXPLAIN shows rows per loop; these are the totals.
        let loops = number("Actual Loops");
        let total = |key: &str| (number(key) * loops).round() as u64;
        let kind = node
            .get("Node Type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let provider = node
            .get("Custom Plan Provider")
            .and_then(Value::as_str)
            .filter(|provider| kind == "Custom Scan" && provider.starts_with("Tess"));
        self.plan_nodes += 1;
        let rows = total("Actual Rows");
        match (kind, provider) {
            (
                "Seq Scan" | "Index Scan" | "Index Only Scan" | "Bitmap Heap Scan" | "Tid Scan"
                | "Tid Range Scan",
                _,
            ) => {
                self.scanned +=
                    rows + total("Rows Removed by Filter") + total("Rows Removed by Index Recheck");
            }
            (_, Some("TessHeapScan")) => {
                self.scanned += rows;
                self.tessera_scanned += rows;
            }
            (_, Some("TessFilter")) => {
                self.batch_removed += total("Rows Removed by Batch Filter");
                self.bloom_removed +=
                    total("Rows Removed by Bloom Filter") + total("Rows Removed by NULL Key");
                self.residual_removed += total("Rows Removed by Residual Filter");
            }
            _ => {}
        }
        if let Some(provider) = provider {
            *self.nodes.entry(provider.to_string()).or_default() += 1;
            if parent == Some("Materialize") {
                self.materialize_over_tessera += 1;
            }
        }
        let name = provider.unwrap_or(kind);
        for child in node
            .get("Plans")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            self.walk(child, Some(name));
        }
    }
}

/// A query's plans in both modes; none for a mode whose execution failed
/// or ran out of time in the check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryParticipation {
    pub query: u8,
    pub on: Option<Participation>,
    pub off: Option<Participation>,
}

impl QueryParticipation {
    /// What is wrong in it: a Tessera node with Tessera off.
    pub fn fault(&self) -> Option<String> {
        let off = self.off.as_ref()?;
        (off.tessera_nodes() > 0).then(|| format!("Tessera off ran {}", off.node_list()))
    }

    /// The line the golden file keeps: the planner's choice with Tessera
    /// on, which the machine's noise does not change.
    pub fn golden_line(&self) -> Option<String> {
        let on = self.on.as_ref()?;
        Some(format!(
            "{}  share {:>3.0}%  {}",
            queries::get(self.query).name(),
            on.share(),
            on.node_list()
        ))
    }
}

/// The plan as EXPLAIN printed it, and what it shows.
fn explain(session: &mut Session<'_>, number: u8) -> Result<(String, Participation), String> {
    let sql = format!(
        "EXPLAIN (ANALYZE, TIMING OFF, FORMAT JSON) EXECUTE {}",
        statement(number)
    );
    match session.run(&sql).outcome {
        Outcome::Rows(rows) => {
            let json: String = rows
                .into_iter()
                .filter_map(|row| row.into_iter().next().flatten())
                .collect::<Vec<_>>()
                .join("\n");
            let participation =
                Participation::from_explain(&json).map_err(|error| format!("{error:#}"))?;
            Ok((json, participation))
        }
        Outcome::Timeout => Err("EXPLAIN ANALYZE ran out of time".into()),
        Outcome::Error(error) => Err(error),
    }
}

/// Explains every query whose execution in a mode answered in the check,
/// writing each plan to `plans` as `qNN-on.json` and `qNN-off.json`.
pub fn run(
    sessions: &mut Sessions<'_>,
    checks: &[QueryCheck],
    plans: &Path,
) -> Result<Vec<QueryParticipation>> {
    std::fs::create_dir_all(plans)?;
    let mut all = Vec::new();
    for check in checks {
        let mode = |session: &mut Session<'_>,
                    answered: bool,
                    name: &str|
         -> Result<Option<Participation>> {
            if !answered {
                return Ok(None);
            }
            match explain(session, check.query) {
                Ok((json, participation)) => {
                    let file = plans.join(format!("{}-{name}.json", statement(check.query)));
                    std::fs::write(&file, json)?;
                    Ok(Some(participation))
                }
                Err(error) => {
                    eprintln!("{} {name}: {error}", queries::get(check.query).name());
                    Ok(None)
                }
            }
        };
        let on = mode(&mut sessions.on, check.on_ms.is_some(), "on")?;
        let off = mode(&mut sessions.off, check.off_ms.is_some(), "off")?;
        all.push(QueryParticipation {
            query: check.query,
            on,
            off,
        });
    }
    Ok(all)
}

/// participation.txt: a line per query and mode.
pub fn table(all: &[QueryParticipation]) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "# scanned: rows every scan read, as it returned them plus those its filter removed, times its loops"
    );
    let _ = writeln!(
        text,
        "# via Tessera: rows TessHeapScan read, and their share of all"
    );
    let _ = writeln!(
        text,
        "# batch, bloom, residual: rows TessFilter removed by its batch clauses, a join's key filter, the clauses left to the core"
    );
    let _ = writeln!(
        text,
        "# materialize: Materialize nodes over a Tessera node, for a backward scan or mark/restore it lacks"
    );
    let _ = writeln!(
        text,
        "{:<5} {:<4} {:>11} {:>11} {:>5} {:>11} {:>11} {:>11} {:>5} {:>5}  Tessera nodes",
        "query",
        "mode",
        "scanned",
        "via Tessera",
        "share",
        "batch",
        "bloom",
        "residual",
        "mat.",
        "nodes"
    );
    for query in all {
        for (mode, participation) in [("on", &query.on), ("off", &query.off)] {
            let name = queries::get(query.query).name();
            match participation {
                None => {
                    let _ = writeln!(text, "{name:<5} {mode:<4} {:>11}", "-");
                }
                Some(p) => {
                    let _ = writeln!(
                        text,
                        "{name:<5} {mode:<4} {:>11} {:>11} {:>4.0}% {:>11} {:>11} {:>11} {:>5} {:>5}  {}",
                        p.scanned,
                        p.tessera_scanned,
                        p.share(),
                        p.batch_removed,
                        p.bloom_removed,
                        p.residual_removed,
                        p.materialize_over_tessera,
                        p.plan_nodes,
                        p.node_list()
                    );
                }
            }
        }
    }
    text
}

/// The golden file of a scale, schema and number of workers, for the
/// default work_mem; none for other settings, which plan differently.
pub fn golden_path(
    root: &Path,
    sf: &str,
    schema: &str,
    workers: u32,
    work_mem: &str,
) -> Option<PathBuf> {
    if work_mem != "256MB" {
        return None;
    }
    let mut name = format!("participation-sf{sf}");
    if schema != "pk" {
        name += &format!("-{schema}");
    }
    if workers > 0 {
        name += &format!("-w{workers}");
    }
    Some(root.join("bench/tpch").join(name + ".txt"))
}

const GOLDEN_HEADING: &str = "\
# The plans with Tessera on that tessera-tpch check expects: per query, the
# share of the rows read through TessHeapScan and the Tessera nodes.
# Rewritten by check --update-golden; a difference is reported, not failed.
";

fn read_golden(path: &Path) -> Result<BTreeMap<String, String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    let mut lines = BTreeMap::new();
    for line in text
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
    {
        let Some((query, _)) = line.split_once(' ') else {
            bail!("{}: a line without a query: {line}", path.display());
        };
        lines.insert(query.to_string(), line.to_string());
    }
    Ok(lines)
}

/// The differences from the golden file, as `(query, golden, now)`; a
/// query the file lacks differs from an empty line.
pub fn compare_golden(
    path: &Path,
    all: &[QueryParticipation],
) -> Result<Vec<(String, String, String)>> {
    let golden = read_golden(path)?;
    let mut differences = Vec::new();
    for query in all {
        let Some(line) = query.golden_line() else {
            continue;
        };
        let name = queries::get(query.query).name();
        let expected = golden.get(&name).cloned().unwrap_or_default();
        if expected != line {
            differences.push((name, expected, line));
        }
    }
    Ok(differences)
}

/// Writes this run's lines into the golden file, keeping the lines of
/// the queries it did not run.
pub fn update_golden(path: &Path, all: &[QueryParticipation]) -> Result<()> {
    let mut lines = read_golden(path)?;
    for query in all {
        if let Some(line) = query.golden_line() {
            lines.insert(queries::get(query.query).name(), line);
        }
    }
    let mut text = GOLDEN_HEADING.to_string();
    for line in lines.values() {
        text.push_str(line);
        text.push('\n');
    }
    std::fs::write(path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plan of the shape EXPLAIN (ANALYZE, TIMING OFF, FORMAT JSON)
    /// prints: a Tessera aggregate over a filter over a heap scan, and a
    /// core join reading another table, its inner side materialized.
    const PLAN: &str = r#"[
      {
        "Plan": {
          "Node Type": "Custom Scan",
          "Custom Plan Provider": "TessAgg",
          "Actual Rows": 4.00,
          "Actual Loops": 1,
          "Plans": [
            {
              "Node Type": "Custom Scan",
              "Custom Plan Provider": "TessFilter",
              "Actual Rows": 5900000.00,
              "Actual Loops": 1,
              "Rows Removed by Batch Filter": 101215,
              "Rows Removed by Residual Filter": 0,
              "Plans": [
                {
                  "Node Type": "Custom Scan",
                  "Custom Plan Provider": "TessHeapScan",
                  "Actual Rows": 6001215.00,
                  "Actual Loops": 1
                }
              ]
            },
            {
              "Node Type": "Nested Loop",
              "Actual Rows": 10.00,
              "Actual Loops": 1,
              "Plans": [
                {
                  "Node Type": "Seq Scan",
                  "Actual Rows": 2.50,
                  "Actual Loops": 4,
                  "Rows Removed by Filter": 20
                },
                {
                  "Node Type": "Materialize",
                  "Actual Rows": 5.00,
                  "Actual Loops": 2,
                  "Plans": [
                    {
                      "Node Type": "Custom Scan",
                      "Custom Plan Provider": "TessHeapScan",
                      "Actual Rows": 5.00,
                      "Actual Loops": 1
                    }
                  ]
                }
              ]
            }
          ]
        },
        "Planning Time": 0.1,
        "Execution Time": 900.0
      }
    ]"#;

    #[test]
    fn plans_count_rows_and_nodes() {
        let p = Participation::from_explain(PLAN).unwrap();
        assert_eq!(p.plan_nodes, 7);
        assert_eq!(p.tessera_nodes(), 4);
        assert_eq!(p.node_list(), "TessAgg, TessFilter, 2 TessHeapScan");
        // 6001215 + 5 through Tessera, 10 + 80 by the sequential scan.
        assert_eq!(p.tessera_scanned, 6_001_220);
        assert_eq!(p.scanned, 6_001_310);
        assert_eq!(p.batch_removed, 101_215);
        assert_eq!(p.materialize_over_tessera, 1);
        assert!((p.share() - 99.9985).abs() < 1e-3);
    }

    #[test]
    fn off_must_run_no_tessera_node() {
        let plan = Participation::from_explain(PLAN).unwrap();
        let query = QueryParticipation {
            query: 1,
            on: Some(plan.clone()),
            off: Some(plan),
        };
        assert_eq!(
            query.fault().as_deref(),
            Some("Tessera off ran TessAgg, TessFilter, 2 TessHeapScan")
        );
        assert_eq!(
            query.golden_line().as_deref(),
            Some("Q01  share 100%  TessAgg, TessFilter, 2 TessHeapScan")
        );
        let clean = QueryParticipation {
            off: Some(Participation::default()),
            ..query
        };
        assert_eq!(clean.fault(), None);
    }

    #[test]
    fn golden_files_keep_lines_and_report_differences() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("golden.txt");
        let plan = Participation::from_explain(PLAN).unwrap();
        let q1 = QueryParticipation {
            query: 1,
            on: Some(plan.clone()),
            off: None,
        };
        let q6 = QueryParticipation {
            query: 6,
            on: Some(Participation::default()),
            off: None,
        };
        assert_eq!(
            compare_golden(&path, std::slice::from_ref(&q1))
                .unwrap()
                .len(),
            1
        );
        update_golden(&path, std::slice::from_ref(&q1)).unwrap();
        update_golden(&path, std::slice::from_ref(&q6)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# The plans"));
        assert!(text.contains("Q01  share 100%"), "{text}");
        assert!(text.contains("Q06  share   0%  -"), "{text}");
        assert!(compare_golden(&path, &[q1.clone(), q6]).unwrap().is_empty());
        let changed = QueryParticipation {
            on: Some(Participation::default()),
            ..q1
        };
        let differences = compare_golden(&path, &[changed]).unwrap();
        assert_eq!(differences[0].0, "Q01");
        assert_eq!(differences[0].2, "Q01  share   0%  -");
    }

    #[test]
    fn golden_files_per_setting() {
        let root = Path::new("/r");
        assert_eq!(
            golden_path(root, "1", "pk", 0, "256MB"),
            Some(PathBuf::from("/r/bench/tpch/participation-sf1.txt"))
        );
        assert_eq!(
            golden_path(root, "10", "indexed", 2, "256MB"),
            Some(PathBuf::from(
                "/r/bench/tpch/participation-sf10-indexed-w2.txt"
            ))
        );
        assert_eq!(golden_path(root, "1", "pk", 0, "4MB"), None);
    }
}

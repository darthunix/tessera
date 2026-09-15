//! Group selected operations and collect a complete ABBA sequence per case.

use crate::report::Run;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// Run name and source index (0 = before, 1 = after).
pub const ORDER: [(&str, usize); 4] =
    [("before1", 0), ("after1", 1), ("after2", 1), ("before2", 0)];

#[derive(Serialize)]
pub struct Case {
    pub name: String,
    pub directory: String,
    pub paths: BTreeSet<String>,
}

impl Case {
    /// Match only selected full IDs, including their operation suffixes.
    pub fn filter(&self) -> String {
        let mut alternatives = Vec::new();
        for path in &self.paths {
            let mut literal = String::new();
            for ch in path.chars() {
                if r"\.+*?()|[]{}^$".contains(ch) {
                    literal.push('\\');
                }
                literal.push(ch);
            }
            alternatives.push(literal);
        }
        format!("^(?:{})$", alternatives.join("|"))
    }
}

pub fn group(bench: &str, paths: BTreeSet<String>) -> Result<Vec<Case>> {
    ensure!(!paths.is_empty(), "no benchmark cases matched");
    let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for path in paths {
        let (name, _) = path.rsplit_once('/').context("invalid benchmark name")?;
        groups.entry(name.to_owned()).or_default().insert(path);
    }
    Ok(groups
        .into_iter()
        .enumerate()
        .map(|(index, (name, paths))| Case {
            name,
            directory: format!("{bench}-case-{:04}", index + 1),
            paths,
        })
        .collect())
}

/// Keep scheduling independent of the process runner so its order is testable.
pub fn measure(
    cases: &[Case],
    mut collect: impl FnMut(&Case, &str, usize) -> Result<Run>,
) -> Result<[Run; 4]> {
    ensure!(!cases.is_empty(), "no benchmark cases to measure");
    let mut runs = std::array::from_fn(|_| Run::new());
    for case in cases {
        for (run, (name, side)) in runs.iter_mut().zip(ORDER) {
            let measured = collect(case, name, side)?;
            ensure!(
                measured.keys().eq(case.paths.iter()),
                "incomplete or unexpected results for {}: {name}",
                case.name
            );
            for (id, entry) in measured {
                ensure!(
                    run.insert(id.clone(), entry).is_none(),
                    "duplicate benchmark result: {id}"
                );
            }
        }
    }
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Bounds, Entry};

    fn selection() -> BTreeSet<String> {
        ["dense/case", "dense/case-extra", "datum/case"]
            .into_iter()
            .flat_map(|name| ["fold", "reference"].map(|path| format!("reader/{name}/{path}")))
            .collect()
    }

    fn measured(case: &Case, ns: f64) -> Run {
        case.paths
            .iter()
            .map(|id| {
                (
                    id.clone(),
                    Entry {
                        group: case.name.clone(),
                        path: id.rsplit_once('/').unwrap().1.to_owned(),
                        time: Bounds {
                            point: ns,
                            low: ns,
                            high: ns,
                        },
                    },
                )
            })
            .collect()
    }

    #[test]
    fn grouping_preserves_the_selection_and_separates_representations() -> Result<()> {
        let cases = group("reader", selection())?;
        assert_eq!(
            cases
                .iter()
                .map(|case| case.name.as_str())
                .collect::<Vec<_>>(),
            [
                "reader/datum/case",
                "reader/dense/case",
                "reader/dense/case-extra"
            ]
        );
        assert_eq!(cases[0].directory, "reader-case-0001");
        assert_eq!(cases[2].directory, "reader-case-0003");
        assert_eq!(
            cases
                .iter()
                .flat_map(|case| case.paths.iter().cloned())
                .collect::<BTreeSet<_>>(),
            selection()
        );
        assert_eq!(
            cases[1].filter(),
            "^(?:reader/dense/case/fold|reader/dense/case/reference)$"
        );
        assert!(group("reader", BTreeSet::new()).is_err());
        assert!(group("reader", BTreeSet::from(["invalid".into()])).is_err());
        Ok(())
    }

    #[test]
    fn filters_escape_regex_characters() -> Result<()> {
        let paths = BTreeSet::from([r"reader/\.+*?()|[]{}^$/fold".into()]);
        let cases = group("reader", paths)?;
        assert_eq!(
            cases[0].filter(),
            r"^(?:reader/\\\.\+\*\?\(\)\|\[\]\{\}\^\$/fold)$"
        );
        Ok(())
    }

    #[test]
    fn each_case_finishes_abba_before_the_next_and_results_stay_in_order() -> Result<()> {
        let cases = group("reader", selection())?;
        let mut calls = Vec::new();
        let runs = measure(&cases, |case, name, side| {
            calls.push((case.name.clone(), name.to_owned(), side));
            Ok(measured(case, calls.len() as f64))
        })?;
        for (index, case) in cases.iter().enumerate() {
            assert_eq!(
                &calls[index * 4..index * 4 + 4],
                &[
                    (case.name.clone(), "before1".into(), 0),
                    (case.name.clone(), "after1".into(), 1),
                    (case.name.clone(), "after2".into(), 1),
                    (case.name.clone(), "before2".into(), 0),
                ]
            );
            for (phase, run) in runs.iter().enumerate() {
                assert!(run.keys().eq(selection().iter()));
                for path in &case.paths {
                    assert_eq!(run[path].time.point, (index * 4 + phase + 1) as f64);
                }
            }
        }
        Ok(())
    }

    #[test]
    fn incomplete_duplicate_and_failed_runs_are_rejected() -> Result<()> {
        let mut cases = group("reader", selection())?;
        assert!(measure(&cases, |_, _, _| Ok(Run::new())).is_err());
        assert!(measure(&cases, |_, _, _| anyhow::bail!("process failed")).is_err());
        assert!(
            measure(&cases, |case, _, _| {
                let mut run = measured(case, 1.);
                let entry = run.remove(case.paths.first().unwrap()).unwrap();
                run.insert("unexpected".into(), entry);
                Ok(run)
            })
            .is_err()
        );
        cases.extend(group("reader", selection())?);
        assert!(measure(&cases, |case, _, _| Ok(measured(case, 1.))).is_err());
        assert!(measure(&[], |case, _, _| Ok(measured(case, 1.))).is_err());
        Ok(())
    }
}

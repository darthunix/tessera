//! Group selected operations and collect a complete ABBA sequence per case.

use crate::report::Run;
use anyhow::{Context, Result, anyhow, ensure};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    thread,
};

/// Run name and source index (0 = before, 1 = after).
pub const ORDER: [(&str, usize); 4] =
    [("before1", 0), ("after1", 1), ("after2", 1), ("before2", 0)];

#[derive(Serialize)]
pub struct Case {
    pub bench: String,
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
            bench: bench.to_owned(),
            name,
            directory: format!("{bench}-case-{:04}", index + 1),
            paths,
        })
        .collect())
}

/// Share one queue across benchmarks; each worker owns a whole ABBA sequence.
/// The caller merges results only after all workers and their children finish.
pub fn measure(
    cases: &[Case],
    jobs: NonZeroUsize,
    collect: impl Fn(&Case, &str, usize) -> Result<Run> + Sync,
) -> Result<BTreeMap<String, [Run; 4]>> {
    ensure!(!cases.is_empty(), "no benchmark cases to measure");
    let next = AtomicUsize::new(0);
    let cancelled = AtomicBool::new(false);
    let measured = thread::scope(|scope| -> Result<Vec<_>> {
        let mut workers = Vec::new();
        let mut error = None;
        for _ in 0..jobs.get().min(cases.len()) {
            let worker = thread::Builder::new().spawn_scoped(scope, || {
                let result = (|| -> Result<Vec<_>> {
                    let mut completed = Vec::new();
                    while !cancelled.load(Relaxed) {
                        let Some(case) = cases.get(next.fetch_add(1, Relaxed)) else {
                            break;
                        };
                        let mut runs: [Run; 4] = std::array::from_fn(|_| Run::new());
                        for (run, (name, side)) in runs.iter_mut().zip(ORDER) {
                            if cancelled.load(Relaxed) {
                                return Ok(completed);
                            }
                            *run = collect(case, name, side)
                                .with_context(|| format!("{}: {name}", case.name))?;
                            ensure!(
                                run.keys().eq(case.paths.iter()),
                                "incomplete or unexpected results for {}: {name}",
                                case.name
                            );
                        }
                        completed.push((case, runs));
                    }
                    Ok(completed)
                })();
                if result.is_err() {
                    cancelled.store(true, Relaxed);
                }
                result
            });
            match worker {
                Ok(worker) => workers.push(worker),
                Err(cause) => {
                    cancelled.store(true, Relaxed);
                    error = Some(anyhow!(cause).context("cannot start benchmark worker"));
                    break;
                }
            }
        }
        let mut completed = Vec::new();
        for worker in workers {
            match worker
                .join()
                .unwrap_or_else(|_| Err(anyhow!("benchmark worker panicked")))
            {
                Ok(results) => completed.extend(results),
                Err(cause) => {
                    cancelled.store(true, Relaxed);
                    error.get_or_insert(cause);
                }
            }
        }
        if let Some(error) = error {
            return Err(error);
        }
        ensure!(completed.len() == cases.len(), "incomplete benchmark run");
        Ok(completed)
    })?;
    // Ordered maps keep reports deterministic even when cases finish out of order.
    let mut benchmarks = BTreeMap::new();
    for (case, measured) in measured {
        let runs = benchmarks
            .entry(case.bench.clone())
            .or_insert_with(|| std::array::from_fn(|_| Run::new()));
        for (run, measured) in runs.iter_mut().zip(measured) {
            for (id, entry) in measured {
                ensure!(
                    run.insert(id.clone(), entry).is_none(),
                    "duplicate benchmark result: {id}"
                );
            }
        }
    }
    Ok(benchmarks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Bounds, Entry};
    use std::{
        sync::{Mutex, mpsc},
        time::Duration,
    };

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
        assert_eq!(cases[0].bench, "reader");
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
        let calls = Mutex::new(Vec::new());
        let runs = measure(&cases, NonZeroUsize::MIN, |case, name, side| {
            let mut calls = calls.lock().unwrap();
            calls.push((case.name.clone(), name.to_owned(), side));
            Ok(measured(case, calls.len() as f64))
        })?;
        let runs = &runs["reader"];
        let calls = calls.into_inner().unwrap();
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
        let jobs = NonZeroUsize::new(8).unwrap();
        assert!(measure(&cases, jobs, |_, _, _| Ok(Run::new())).is_err());
        assert!(measure(&cases, jobs, |_, _, _| anyhow::bail!("process failed")).is_err());
        assert!(
            measure(&cases, jobs, |case, _, _| {
                let mut run = measured(case, 1.);
                let entry = run.remove(case.paths.first().unwrap()).unwrap();
                run.insert("unexpected".into(), entry);
                Ok(run)
            })
            .is_err()
        );
        cases.extend(group("reader", selection())?);
        assert!(measure(&cases, jobs, |case, _, _| Ok(measured(case, 1.))).is_err());
        assert!(measure(&[], jobs, |case, _, _| Ok(measured(case, 1.))).is_err());
        Ok(())
    }

    #[test]
    fn workers_share_benchmarks_and_merge_out_of_order_results() -> Result<()> {
        for fail in [false, true] {
            let mut cases = group("reader", selection())?;
            cases.extend(group(
                "filter",
                BTreeSet::from(["filter/case/scalar".into(), "filter/case/reference".into()]),
            )?);
            let calls = Mutex::new(Vec::new());
            let active = AtomicUsize::new(0);
            let peak = AtomicUsize::new(0);
            let (finished_tx, finished_rx) = mpsc::channel();
            let (held_tx, held_rx) = mpsc::channel();
            let held_rx = Mutex::new(held_rx);
            let (release_tx, release_rx) = mpsc::channel();
            let release_rx = Mutex::new(release_rx);
            let result = thread::scope(|scope| -> Result<_> {
                let worker = scope.spawn(|| {
                    measure(&cases, NonZeroUsize::new(2).unwrap(), |case, name, side| {
                        let count = active.fetch_add(1, Relaxed) + 1;
                        peak.fetch_max(count, Relaxed);
                        calls
                            .lock()
                            .unwrap()
                            .push((case.name.clone(), name.to_owned(), side));
                        // Hold the first case while the other worker drains the shared queue,
                        // including the second benchmark. A sequential scheduler times out.
                        if case.directory == cases[0].directory && name == "before1" {
                            held_tx.send(())?;
                            release_rx
                                .lock()
                                .unwrap()
                                .recv_timeout(Duration::from_secs(5))?;
                        }
                        if case.directory == cases[1].directory && name == "before1" {
                            held_rx
                                .lock()
                                .unwrap()
                                .recv_timeout(Duration::from_secs(5))?;
                        }
                        let last_phase = case.bench == "filter" && name == "before2";
                        if last_phase {
                            finished_tx.send(())?;
                        }
                        active.fetch_sub(1, Relaxed);
                        ensure!(!(fail && last_phase), "injected failure");
                        Ok(measured(
                            case,
                            ORDER.iter().position(|&(phase, _)| phase == name).unwrap() as f64 + 1.,
                        ))
                    })
                });
                finished_rx.recv_timeout(Duration::from_secs(5))?;
                assert!(!worker.is_finished(), "must wait for the held case");
                release_tx.send(())?;
                worker.join().unwrap()
            });
            assert_eq!(peak.load(Relaxed), 2);
            assert_eq!(active.load(Relaxed), 0);
            if fail {
                assert!(format!("{:#}", result.err().unwrap()).contains("injected failure"));
                continue;
            }
            let runs = result?;
            assert_eq!(
                runs.keys().map(String::as_str).collect::<Vec<_>>(),
                ["filter", "reader"]
            );
            let calls = calls.into_inner().unwrap();
            assert_eq!(calls.len(), cases.len() * ORDER.len());
            for case in &cases {
                assert_eq!(
                    calls
                        .iter()
                        .filter(|(id, _, _)| id == &case.name)
                        .map(|(_, name, side)| (name.as_str(), *side))
                        .collect::<Vec<_>>(),
                    ORDER
                );
                for (phase, run) in runs[&case.bench].iter().enumerate() {
                    for path in &case.paths {
                        assert_eq!(run[path].time.point, phase as f64 + 1.);
                    }
                }
            }
            assert!(runs["reader"][0].keys().eq(selection().iter()));
        }
        Ok(())
    }

    #[test]
    fn failure_stops_later_phases_and_cases() -> Result<()> {
        let cases = group("reader", selection())?;
        let calls = Mutex::new(Vec::new());
        let result = measure(&cases, NonZeroUsize::MIN, |case, name, _| {
            calls
                .lock()
                .unwrap()
                .push((case.name.clone(), name.to_owned()));
            ensure!(name != "after1", "process failed");
            Ok(measured(case, 1.))
        });
        let error = result.err().unwrap();
        assert!(format!("{error:#}").contains("process failed"));
        assert_eq!(
            calls.into_inner().unwrap(),
            [
                (cases[0].name.clone(), "before1".into()),
                (cases[0].name.clone(), "after1".into()),
            ]
        );
        Ok(())
    }

    #[test]
    fn jobs_are_capped_by_the_case_count() -> Result<()> {
        let cases = group("reader", selection())?;
        let calls = AtomicUsize::new(0);
        let runs = measure(&cases[..1], NonZeroUsize::MAX, |case, _, _| {
            calls.fetch_add(1, Relaxed);
            Ok(measured(case, 1.))
        })?;
        assert_eq!(calls.load(Relaxed), ORDER.len());
        assert!(runs["reader"][0].keys().eq(cases[0].paths.iter()));
        Ok(())
    }
}

//! Deterministic tests of the benchmark machinery, not performance measurements.
//!
//! Check fixtures and reader sums, CLI selection, timing assessments with
//! synthetic samples, reports and saved runs without depending on clock noise.

#[path = "../benches/support/filtering.rs"]
#[allow(dead_code)]
mod filtering;
#[path = "../benches/support/reading.rs"]
#[allow(dead_code)]
mod reading;
#[path = "../benches/support/mod.rs"]
mod support;

use anyhow::Result;
use baseline::{Definition, Kind, Run};
use filtering::timing as filter_timing;
use measurement::{Assessment, Measurement, Policy, Ratios, SAMPLES, SERIES, Sample, Status};
use std::collections::BTreeMap;
use support::{baseline, fixture, measurement, options, reference, report, runner, sampling};
use tessera_core::ColumnReader;

#[test]
fn initialized_views_borrow_the_original_values_without_copying() {
    fn check<T: Copy + PartialEq + std::fmt::Debug>(values: &[T]) {
        let view = fixture::as_uninit(values);
        assert_eq!(view.as_ptr().cast::<T>(), values.as_ptr());
        assert_eq!(view.len(), values.len());
        for (slot, value) in view.iter().zip(values) {
            // SAFETY: as_uninit borrows these initialized values without mutation.
            assert_eq!(unsafe { slot.assume_init() }, *value);
        }
    }
    for nrows in [0, 1, 63, 64, 65, 1024] {
        let case = fixture::Fixture::from_values(
            (0..nrows).map(|row| row - 50).collect(),
            "all",
            "mixed",
            Some(7),
            true,
        );
        check(&case.values);
        check(&case.datums);
        check(&case.nulls);
        case.dense_column().unwrap();
        case.datum_column().unwrap();
    }
}

fn assert_sums<C: ColumnReader<Value = i32>>(
    input: &reading::Input<'_, C>,
    direct: impl Fn(&reading::Input<'_, C>) -> Result<i64>,
    expected: i64,
) {
    assert_eq!(direct(input).unwrap(), expected);
    assert_eq!(reading::fold_sum(input).unwrap(), expected);
    assert_eq!(reading::iter_sum(input).unwrap(), expected);
    assert_eq!(reading::word_sum(input).unwrap(), expected);
}

#[test]
fn reference_matches_scalar_model_and_bitmap_views() {
    let cases = reading::cases();
    assert_eq!(cases.len() * 2, 36);
    for case in cases {
        let rows = case.selected.reference();
        let prepared = case.prepared.as_ref().map(fixture::Bitmap::reference);
        let non_nulls = case.non_nulls.as_ref().map(fixture::Bitmap::reference);
        assert_eq!(
            reference::dense(&case.values, rows, prepared, non_nulls).unwrap(),
            case.expected
        );
        assert_eq!(
            reference::datum(&case.datums, &case.nulls, rows, prepared).unwrap(),
            case.expected
        );
        for index in 0..rows.nrows.div_ceil(64) {
            assert_eq!(rows.word(index), case.selected.view().word(index).unwrap());
        }
        let dense = case.dense_column().unwrap();
        let datum = case.datum_column().unwrap();
        assert_sums(
            &reading::Input::new(&dense, &case),
            reading::dense_reference,
            case.expected,
        );
        assert_sums(
            &reading::Input::new(&datum, &case),
            reading::datum_reference,
            case.expected,
        );
    }
}

#[test]
fn quick_cases_are_the_agreed_subset_of_the_unchanged_full_matrix() {
    let cases = reading::cases();
    let names: Vec<_> = cases.iter().map(|case| case.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "words/1024/all/nulls-none/ready",
            "words/1024/half/nulls-none/ready",
            "words/1024/sparse/nulls-none/ready",
            "words/1024/empty/nulls-none/ready",
            "words/1024/all/nulls-mixed/ready",
            "words/1024/half/nulls-mixed/ready",
            "words/1024/sparse/nulls-mixed/ready",
            "words/1024/empty/nulls-mixed/ready",
            "words/0/all/nulls-mixed/ready",
            "words/1/all/nulls-mixed/ready",
            "words/63/all/nulls-mixed/ready",
            "words/64/all/nulls-mixed/ready",
            "words/65/all/nulls-mixed/ready",
            "words/1024/all/nulls-all/ready",
            "words/1024/all/nulls-mixed/partial",
            "bytes-3/65/all/nulls-mixed/partial",
            "bytes-7/1024/all/nulls-mixed/partial",
            "bytes-7/1024/sparse/nulls-mixed/partial",
        ]
    );
    let quick_names: Vec<_> = cases
        .iter()
        .filter(|case| case.quick)
        .map(|case| case.name.as_str())
        .collect();
    assert_eq!(
        quick_names,
        [
            "words/1024/all/nulls-none/ready",
            "words/1024/sparse/nulls-none/ready",
            "words/1024/empty/nulls-none/ready",
            "words/1024/all/nulls-mixed/ready",
            "words/1024/all/nulls-all/ready",
            "words/1024/all/nulls-mixed/partial",
            "bytes-7/1024/sparse/nulls-mixed/partial",
        ]
    );
    for quick in quick_names {
        assert!(names.contains(&quick));
    }
}

fn sample(ratio: f64) -> Sample {
    Sample {
        before_ns: 100.,
        measured_ns: ratio * 100.,
        after_ns: 100.,
    }
}

fn measured(
    definition: Definition,
    reference: [f64; SERIES],
    measured: [f64; SERIES],
    control: [f64; SERIES],
) -> Measurement {
    let mut result = Measurement::default();
    for &path in definition.paths {
        for series in 0..SERIES {
            for index in 0..SAMPLES {
                result
                    .insert(
                        path,
                        series,
                        index,
                        Sample {
                            before_ns: reference[series],
                            after_ns: reference[series],
                            measured_ns: if path == "control" {
                                control[series]
                            } else {
                                measured[series]
                            },
                        },
                    )
                    .unwrap();
            }
        }
    }
    result
}

fn measurements(readers: [f64; SERIES], control: [f64; SERIES]) -> Measurement {
    measured(
        reading::DEFINITION,
        [100.; SERIES],
        readers.map(|r| r * 100.),
        control.map(|r| r * 100.),
    )
}

fn range(low: f64, median: f64, high: f64) -> Ratios {
    Ratios { low, median, high }
}

#[test]
fn paired_ratios_and_series_medians_use_all_samples() {
    let pair = Sample {
        before_ns: 80.,
        measured_ns: 100.,
        after_ns: 120.,
    };
    assert_eq!(pair.reference_ns(), 100.);
    assert_eq!(pair.ratio(), 1.);
    let mut data = Measurement::default();
    // Seven slow samples must be retained but must not replace the median.
    for series in 0..SERIES {
        for index in 0..SAMPLES {
            let ratio = if index < 8 {
                [0.9, 1., 1.1][series]
            } else {
                2.
            };
            data.insert("fold", series, index, sample(ratio)).unwrap();
        }
    }
    let summary = data.summary("fold").unwrap();
    assert_eq!(summary.ratios.low, 0.9);
    assert_eq!(summary.ratios.high, 1.1);
    assert!((summary.ratios.median - 1.1).abs() < 1e-12);
    assert!((summary.measured_ns - 110.).abs() < 1e-12);
    assert_eq!(summary.reference_ns, 100.);
    assert_eq!(data.sample("fold", 0, 14).unwrap(), sample(2.));
    assert!(data.summary("words").is_err());
}

#[test]
fn range_gates_are_inclusive_at_three_percent() {
    for (ratios, status) in [
        (range(0.9, 1., 1.02), Status::Pass),
        (range(1.03, 1.03, 1.03), Status::Pass),
        (range(1.031, 1.04, 1.05), Status::Fail),
        (range(1.02, 1.04, 1.05), Status::Unstable),
        (range(1.03, 1.03, 1.04), Status::Unstable),
    ] {
        assert_eq!(ratios.status(), status);
    }
    assert!(range(0.97, 1., 1.03).control_ok());
    assert!(!range(0.969, 1., 1.02).control_ok());
    assert!(!range(0.98, 1., 1.031).control_ok());
    assert!(!range(1.1, 1.1, 1.1).control_ok());
}

#[test]
fn baseline_comparison_uses_both_series_ranges() {
    let current = range(0.8, 0.81, 0.82);
    let previous = range(0.78, 0.79, 0.8);
    let change = current.relative_to(previous);
    assert_eq!(change.low, 0.8 / 0.8);
    assert_eq!(change.median, 0.81 / 0.79);
    assert_eq!(change.high, 0.82 / 0.78);
    let old = measurements([0.78, 0.79, 0.8], [1.; SERIES]);
    let data = measurements([0.8, 0.81, 0.82], [1.; SERIES]);
    let comparison = data.comparison("fold", Some(&old), Policy::Strict).unwrap();
    assert_eq!(comparison.ratios, change);
    assert_eq!(comparison.status, Status::Unstable);
}

#[test]
fn control_invalidates_results_and_valid_failures_take_precedence() {
    use Status::{Fail, Pass, Unstable};
    for (reference, previous, control, expected) in [
        (Fail, Some(Fail), false, Unstable),
        (Pass, None, false, Unstable),
        (Fail, Some(Unstable), true, Fail),
        (Unstable, Some(Fail), true, Fail),
        (Pass, Some(Fail), true, Fail),
        (Pass, Some(Unstable), true, Unstable),
        (Pass, Some(Pass), true, Pass),
        (Pass, None, true, Pass),
    ] {
        let assessment = Assessment::new(reference, previous, control);
        assert_eq!(assessment.status, expected);
        assert_eq!(
            assessment.reference,
            if control { reference } else { Unstable }
        );
        assert_eq!(
            assessment.previous,
            previous.map(|s| if control { s } else { Unstable })
        );
    }
    assert_eq!(
        Assessment::new(Pass, Some(Fail), true).reason,
        "slower than baseline"
    );
    assert_eq!(Unstable.to_string(), "UNSTABLE");
}

#[test]
fn samples_reject_invalid_numbers_indices_and_duplicates() {
    let mut data = Measurement::default();
    for invalid in [0., -1., f64::NAN, f64::INFINITY] {
        for value in [
            Sample {
                before_ns: invalid,
                ..sample(1.)
            },
            Sample {
                measured_ns: invalid,
                ..sample(1.)
            },
            Sample {
                after_ns: invalid,
                ..sample(1.)
            },
        ] {
            assert!(data.insert("fold", 0, 0, value).is_err());
        }
    }
    assert!(data.insert("fold", SERIES, 0, sample(1.)).is_err());
    assert!(data.insert("fold", 0, SAMPLES, sample(1.)).is_err());
    data.insert("fold", 0, 0, sample(1.)).unwrap();
    assert!(data.insert("fold", 0, 0, sample(2.)).is_err());
    assert_eq!(data.sample("fold", 0, 0).unwrap(), sample(1.));
    assert!(
        data.passes_reference(&reading::PATHS, measurement::Policy::Strict)
            .is_err()
    );
    assert!(
        !measurements([1.04; SERIES], [1.; SERIES])
            .passes_reference(&reading::PATHS, measurement::Policy::Strict)
            .unwrap()
    );
    assert!(
        !measurements([1.02, 1.03, 1.04], [1.; SERIES])
            .passes_reference(&reading::PATHS, measurement::Policy::Strict)
            .unwrap()
    );
    assert!(
        !measurements([0.8; SERIES], [0.96, 1., 1.])
            .passes_reference(&reading::PATHS, measurement::Policy::Strict)
            .unwrap()
    );
}

fn saved(definition: Definition, data: Measurement) -> Run {
    Run {
        kind: Kind::Baseline,
        definition,
        environment: "host".into(),
        diagnostic: false,
        previous: None,
        measurements: BTreeMap::from([("case".into(), data)]),
    }
}

fn temp_dir() -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "tessera-bench-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    directory
}

#[test]
fn saved_runs_round_trip_and_reject_corrupt_incomplete_or_nonpassing_data() {
    for (definition, reference, passing, limit) in [
        (reading::DEFINITION, 100., 80.123456789123, 103.),
        (filtering::DEFINITION, 2., 2.9, 3.),
    ] {
        let data = |values, control| measured(definition, [reference; SERIES], values, control);
        for kind in [Kind::Baseline, Kind::Results] {
            let directory = temp_dir();
            let file = directory.join("before.tsv");
            let mut baseline = saved(definition, data([passing; SERIES], [reference; SERIES]));
            baseline.kind = kind;
            baseline.previous = Some("before".into());
            baseline.diagnostic = kind == Kind::Results;
            baseline.save(&file).unwrap();
            let bytes = std::fs::read(&file).unwrap();
            let restored = Run::read(&file, "host", definition, kind).unwrap();
            assert_eq!(restored.kind, kind);
            assert_eq!(restored.previous, baseline.previous);
            assert_eq!(restored.diagnostic, baseline.diagnostic);
            for &path in definition.paths {
                for series in 0..SERIES {
                    for index in 0..SAMPLES {
                        assert_eq!(
                            baseline.measurements["case"]
                                .sample(path, series, index)
                                .unwrap(),
                            restored.measurements["case"]
                                .sample(path, series, index)
                                .unwrap()
                        );
                    }
                }
            }
            assert!(baseline.save(&file).is_err());
            assert_eq!(std::fs::read(&file).unwrap(), bytes);
            assert!(Run::read(&file, "other host", definition, kind).is_err());
            let other_kind = if kind == Kind::Baseline {
                Kind::Results
            } else {
                Kind::Baseline
            };
            assert!(Run::read(&file, "host", definition, other_kind).is_err());
            let other = if definition.name == reading::DEFINITION.name {
                filtering::DEFINITION
            } else {
                reading::DEFINITION
            };
            assert!(Run::read(&file, "host", other, kind).is_err());
            assert_ne!(
                baseline::path_for("before", definition, kind).unwrap(),
                baseline::path_for("before", other, kind).unwrap()
            );
            assert_ne!(
                baseline::path_for("before", definition, kind).unwrap(),
                baseline::path_for("before", definition, other_kind).unwrap()
            );
            restored.validate_case_set(&["case".into()]).unwrap();
            for names in [
                vec![],
                vec!["different".into()],
                vec!["case".into(), "missing".into()],
            ] {
                assert!(restored.validate_case_set(&names).is_err());
            }

            let text = String::from_utf8(bytes).unwrap();
            let lines: Vec<_> = text.lines().collect();
            let first = lines[4];
            let fields: Vec<_> = first.split('\t').collect();
            for (field, invalid) in [
                (0, ""),
                (1, "unknown"),
                (2, "3"),
                (3, "15"),
                (4, "NaN"),
                (5, "0"),
                (6, "inf"),
            ] {
                let mut bad = fields.clone();
                bad[field] = invalid;
                let corrupt = text.replacen(first, &bad.join("\t"), 1);
                assert!(Run::parse(&corrupt, "host", definition, kind).is_err());
            }
            for corrupt in [
                text.replacen(&definition.header(kind), "incompatible-benchmark", 1),
                text.replacen(lines[2], "mode\tunknown\t", 1),
                text.replacen(lines[2], "mode\tfull\t../before", 1),
                text.replacen(lines[2], "mode\tfull\tbefore\textra", 1),
                text.replacen(first, &fields[..6].join("\t"), 1),
                text.replacen(&format!("{first}\n"), "", 1),
                format!("{text}{first}\n"),
                lines[..4].join("\n"),
            ] {
                assert!(Run::parse(&corrupt, "host", definition, kind).is_err());
            }
            // Timing gates are independent of structural validity in both directions.
            let slow = text.replace(
                &format!("\t{reference}\t{passing}\t{reference}"),
                &format!("\t{reference}\t{}\t{reference}", limit + 0.1),
            );
            assert_eq!(
                Run::parse(&slow, "host", definition, kind).is_ok(),
                kind == Kind::Results
            );
            let rejected = directory.join("rejected.tsv");
            if kind == Kind::Baseline {
                let diagnostic = text.replacen("mode\tfull\t", "mode\tdiagnostic\t", 1);
                assert!(Run::parse(&diagnostic, "host", definition, kind).is_err());
                baseline.diagnostic = true;
                assert!(baseline.save(&rejected).is_err());
                assert!(!rejected.exists());
            }
            for slow in [
                data([limit + 0.1; SERIES], [reference; SERIES]),
                data([limit - 0.1, limit, limit + 0.1], [reference; SERIES]),
                data([passing; SERIES], [reference * 1.05; SERIES]),
            ] {
                let mut run = saved(definition, slow);
                run.kind = kind;
                assert_eq!(run.save(&rejected).is_ok(), kind == Kind::Results);
                if kind == Kind::Results {
                    Run::read(&rejected, "host", definition, kind).unwrap();
                    std::fs::remove_file(&rejected).unwrap();
                } else {
                    assert!(!rejected.exists());
                }
            }
            let mut empty = saved(definition, Measurement::default());
            empty.kind = kind;
            assert!(empty.save(&rejected).is_err());
            assert!(!rejected.exists());
            empty.measurements.clear();
            assert!(empty.save(&rejected).is_err());
            assert!(!rejected.exists());
            std::fs::remove_file(file).unwrap();
            std::fs::remove_dir(directory).unwrap();
        }
    }
}

fn configurations() -> [(Definition, Vec<fixture::Fixture>); 2] {
    [
        (reading::DEFINITION, reading::cases()),
        (filtering::DEFINITION, filtering::cases()),
    ]
}

fn parse(definition: Definition, args: &[&str]) -> Result<Option<options::Options>> {
    options::Options::parse(args.iter().map(|s| (*s).to_owned()), definition)
}

#[test]
fn cli_selection_and_complete_saved_case_sets_apply_to_both_benchmarks() {
    for (definition, cases) in configurations() {
        let directory = temp_dir();
        let saved_name = directory.file_name().unwrap().to_str().unwrap();
        assert!(parse(definition, &["--help"]).unwrap().is_none());
        let names: Vec<_> = runner::prepared_cases(&cases)
            .map(|case| (case.name, case.fixture.quick))
            .collect();
        let full = names.len();
        let help = options::Options::help(definition, full, 14);
        assert!(help.contains(&format!("{full} full cases")));
        assert!(help.contains("14 diagnostic cases"));
        assert!(help.contains("--save-results"));
        for (args, expected) in [
            (vec![], full),
            (
                vec!["--bench", "--baseline", "before", "--filter", "dense/"],
                full / 2,
            ),
            (vec!["--quick"], 14),
            (vec!["--quick", "--baseline", "before"], 14),
            (vec!["--quick", "--filter", "dense/"], 7),
            (
                vec!["--filter", "datum/", "--quick", "--baseline", "before"],
                7,
            ),
            (vec!["--filter", "/63/"], 2),
            (vec!["--quick", "--filter", "/63/"], 0),
            (vec!["--quick", "--filter", ""], 14),
        ] {
            // Every selection/history combination also supports saving raw results.
            let mut saving = args.clone();
            saving.extend(["--save-results", saved_name]);
            let options = parse(definition, &saving).unwrap().unwrap();
            assert_eq!(
                options.save_results.as_ref().unwrap(),
                &baseline::path_for(saved_name, definition, Kind::Results).unwrap()
            );
            assert_eq!(
                names
                    .iter()
                    .filter(|(name, quick)| options.matches(name, *quick))
                    .count(),
                expected,
                "{args:?}"
            );
            assert_eq!(
                options.is_diagnostic(),
                args.contains(&"--quick") || args.contains(&"--filter")
            );
            if args.contains(&"--baseline") {
                assert_eq!(
                    options.previous.unwrap(),
                    baseline::path_for("before", definition, Kind::Baseline).unwrap()
                );
            }
        }
        for args in [
            vec!["--baseline"],
            vec!["--filter"],
            vec!["--save-baseline"],
            vec!["--save-results"],
            vec!["--save-results", "../results"],
            vec!["--unknown"],
            vec!["--baseline", "../before"],
            vec!["--save-baseline", "before", "--filter", "dense"],
            vec!["--save-baseline", "before", "--filter", ""],
            vec!["--quick", "--save-baseline", "before"],
            vec!["--save-baseline", "before", "--quick"],
            vec!["--quick", "--filter", "dense/", "--save-baseline", "before"],
        ] {
            assert!(parse(definition, &args).is_err());
        }
        let both = parse(
            definition,
            &["--save-baseline", saved_name, "--save-results", saved_name],
        )
        .unwrap()
        .unwrap();
        assert!(both.save.is_some() && both.save_results.is_some());
        for name in ["", "../before", "/tmp/before", "a/b", "a b"] {
            for kind in [Kind::Baseline, Kind::Results] {
                assert!(baseline::path_for(name, definition, kind).is_err());
            }
        }
        let options = parse(definition, &["--quick", "--baseline", "before"])
            .unwrap()
            .unwrap();
        let data = measured(definition, [100.; SERIES], [80.; SERIES], [100.; SERIES]);
        let mut baseline = saved(definition, data.clone());
        baseline.measurements = names
            .iter()
            .map(|(name, _)| (name.clone(), data.clone()))
            .collect();
        let full_names: Vec<_> = names.iter().map(|(name, _)| name.clone()).collect();
        baseline.validate_case_set(&full_names).unwrap();
        baseline.measurements.retain(|name, _| {
            let quick = names.iter().find(|(n, _)| n == name).unwrap().1;
            options.matches(name, quick)
        });
        assert_eq!(baseline.measurements.len(), 14);
        assert!(baseline.validate_case_set(&full_names).is_err());
        for (flag, kind) in [
            ("--save-baseline", Kind::Baseline),
            ("--save-results", Kind::Results),
        ] {
            let path = baseline::path_for(saved_name, definition, kind).unwrap();
            let mut run = saved(definition, data.clone());
            run.kind = kind;
            run.save(&path).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            assert!(parse(definition, &[flag, saved_name]).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir(directory).unwrap();
    }
    assert_ne!(
        baseline::fingerprint(&["a", "b"]),
        baseline::fingerprint(&["ab"])
    );
    assert_ne!(
        baseline::fingerprint(&["a", "b"]),
        baseline::fingerprint(&["a", "changed"])
    );
}

#[test]
fn reports_and_saving_keep_diagnostics_separate_from_baseline_approval() {
    for (definition, _) in configurations() {
        let data =
            |values, control| measured(definition, [100.; SERIES], values, [control; SERIES]);
        let old = saved(definition, data([50.; SERIES], 100.));
        for (values, control, previous, status, reference_status) in [
            ([80.; SERIES], 100., None, Status::Pass, Status::Pass),
            ([110.; SERIES], 100., None, Status::Fail, Status::Fail),
            (
                [102., 103., 104.],
                100.,
                None,
                Status::Unstable,
                Status::Unstable,
            ),
            (
                [110.; SERIES],
                105.,
                None,
                Status::Unstable,
                Status::Unstable,
            ),
            ([80.; SERIES], 100., Some(&old), Status::Fail, Status::Pass),
        ] {
            for diagnostic in [false, true] {
                let directory = temp_dir();
                let raw = directory.join("results.tsv");
                let baseline = directory.join("baseline.tsv");
                let options = options::Options {
                    quick: diagnostic,
                    save_results: Some(raw.clone()),
                    save: (!diagnostic).then(|| baseline.clone()),
                    ..Default::default()
                };
                let mut run = saved(definition, data(values, control));
                run.kind = Kind::Results;
                run.diagnostic = diagnostic;
                run.previous = previous.map(|_| "before".into());
                let mut output = Vec::new();
                let result = runner::finish(run, &options, previous, &["case".into()], &mut output);
                assert_eq!(result.is_ok(), status == Status::Pass);
                let output = String::from_utf8(output).unwrap();
                assert!(output.starts_with("Saved results "));
                let count = definition.paths.len() - 1;
                let counts = [Status::Pass, Status::Fail, Status::Unstable]
                    .map(|s| if s == status { count } else { 0 });
                assert!(output.contains(&format!(
                    "{} PASS, {} FAIL, {} UNSTABLE paths in 1 cases; {} unstable controls.",
                    counts[0],
                    counts[1],
                    counts[2],
                    usize::from(control != 100.)
                )));
                for path in definition.paths.iter().filter(|&&p| p != "control") {
                    assert!(output.contains(&format!("{status} case/{path}: time=")));
                }
                assert!(output.contains(&format!("reference={reference_status} ")));
                assert!(output.contains(if previous.is_some() {
                    "baseline=FAIL "
                } else {
                    "baseline=not-compared"
                }));
                let restored = Run::read(&raw, "host", definition, Kind::Results).unwrap();
                assert_eq!(restored.diagnostic, diagnostic);
                assert_eq!(restored.previous.as_deref(), previous.map(|_| "before"));
                assert!(Run::read(&raw, "host", definition, Kind::Baseline).is_err());
                assert_eq!(baseline.exists(), !diagnostic && status == Status::Pass);
                if baseline.exists() {
                    Run::read(&baseline, "host", definition, Kind::Baseline).unwrap();
                    std::fs::remove_file(baseline).unwrap();
                }
                std::fs::remove_file(raw).unwrap();
                std::fs::remove_dir(directory).unwrap();
            }
        }
        let results = BTreeMap::from([
            ("pass".into(), data([80.; SERIES], 100.)),
            ("fail".into(), data([110.; SERIES], 100.)),
            ("unstable".into(), data([102., 103., 104.], 100.)),
            ("control".into(), data([80.; SERIES], 105.)),
        ]);
        let mut output = Vec::new();
        assert!(
            !report::print(
                &mut output,
                &results,
                None,
                definition.paths,
                definition.policy
            )
            .unwrap()
        );
        let count = definition.paths.len() - 1;
        assert!(String::from_utf8(output).unwrap().contains(&format!(
            "{count} PASS, {count} FAIL, {} UNSTABLE paths in 4 cases; 1 unstable controls.",
            2 * count
        )));
    }
}

#[test]
fn output_failure_keeps_raw_results_but_never_saves_a_baseline() {
    for (definition, _) in configurations() {
        let directory = temp_dir();
        let options = options::Options {
            save_results: Some(directory.join("results.tsv")),
            save: Some(directory.join("baseline.tsv")),
            ..Default::default()
        };
        let run = saved(
            definition,
            measured(definition, [100.; SERIES], [80.; SERIES], [100.; SERIES]),
        );
        let mut no_output: &mut [u8] = &mut [];
        assert!(runner::finish(run, &options, None, &["case".into()], &mut no_output).is_err());
        let raw = options.save_results.unwrap();
        Run::read(&raw, "host", definition, Kind::Results).unwrap();
        assert!(!options.save.unwrap().exists());
        std::fs::remove_file(raw).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}

fn filter_measurements(reference: [f64; SERIES], values: [f64; SERIES]) -> Measurement {
    measured(filtering::DEFINITION, reference, values, reference)
}

#[test]
fn filter_tolerance_requires_both_relative_and_absolute_slowdown() {
    use measurement::Policy;
    for (reference, measured, expected) in [
        (2., [2.9; SERIES], Status::Pass),
        (2., [3.; SERIES], Status::Pass),
        (2., [3.000001; SERIES], Status::Fail),
        (100., [103.; SERIES], Status::Pass),
        (100., [103.000001; SERIES], Status::Fail),
        (2., [2.9, 3., 3.1], Status::Unstable),
    ] {
        let data = filter_measurements([reference; SERIES], measured);
        let comparison = data.comparison("scalar", None, Policy::Filter).unwrap();
        assert_eq!(comparison.status, expected);
        assert!((comparison.delta_ns - (measured[1] - reference)).abs() < 1e-12);
        assert_eq!(comparison.ratios.median, measured[1] / reference);
    }
    let tiny = filter_measurements([2.; SERIES], [2.9; SERIES]);
    assert_eq!(
        tiny.comparison("scalar", None, Policy::Strict)
            .unwrap()
            .status,
        Status::Fail
    );
    assert!(
        tiny.passes_reference(&filtering::PATHS, Policy::Filter)
            .unwrap()
    );
    assert!(
        !tiny
            .passes_reference(&filtering::PATHS, Policy::Strict)
            .unwrap()
    );
}

#[test]
fn filter_history_normalizes_nanoseconds_and_keeps_both_series_ranges() {
    use measurement::Policy;
    let old = filter_measurements([2.; SERIES], [2.; SERIES]);
    let drift = filter_measurements([10.; SERIES], [10.8; SERIES]);
    let comparison = drift
        .comparison("scalar", Some(&old), Policy::Filter)
        .unwrap();
    assert_eq!(comparison.status, Status::Pass);
    assert!((comparison.delta_ns - 0.8).abs() < 1e-12);
    let old = filter_measurements([100.; SERIES], [100., 101., 102.]);
    let current = filter_measurements([100.; SERIES], [105.; SERIES]);
    let comparison = current
        .comparison("scalar", Some(&old), Policy::Filter)
        .unwrap();
    assert_eq!(comparison.ratios.low, 1.05 / 1.02);
    assert_eq!(comparison.ratios.high, 1.05);
    assert_eq!(comparison.status, Status::Unstable);
    let assessment = Assessment::new(Status::Pass, Some(Status::Fail), false);
    assert_eq!(assessment.status, Status::Unstable);
    assert_eq!(assessment.previous, Some(Status::Unstable));
}

#[test]
fn filter_cases_and_quick_selection_are_fixed() {
    let cases = filtering::cases();
    assert_eq!(cases.len() * 2, 48);
    assert_eq!(cases.iter().filter(|case| case.quick).count() * 2, 14);
    let names: std::collections::BTreeSet<_> = cases.iter().map(|case| case.name.clone()).collect();
    assert_eq!(names.len(), cases.len());
    let quick: Vec<_> = cases
        .iter()
        .filter(|case| case.quick)
        .map(|case| case.name.as_str())
        .collect();
    assert_eq!(
        quick,
        [
            "bytes-0/65/one-per128/nulls-none/ready",
            "bytes-0/65/one-per128/nulls-mixed/ready",
            "bytes-0/1024/all/nulls-none/ready",
            "bytes-0/1024/all/nulls-mixed/ready",
            "bytes-0/1024/one-per128/nulls-mixed/ready",
            "bytes-0/1024/empty/nulls-none/ready",
            "bytes-7/1024/one-per128/nulls-mixed/partial",
        ]
    );
}

fn assert_filter_model<C: ColumnReader<Value = i32>>(
    column: &C,
    case: &fixture::Fixture,
    reference: impl Fn(&filtering::Input<'_, C>, &mut [u64]) -> Result<()>,
) {
    use tessera_core::RowMask;
    use tessera_kernels::int32::CompareOp;
    for op in [
        CompareOp::Eq,
        CompareOp::Ne,
        CompareOp::Lt,
        CompareOp::Le,
        CompareOp::Gt,
        CompareOp::Ge,
    ] {
        for scalar in [i32::MIN, -1, 0, 1, i32::MAX] {
            let input = filtering::Input::new(column, case, op, scalar);
            let original = case.selected.words();
            let expected = filtering::expected(case, original, op, scalar);
            let mut direct = original.to_vec();
            reference(&input, &mut direct).unwrap();
            assert_eq!(direct, expected);
            let mut actual = original.to_vec();
            let mut mask = RowMask::try_new(case.values.len(), &mut actual).unwrap();
            filtering::scalar(&input, &mut mask).unwrap();
            filtering::scalar(&input, &mut mask).unwrap();
            assert_eq!(actual, expected);
        }
    }
}

#[test]
fn filter_references_match_both_readers_and_independent_model() {
    let mut cases = filtering::cases();
    for nulls in ["none", "mixed"] {
        cases.push(fixture::Fixture::from_values(
            vec![i32::MIN, i32::MAX, -1, 0, 1],
            "all",
            nulls,
            Some(7),
            false,
        ));
    }
    for case in cases {
        let dense = case.dense_column().unwrap();
        let datum = case.datum_column().unwrap();
        assert_filter_model(&dense, &case, filtering::dense_reference);
        assert_filter_model(&datum, &case, filtering::datum_reference);
    }
}

fn assert_filter_readiness_error<C: ColumnReader<Value = i32>>(
    column: &C,
    case: &fixture::Fixture,
    reference: impl Fn(&filtering::Input<'_, C>, &mut [u64]) -> Result<()>,
) {
    use tessera_core::RowMask;
    use tessera_kernels::int32::CompareOp;
    // Row 65 is unprepared. The first word may change, but the second must not.
    // Eq(0) removes the selected ready row 0, whose value is 42.
    let input = filtering::Input::new(column, case, CompareOp::Eq, 0);
    let mut direct = [1, 3];
    assert!(reference(&input, &mut direct).is_err());
    assert_eq!(direct, [0, 3]);
    let mut actual = [1, 3];
    let mut mask = RowMask::try_new(66, &mut actual).unwrap();
    assert!(filtering::scalar(&input, &mut mask).is_err());
    assert_eq!(actual, direct);
    // A repeated error must leave both remaining words unchanged.
    assert!(reference(&input, &mut direct).is_err());
    assert!(filtering::scalar(&input, &mut RowMask::try_new(66, &mut actual).unwrap()).is_err());
    assert_eq!(actual, [0, 3]);
    assert_eq!(actual, direct);
}

#[test]
fn filter_references_match_word_local_readiness_errors() {
    for offset in [None, Some(0), Some(7)] {
        let case = fixture::Fixture::from_values(vec![42; 66], "all", "mixed", offset, true);
        let dense = case.dense_column().unwrap();
        let datum = case.datum_column().unwrap();
        assert_filter_readiness_error(&dense, &case, filtering::dense_reference);
        assert_filter_readiness_error(&datum, &case, filtering::datum_reference);
    }
}

#[test]
fn timed_mask_blocks_restore_every_invocation_and_handle_empty_rows() {
    use tessera_core::RowMask;
    use tessera_kernels::int32::CompareOp;
    for nrows in [0, 1, 63, 64, 65, 1024] {
        let case = fixture::Fixture::from_values(vec![42; nrows], "all", "none", None, false);
        let column = tessera_core::ColumnView::try_new(&case.values, None).unwrap();
        let input = filtering::Input::new(&column, &case, CompareOp::Eq, 0);
        let mut blocks = filter_timing::Masks::new(nrows, case.selected.words()).unwrap();
        for count in [filter_timing::BLOCK_SIZE, 3, filter_timing::BLOCK_SIZE, 0] {
            let mut visited = 0;
            for words in blocks.reset(count) {
                assert_eq!(words, case.selected.words());
                filtering::scalar(&input, &mut RowMask::try_new(nrows, words).unwrap()).unwrap();
                assert!(words.iter().all(|&word| word == 0));
                visited += 1;
            }
            assert_eq!(visited, count);
        }
    }
    assert!(filter_timing::Masks::new(65, &[u64::MAX]).is_err());
    assert!(filter_timing::Masks::new(1, &[2]).is_err());
}

#[test]
fn changing_reference_speed_keeps_paired_ratios_and_filter_margins() {
    let mut data = Measurement::default();
    for series in 0..SERIES {
        for index in 0..SAMPLES {
            // Different majorities exceed 3% and 1 ns; only the last group
            // exceeds BOTH. Comparing separate medians would wrongly fail.
            let (reference, measured) = [(2., 2.9), (100., 102.), (1000., 1040.)][index / 5];
            data.insert(
                "scalar",
                series,
                index,
                Sample {
                    before_ns: reference,
                    measured_ns: measured,
                    after_ns: reference,
                },
            )
            .unwrap();
        }
    }
    let summary = data.summary("scalar").unwrap();
    assert_eq!(summary.ratios, range(1.04, 1.04, 1.04));
    assert_eq!(summary.measured_ns / summary.reference_ns, 1.02);
    assert_eq!(
        data.comparison("scalar", None, Policy::Strict)
            .unwrap()
            .status,
        Status::Fail
    );
    let filter = data.comparison("scalar", None, Policy::Filter).unwrap();
    assert_eq!(filter.delta_ns, 2.);
    assert_eq!(filter.status, Status::Pass);
    let previous = filter_measurements([2.; SERIES], [2.; SERIES]);
    let history = data
        .comparison("scalar", Some(&previous), Policy::Filter)
        .unwrap();
    assert_eq!(history.status, filter.status);
    assert_eq!(history.ratios, filter.ratios);
    assert_eq!(history.delta_ns, filter.delta_ns);
}

#[test]
fn shared_sampling_rotates_brackets_and_calibrates_only_once() {
    for definition in [reading::DEFINITION, filtering::DEFINITION] {
        let mut data = Measurement::default();
        let mut calibration = None;
        for series in 0..SERIES {
            let mut warmup = Vec::new();
            let mut calls = Vec::new();
            let reference = 100. * (series + 1) as f64;
            sampling::Series {
                index: series,
                calibration: &mut calibration,
                measurement: &mut data,
            }
            .collect(definition.paths, |path, iterations| {
                if iterations == 10_000 {
                    if warmup.len() < definition.paths.len() {
                        warmup.push(path);
                    }
                } else {
                    calls.push((path, iterations));
                }
                if path == "control" {
                    reference
                } else {
                    reference * 0.9
                }
            })
            .unwrap();
            let expected_warmup: Vec<_> = std::iter::once("control")
                .chain(definition.paths.iter().copied().filter(|&p| p != "control"))
                .collect();
            assert_eq!(warmup, expected_warmup);
            assert_eq!(calibration, Some(100_000)); // Later, slower references cannot recalibrate.
            let mut expected = Vec::new();
            for index in 0..SAMPLES {
                for step in 0..definition.paths.len() {
                    let path = definition.paths[(series + index + step) % definition.paths.len()];
                    expected.extend([("control", 100_000), (path, 100_000), ("control", 100_000)]);
                    let sample = data.sample(path, series, index).unwrap();
                    assert_eq!(sample.before_ns, reference);
                    assert_eq!(sample.after_ns, reference);
                    assert_eq!(
                        sample.measured_ns,
                        if path == "control" {
                            reference
                        } else {
                            reference * 0.9
                        }
                    );
                }
            }
            assert_eq!(calls, expected);
        }
        assert!(
            data.passes_reference(definition.paths, definition.policy)
                .unwrap()
        );
    }
}

#[test]
fn runner_traverses_complete_series_and_keeps_per_case_calibrations() {
    for (definition, cases) in configurations() {
        for args in [vec![], vec!["--quick", "--filter", "dense/"]] {
            let options = parse(definition, &args).unwrap().unwrap();
            let selected: Vec<_> = runner::prepared_cases(&cases)
                .filter(|case| options.matches(&case.name, case.fixture.quick))
                .collect();
            let names: Vec<_> = selected.iter().map(|case| case.name.clone()).collect();
            let mut visited = Vec::new();
            let results = runner::collect(
                selected,
                |case,
                 format,
                 sampling::Series {
                     index: series,
                     calibration,
                     measurement: data,
                 }| {
                    let name = format!("{format}/{}", case.name);
                    let index = names.iter().position(|n| n == &name).unwrap();
                    assert_eq!(
                        *calibration,
                        if series == 0 { None } else { Some(index + 10) }
                    );
                    *calibration = Some(index + 10);
                    for &path in definition.paths {
                        for index in 0..SAMPLES {
                            data.insert(path, series, index, sample(1.))?;
                        }
                    }
                    visited.push((series, name));
                    Ok(())
                },
            )
            .unwrap();
            let expected: Vec<_> = (0..SERIES)
                .flat_map(|series| names.iter().map(move |name| (series, name.clone())))
                .collect();
            assert_eq!(visited, expected);
            assert_eq!(results.len(), names.len());
            for data in results.values() {
                assert!(
                    data.passes_reference(definition.paths, definition.policy)
                        .unwrap()
                );
            }
        }
        let options = parse(definition, &["--filter", "no-such-case"])
            .unwrap()
            .unwrap();
        assert!(
            runner::collect(
                runner::prepared_cases(&cases)
                    .filter(|case| options.matches(&case.name, case.fixture.quick))
                    .collect(),
                |_, _, _| panic!("unselected case"),
            )
            .is_err()
        );
    }
}

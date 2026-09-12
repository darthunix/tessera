//! Deterministic tests of the benchmark machinery, not performance measurements.
//!
//! Check fixtures and reader sums, CLI selection, timing assessments with
//! synthetic samples, and baseline persistence without depending on clock noise.

#[path = "../benches/support/baseline.rs"]
#[allow(dead_code)]
mod baseline;
#[path = "../benches/support/fixture.rs"]
#[allow(dead_code)]
mod fixture;
#[path = "../benches/support/measurement.rs"]
mod measurement;
#[path = "../benches/support/options.rs"]
#[allow(dead_code)]
mod options;
#[path = "../benches/support/reading.rs"]
mod reading;
#[path = "../benches/support/reference.rs"]
mod reference;

use anyhow::Result;
use baseline::Baseline;
use measurement::{Assessment, Measurement, Path, Ratios, SAMPLES, SERIES, Sample, Status};
use std::collections::BTreeMap;
use tessera_capi::{DatumInt32Column, DenseInt32Column};
use tessera_core::ColumnReader;

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
    let cases = fixture::cases();
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
        let prepared = case.prepared.as_ref().map(fixture::Bitmap::view);
        let non_nulls = case.non_nulls.as_ref().map(fixture::Bitmap::view);
        // SAFETY: fixtures initialize all values and flags and stay immutable.
        let dense = unsafe { DenseInt32Column::try_new(&case.dense, non_nulls, prepared) }.unwrap();
        // SAFETY: the same initialization and lifetime guarantees hold.
        let datum =
            unsafe { DatumInt32Column::try_new(&case.datum_values, &case.isnull, prepared) }
                .unwrap();
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
    let cases = fixture::cases();
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

fn measurements(readers: [f64; SERIES], control: [f64; SERIES]) -> Measurement {
    let mut result = Measurement::default();
    for path in Path::ALL {
        for series in 0..SERIES {
            for index in 0..SAMPLES {
                let ratio = if path == Path::Control {
                    control[series]
                } else {
                    readers[series]
                };
                result.insert(path, series, index, sample(ratio)).unwrap();
            }
        }
    }
    result
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
            data.insert(Path::Fold, series, index, sample(ratio))
                .unwrap();
        }
    }
    let summary = data.summary(Path::Fold).unwrap();
    assert_eq!(summary.ratios.low, 0.9);
    assert_eq!(summary.ratios.high, 1.1);
    assert!((summary.ratios.median - 1.1).abs() < 1e-12);
    assert!((summary.measured_ns - 110.).abs() < 1e-12);
    assert_eq!(summary.reference_ns, 100.);
    assert_eq!(data.sample(Path::Fold, 0, 14).unwrap(), sample(2.));
    assert!(data.summary(Path::Words).is_err());
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
    let result = Assessment::new(current, Some(previous), true);
    assert_eq!(result.change, Some(change));
    assert_eq!(result.reference, Status::Pass);
    assert_eq!(result.previous, Some(Status::Unstable));
    assert_eq!(result.status, Status::Unstable);
    let result = Assessment::new(range(0.85, 0.85, 0.85), Some(range(0.8, 0.8, 0.8)), true);
    assert_eq!(result.reference, Status::Pass);
    assert_eq!(result.previous, Some(Status::Fail));
    assert_eq!(result.status, Status::Fail);
    assert_eq!(result.reason, "slower than baseline");
    let boundary = Assessment::new(range(1.03, 1.03, 1.03), Some(range(1., 1., 1.)), true);
    assert_eq!(boundary.status, Status::Pass);
}

#[test]
fn control_invalidates_results_and_valid_failures_take_precedence() {
    let bad_control = Assessment::new(range(1.5, 1.5, 1.5), Some(range(1., 1., 1.)), false);
    assert_eq!(bad_control.status, Status::Unstable);
    assert_eq!(bad_control.reference, Status::Unstable);
    assert_eq!(bad_control.previous, Some(Status::Unstable));
    let reference_fail =
        Assessment::new(range(1.04, 1.05, 1.06), Some(range(1., 1.02, 1.03)), true);
    assert_eq!(reference_fail.reference, Status::Fail);
    assert_eq!(reference_fail.previous, Some(Status::Unstable));
    assert_eq!(reference_fail.status, Status::Fail);
    let baseline_fail = Assessment::new(range(1.02, 1.03, 1.04), Some(range(0.9, 0.9, 0.9)), true);
    assert_eq!(baseline_fail.reference, Status::Unstable);
    assert_eq!(baseline_fail.previous, Some(Status::Fail));
    assert_eq!(baseline_fail.status, Status::Fail);
    assert_eq!(Status::Unstable.to_string(), "UNSTABLE");
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
            assert!(data.insert(Path::Fold, 0, 0, value).is_err());
        }
    }
    assert!(data.insert(Path::Fold, SERIES, 0, sample(1.)).is_err());
    assert!(data.insert(Path::Fold, 0, SAMPLES, sample(1.)).is_err());
    data.insert(Path::Fold, 0, 0, sample(1.)).unwrap();
    assert!(data.insert(Path::Fold, 0, 0, sample(2.)).is_err());
    assert_eq!(data.sample(Path::Fold, 0, 0).unwrap(), sample(1.));
    assert!(data.passes_reference().is_err());
    assert!(
        !measurements([1.04; SERIES], [1.; SERIES])
            .passes_reference()
            .unwrap()
    );
    assert!(
        !measurements([1.02, 1.03, 1.04], [1.; SERIES])
            .passes_reference()
            .unwrap()
    );
    assert!(
        !measurements([0.8; SERIES], [0.96, 1., 1.])
            .passes_reference()
            .unwrap()
    );
}

fn baseline_text(readers: [f64; SERIES], control: [f64; SERIES]) -> String {
    let data = measurements(readers, control);
    let mut text = "tessera-column-reader-baseline-v2\nhost\ncase\tpath\tseries\tsample\tbefore_ns\tmeasured_ns\tafter_ns\n".to_owned();
    for path in Path::ALL {
        for series in 0..SERIES {
            for index in 0..SAMPLES {
                let sample = data.sample(path, series, index).unwrap();
                text.push_str(&format!(
                    "case\t{}\t{series}\t{index}\t{}\t{}\t{}\n",
                    path.name(),
                    sample.before_ns,
                    sample.measured_ns,
                    sample.after_ns
                ));
            }
        }
    }
    text
}

#[test]
fn parser_rejects_old_corrupt_incomplete_and_nonpassing_baselines() {
    let good = baseline_text([0.8; SERIES], [1.; SERIES]);
    let baseline = Baseline::parse(&good, "host").unwrap();
    baseline.validate_case_set(&["case".into()]).unwrap();
    assert!(baseline.validate_case_set(&["different".into()]).is_err());
    assert!(baseline.validate_case_set(&[]).is_err());
    assert!(
        baseline
            .validate_case_set(&["case".into(), "missing".into()])
            .is_err()
    );
    assert!(Baseline::parse(&good, "different host").is_err());
    let old = good.replace("baseline-v2", "baseline-v1");
    assert!(
        Baseline::parse(&old, "host")
            .err()
            .unwrap()
            .to_string()
            .contains("v2")
    );
    for corrupt in [
        good.replace("case\tfold\t0\t0", "case\tunknown\t0\t0"),
        good.replace("case\tfold\t0\t0", "case\tfold\t3\t0"),
        good.replace("case\tfold\t0\t0", "case\tfold\t0\t15"),
        good.replace("case\tfold\t0\t0\t100\t80\t100\n", ""),
        good.replace(
            "case\tfold\t0\t0\t100\t80\t100",
            "case\tfold\t0\t0\tNaN\t80\t100",
        ),
        good.replace(
            "case\tfold\t0\t0\t100\t80\t100",
            "case\tfold\t0\t0\t100\t0\t100",
        ),
        good.replace(
            "case\tfold\t0\t0\t100\t80\t100",
            "case\tfold\t0\t0\t100\t80",
        ),
        format!("{good}case\tfold\t0\t0\t100\t80\t100\n"),
        good.lines().take(3).collect::<Vec<_>>().join("\n"),
        baseline_text([1.04; SERIES], [1.; SERIES]),
        baseline_text([1.02, 1.03, 1.04], [1.; SERIES]),
        baseline_text([0.8; SERIES], [0.96, 1., 1.]),
    ] {
        assert!(Baseline::parse(&corrupt, "host").is_err());
    }
}

#[test]
fn raw_samples_round_trip_without_overwrite_or_partial_save() {
    let directory = std::env::temp_dir().join(format!(
        "tessera-bench-v2-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("before.tsv");
    let mut baseline = Baseline {
        environment: "host".into(),
        measurements: BTreeMap::from([(
            "case".into(),
            measurements([0.80123456789123; SERIES], [1.; SERIES]),
        )]),
    };
    baseline.save(&path).unwrap();
    let saved = std::fs::read(&path).unwrap();
    let restored = Baseline::read(&path, "host").unwrap();
    for reading in Path::ALL {
        for series in 0..SERIES {
            for index in 0..SAMPLES {
                assert_eq!(
                    baseline.measurements["case"]
                        .sample(reading, series, index)
                        .unwrap(),
                    restored.measurements["case"]
                        .sample(reading, series, index)
                        .unwrap()
                );
            }
        }
    }
    assert!(baseline.save(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), saved);
    assert!(Baseline::read(&path, "other host").is_err());
    let rejected = directory.join("rejected.tsv");
    for measurement in [
        Measurement::default(),
        measurements([1.04; SERIES], [1.; SERIES]),
        measurements([1.02, 1.03, 1.04], [1.; SERIES]),
        measurements([0.8; SERIES], [1.04; SERIES]),
    ] {
        baseline.measurements.insert("case".into(), measurement);
        assert!(baseline.save(&rejected).is_err());
        assert!(!rejected.exists());
    }
    baseline.measurements.clear();
    assert!(baseline.save(&rejected).is_err());
    assert!(!rejected.exists());
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn cli_keeps_filters_diagnostic_and_rejects_invalid_arguments() {
    let parse = |args: &[&str]| options::Options::parse(args.iter().map(|arg| (*arg).to_owned()));
    assert!(parse(&["--help"]).unwrap().is_none());
    let full = parse(&[]).unwrap().unwrap();
    assert!(!full.is_diagnostic());
    assert!(full.matches("dense/words", false));
    assert!(full.matches("datum/words", true));
    let options = parse(&["--bench", "--baseline", "before", "--filter", "dense/"])
        .unwrap()
        .unwrap();
    assert!(options.is_diagnostic());
    assert!(options.matches("dense/words", false));
    assert!(!options.matches("datum/words", true));
    assert!(options.previous.unwrap().ends_with("before.tsv"));
    for args in [
        vec!["--baseline"],
        vec!["--filter"],
        vec!["--save-baseline"],
        vec!["--unknown"],
        vec!["--baseline", "../before"],
        vec!["--save-baseline", "before", "--filter", "dense"],
        vec!["--save-baseline", "before", "--filter", ""],
        vec!["--quick", "--save-baseline", "before"],
        vec!["--save-baseline", "before", "--quick"],
        vec!["--quick", "--filter", "dense/", "--save-baseline", "before"],
    ] {
        assert!(parse(&args).is_err());
    }
    for name in ["", "../before", "/tmp/before", "a/b", "a b"] {
        assert!(baseline::path_for(name).is_err());
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
fn quick_cli_selects_both_formats_and_intersects_filters() {
    let parse = |args: &[&str]| {
        options::Options::parse(args.iter().map(|arg| (*arg).to_owned()))
            .unwrap()
            .unwrap()
    };
    let cases = fixture::cases();
    let names: Vec<_> = cases
        .iter()
        .flat_map(|case| {
            ["dense", "datum"].map(|format| (format!("{format}/{}", case.name), case.quick))
        })
        .collect();
    for (args, expected) in [
        (vec![], 36),
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
        let options = parse(&args);
        assert_eq!(
            names
                .iter()
                .filter(|(name, quick)| options.matches(name, *quick))
                .count(),
            expected,
            "{args:?}"
        );
        if options.quick {
            assert!(options.is_diagnostic());
        }
        if args.contains(&"--baseline") {
            assert!(options.previous.unwrap().ends_with("before.tsv"));
        }
    }
}

#[test]
fn quick_comparisons_require_a_complete_saved_case_set() {
    let options = options::Options::parse(["--quick", "--baseline", "before"].map(str::to_owned))
        .unwrap()
        .unwrap();
    let cases = fixture::cases();
    let names: Vec<_> = cases
        .iter()
        .flat_map(|case| {
            ["dense", "datum"].map(|format| (format!("{format}/{}", case.name), case.quick))
        })
        .collect();
    let full_names: Vec<_> = names.iter().map(|(name, _)| name.clone()).collect();
    let data = measurements([0.8; SERIES], [1.; SERIES]);
    let mut baseline = Baseline {
        environment: "host".into(),
        measurements: full_names
            .iter()
            .map(|name| (name.clone(), data.clone()))
            .collect(),
    };
    baseline.validate_case_set(&full_names).unwrap();
    for (name, quick) in &names {
        if options.matches(name, *quick) {
            assert_eq!(
                baseline.measurements[name]
                    .summary(Path::Fold)
                    .unwrap()
                    .ratios
                    .status(),
                Status::Pass
            );
        } else {
            baseline.measurements.remove(name);
        }
    }
    assert_eq!(baseline.measurements.len(), 14);
    assert!(baseline.validate_case_set(&full_names).is_err());
}

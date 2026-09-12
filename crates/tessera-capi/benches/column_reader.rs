//! Performance checks for column reading and summation, not filtering.
//!
//! Compare three reader paths with an independent scalar reference and optional
//! saved measurements. Prepare inputs before timing, then collect paired samples
//! and a self-reference control. See README.md for full and quick workflows;
//! support modules separate fixtures, kernels, assessment, persistence, and CLI.
#[path = "support/baseline.rs"]
mod baseline;
#[path = "support/fixture.rs"]
mod fixture;
#[path = "support/measurement.rs"]
mod measurement;
#[path = "support/options.rs"]
mod options;
#[path = "support/reading.rs"]
mod reading;
#[path = "support/reference.rs"]
mod reference;

use anyhow::{Context, Result, ensure};
use baseline::Baseline;
use measurement::{Assessment, Measurement, Path, Ratios, SAMPLES, SERIES, Sample, Status};
use options::Options;
use reading::Input;
use std::collections::BTreeMap;
use std::hint::black_box;
use std::time::{Duration, Instant};
use tessera_capi::{DatumInt32Column, DenseInt32Column};
use tessera_core::ColumnReader;

fn time<C>(
    input: &Input<'_, C>,
    run: impl Fn(&Input<'_, C>) -> Result<i64>,
    iterations: usize,
) -> f64 {
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(run(black_box(input)).unwrap());
    }
    start.elapsed().as_secs_f64() * 1e9 / iterations as f64
}

fn measure_series<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    direct: impl Fn(&Input<'_, C>) -> Result<i64> + Copy,
    expected: i64,
    series: usize,
    calibration: &mut Option<usize>,
    measurement: &mut Measurement,
) -> Result<()> {
    ensure!(
        direct(input)? == expected,
        "reference result differs from the scalar fixture model"
    );
    ensure!(
        reading::fold_sum(input)? == expected,
        "fold reader result differs"
    );
    ensure!(
        reading::iter_sum(input)? == expected,
        "try_fold reader result differs"
    );
    ensure!(
        reading::word_sum(input)? == expected,
        "word reader result differs"
    );

    let warmup = Instant::now();
    let estimate = loop {
        let reference_ns = time(input, direct, 10_000);
        time(input, reading::fold_sum, 10_000);
        time(input, reading::iter_sum, 10_000);
        time(input, reading::word_sum, 10_000);
        if warmup.elapsed() >= Duration::from_millis(30) {
            break reference_ns;
        }
    };
    ensure!(
        estimate.is_finite() && estimate > 0.,
        "invalid calibration duration"
    );
    // Calibrate once per case, then keep the iteration count for all series.
    let iterations = *calibration
        .get_or_insert_with(|| (10_000_000. / estimate).ceil().clamp(1., 20_000_000.) as usize);
    for index in 0..SAMPLES {
        for step in 0..Path::ALL.len() {
            let path = Path::ALL[(series + index + step) % Path::ALL.len()];
            let before_ns = time(input, direct, iterations);
            let measured_ns = match path {
                Path::Fold => time(input, reading::fold_sum, iterations),
                Path::TryFold => time(input, reading::iter_sum, iterations),
                Path::Words => time(input, reading::word_sum, iterations),
                Path::Control => time(input, direct, iterations),
            };
            let after_ns = time(input, direct, iterations);
            measurement.insert(
                path,
                series,
                index,
                Sample {
                    before_ns,
                    measured_ns,
                    after_ns,
                },
            )?;
        }
    }
    Ok(())
}

fn command(program: &str, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new(program).args(args).output()?;
    ensure!(output.status.success(), "{program} failed");
    Ok(String::from_utf8(output.stdout)?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" "))
}

fn environment() -> Result<String> {
    let cpu = if cfg!(target_os = "macos") {
        command("sysctl", &["-n", "machdep.cpu.brand_string"])?
    } else {
        std::fs::read_to_string("/proc/cpuinfo")?
            .lines()
            .find(|line| line.starts_with("model name") || line.starts_with("Hardware"))
            .context("cannot identify CPU for a comparable baseline")?
            .to_owned()
    };
    let definition = baseline::fingerprint(&[
        include_str!("column_reader.rs"),
        include_str!("support/reference.rs"),
        include_str!("support/fixture.rs"),
        include_str!("support/reading.rs"),
        include_str!("support/measurement.rs"),
        include_str!("support/options.rs"),
        include_str!("support/baseline.rs"),
        include_str!("../../../Cargo.lock"),
        include_str!("../../../Cargo.toml"),
        include_str!("../Cargo.toml"),
        include_str!("../../tessera-core/Cargo.toml"),
    ]);
    Ok(format!(
        "{}|{}|{}|{}|{:?}|{:?}|{definition:016x}",
        std::env::consts::ARCH,
        command("uname", &["-sr"])?,
        cpu,
        command("rustc", &["-vV"])?,
        std::env::var("RUSTFLAGS").unwrap_or_default(),
        std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default()
    ))
}

fn format_ratios(ratios: Ratios) -> String {
    format!(
        "{:+.2}% [{:+.2}%, {:+.2}%]",
        (ratios.median - 1.) * 100.,
        (ratios.low - 1.) * 100.,
        (ratios.high - 1.) * 100.
    )
}

fn report(results: &BTreeMap<String, Measurement>, previous: Option<&Baseline>) -> Result<bool> {
    let mut passed = 0;
    let mut failed = 0;
    let mut unstable = 0;
    let mut bad_controls = 0;
    println!(
        "\nMedian times; paired-ratio median [min, max series medians]. These are not confidence intervals."
    );
    println!("Reference overhead and saved-baseline change are separate comparisons.");
    println!(
        "words includes copied-word bounds/padding validation; fold and try_fold use validated selection views."
    );
    for (name, measurement) in results {
        let control = measurement.summary(Path::Control)?;
        let control_ok = control.ratios.control_ok();
        bad_controls += usize::from(!control_ok);
        println!(
            "CONTROL {name}: {} ref={:.2}ns self={:.2}ns {}",
            if control_ok {
                Status::Pass
            } else {
                Status::Unstable
            },
            control.reference_ns,
            control.measured_ns,
            format_ratios(control.ratios)
        );
        for path in Path::READERS {
            let summary = measurement.summary(path)?;
            let old = previous
                .map(|old| old.measurements[name].summary(path))
                .transpose()?;
            let assessment = Assessment::new(summary.ratios, old.map(|old| old.ratios), control_ok);
            match assessment.status {
                Status::Pass => passed += 1,
                Status::Fail => failed += 1,
                Status::Unstable => unstable += 1,
            }
            let previous = match (assessment.previous, assessment.change) {
                (Some(status), Some(change)) => {
                    format!("baseline={status} {}", format_ratios(change))
                }
                _ => "baseline=not-compared".to_owned(),
            };
            println!(
                "{} {name}/{} time={:.2}ns ref={:.2}ns reference={} {}; {previous}; {}",
                assessment.status,
                path.name(),
                summary.measured_ns,
                summary.reference_ns,
                assessment.reference,
                format_ratios(summary.ratios),
                assessment.reason
            );
        }
    }
    println!(
        "\n{passed} PASS, {failed} FAIL, {unstable} UNSTABLE reading paths in {} cases; {bad_controls} unstable controls.",
        results.len()
    );
    Ok(failed == 0 && unstable == 0)
}

fn main() -> Result<()> {
    let Some(options) = Options::parse(std::env::args().skip(1))? else {
        println!("{}", options::HELP);
        return Ok(());
    };
    let environment = environment()?;
    let previous = options
        .previous
        .as_ref()
        .map(|path| Baseline::read(path, &environment))
        .transpose()?;
    let cases = fixture::cases();
    let named_cases: Vec<_> = cases
        .iter()
        .flat_map(|case| {
            ["dense", "datum"].map(|format| (format!("{format}/{}", case.name), case.quick))
        })
        .collect();
    // Validate saved runs against the full matrix, even for diagnostic reads.
    let names: Vec<_> = named_cases.iter().map(|(name, _)| name.clone()).collect();
    if let Some(previous) = &previous {
        previous.validate_case_set(&names)?;
    }
    let mut results: BTreeMap<_, _> = named_cases
        .into_iter()
        .filter(|(name, quick)| options.matches(name, *quick))
        .map(|(name, _)| (name, Measurement::default()))
        .collect();
    ensure!(!results.is_empty(), "no benchmark cases matched");
    let mut calibrations = BTreeMap::<String, Option<usize>>::new();

    if options.is_diagnostic() {
        println!(
            "DIAGNOSTIC RUN: {} of {} cases; not a complete performance check; no baseline can be saved.",
            results.len(),
            names.len()
        );
    } else {
        println!("FULL RUN: all {} cases.", names.len());
    }
    println!(
        "{SERIES} complete series x {SAMPLES} paired samples; 3% limit; self-reference control +/-3%."
    );
    println!(
        "Run on an idle machine. No retries or sample discards. Full run takes several minutes."
    );
    // Each series traverses the entire matrix before the next series begins.
    for series in 0..SERIES {
        for case in &cases {
            let prepared = case.prepared.as_ref().map(fixture::Bitmap::view);
            let non_nulls = case.non_nulls.as_ref().map(fixture::Bitmap::view);
            // SAFETY: fixtures initialize all values and flags, remain immutable,
            // and outlive both adapters and all timed invocations.
            let dense = unsafe { DenseInt32Column::try_new(&case.dense, non_nulls, prepared) }?;
            // SAFETY: the same fixture initialization and lifetime guarantees hold.
            let datum =
                unsafe { DatumInt32Column::try_new(&case.datum_values, &case.isnull, prepared) }?;
            for format in ["dense", "datum"] {
                let name = format!("{format}/{}", case.name);
                let Some(measurement) = results.get_mut(&name) else {
                    continue;
                };
                let calibration = calibrations.entry(name.clone()).or_default();
                if format == "dense" {
                    let input = Input::new(&dense, case);
                    measure_series(
                        &input,
                        reading::dense_reference,
                        case.expected,
                        series,
                        calibration,
                        measurement,
                    )?;
                } else {
                    let input = Input::new(&datum, case);
                    measure_series(
                        &input,
                        reading::datum_reference,
                        case.expected,
                        series,
                        calibration,
                        measurement,
                    )?;
                }
                println!("Series {}/{SERIES}: {name}", series + 1);
            }
        }
    }
    ensure!(
        report(&results, previous.as_ref())?,
        "FAIL or UNSTABLE results; no baseline saved (see separate reference and baseline comparisons above)"
    );
    if let Some(path) = options.save {
        let baseline = Baseline {
            environment,
            measurements: results,
        };
        baseline.validate_case_set(&names)?;
        baseline.save(&path)?;
        println!("Saved {}", path.display());
    }
    Ok(())
}

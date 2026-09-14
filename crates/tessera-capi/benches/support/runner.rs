//! Case selection and complete-series traversal, shared by benchmark executables.
//!
//! A benchmark callback prepares and validates one case and delegates its timed
//! segments to sampling. No operation is called indirectly inside its timer.

use super::{
    baseline::{self, Definition, Kind, Run},
    fixture::Fixture,
    measurement::{Measurement, SAMPLES, SERIES},
    options::Options,
    report,
    sampling::Series,
};
use anyhow::{Result, ensure};
use std::collections::BTreeMap;
use std::io::Write;

pub struct Case<'a> {
    pub name: String,
    pub fixture: &'a Fixture,
    format: &'static str,
    calibration: Option<usize>,
    measurement: Measurement,
}

pub fn prepared_cases(cases: &[Fixture]) -> impl Iterator<Item = Case<'_>> {
    cases.iter().flat_map(|case| {
        ["dense", "datum"].map(move |format| Case {
            name: format!("{format}/{}", case.name),
            fixture: case,
            format,
            calibration: None,
            measurement: Measurement::default(),
        })
    })
}

pub fn collect(
    mut cases: Vec<Case<'_>>,
    mut measure: impl FnMut(&Fixture, &str, Series<'_>) -> Result<()>,
) -> Result<BTreeMap<String, Measurement>> {
    ensure!(!cases.is_empty(), "no benchmark cases matched");
    for index in 0..SERIES {
        for case in &mut cases {
            measure(
                case.fixture,
                case.format,
                Series {
                    index,
                    calibration: &mut case.calibration,
                    measurement: &mut case.measurement,
                },
            )?;
            println!("Series {}/{SERIES}: {}", index + 1, case.name);
        }
    }
    Ok(cases
        .into_iter()
        .map(|case| (case.name, case.measurement))
        .collect())
}

pub fn run(
    definition: Definition,
    cases: Vec<Fixture>,
    sources: &[&str],
    measure: impl FnMut(&Fixture, &str, Series<'_>) -> Result<()>,
) -> Result<()> {
    let Some(options) = Options::parse(std::env::args().skip(1), definition)? else {
        println!(
            "{}",
            Options::help(
                definition,
                cases.len() * 2,
                cases.iter().filter(|c| c.quick).count() * 2
            )
        );
        return Ok(());
    };
    let environment = baseline::environment(sources)?;
    let previous = options
        .previous
        .as_ref()
        .map(|path| Run::read(path, &environment, definition, Kind::Baseline))
        .transpose()?;
    let mut selected: Vec<_> = prepared_cases(&cases).collect();
    let names: Vec<_> = selected.iter().map(|case| case.name.clone()).collect();
    if let Some(previous) = &previous {
        previous.validate_case_set(&names)?;
    }
    selected.retain(|case| options.matches(&case.name, case.fixture.quick));
    let count = selected.len();
    ensure!(count != 0, "no benchmark cases matched");
    if options.is_diagnostic() {
        println!(
            "DIAGNOSTIC RUN: {count} of {} cases; incomplete; no baseline can be saved.",
            names.len()
        );
    } else {
        println!("FULL RUN: all {count} cases.");
    }
    println!(
        "{SERIES} complete series x {SAMPLES} paired samples; {} limit; control +/-3%.",
        definition.policy
    );
    println!(
        "Run on an idle machine. No retries or discarded samples. Preparation is outside timing."
    );
    let run = Run {
        kind: Kind::Results,
        definition,
        environment,
        diagnostic: options.is_diagnostic(),
        previous: options
            .previous
            .as_ref()
            .and_then(|path| path.file_stem())
            .map(|name| name.to_string_lossy().into_owned()),
        measurements: collect(selected, measure)?,
    };
    finish(
        run,
        &options,
        previous.as_ref(),
        &names,
        &mut std::io::stdout().lock(),
    )
}

/// Persist complete raw data before reporting or returning a performance failure.
pub fn finish(
    mut run: Run,
    options: &Options,
    previous: Option<&Run>,
    names: &[String],
    out: &mut impl Write,
) -> Result<()> {
    if let Some(path) = &options.save_results {
        run.kind = Kind::Results;
        run.save(path)?;
        writeln!(out, "Saved results {}", path.display())?;
    }
    ensure!(
        report::print(
            out,
            &run.measurements,
            previous,
            run.definition.paths,
            run.definition.policy
        )?,
        "FAIL or UNSTABLE results; no baseline saved"
    );
    if let Some(path) = &options.save {
        run.kind = Kind::Baseline;
        run.validate_case_set(names)?;
        run.save(path)?;
        writeln!(out, "Saved baseline {}", path.display())?;
    }
    Ok(())
}

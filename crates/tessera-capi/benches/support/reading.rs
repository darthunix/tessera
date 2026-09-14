//! Timed summation kernels using each public column-reading path.
//!
//! Exercise bulk fold, short-circuiting try_fold, and copied-word iteration
//! separately: their implementations and generated loops differ. The reference
//! wrappers receive the same prebuilt input and use the same summation rule;
//! allocation and construction stay outside the common timing loop.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMaskView};

use crate::support::{
    baseline::Definition,
    fixture::Fixture,
    reference::{self, Mask},
    sampling::Series,
};

pub const PATHS: [&str; 4] = ["fold", "try_fold", "words", "control"];
pub const DEFINITION: Definition = Definition {
    name: "column_reader",
    paths: &PATHS,
    policy: crate::support::measurement::Policy::Strict,
};

// Every timed entry point takes the same borrowed input. It is built before
// timing and black-boxed once per invocation by the common timing loop.
pub struct Input<'a, C> {
    column: &'a C,
    rows: RowMaskView<'a>,
    values: &'a [i32],
    datums: &'a [u64],
    nulls: &'a [bool],
    reference_rows: Mask<'a>,
    prepared: Option<Mask<'a>>,
    non_nulls: Option<Mask<'a>>,
}

impl<'a, C> Input<'a, C> {
    pub fn new(column: &'a C, case: &'a Fixture) -> Self {
        Self {
            column,
            rows: case.selected.view(),
            values: &case.values,
            datums: &case.datums,
            nulls: &case.nulls,
            reference_rows: case.selected.reference(),
            prepared: case
                .prepared
                .as_ref()
                .map(crate::support::fixture::Bitmap::reference),
            non_nulls: case
                .non_nulls
                .as_ref()
                .map(crate::support::fixture::Bitmap::reference),
        }
    }
}

#[inline(never)]
// Deliberately exercise the bulk fold implementation separately from try_fold.
#[allow(clippy::manual_try_fold)]
pub fn fold_sum<C: ColumnReader<Value = i32>>(input: &Input<'_, C>) -> Result<i64> {
    input
        .column
        .selected_values(&input.rows)?
        .fold(Ok(0), |sum, row| {
            let mut sum = sum?;
            if let Some(value) = row?.1 {
                sum += i64::from(value);
            }
            Ok(sum)
        })
}

#[inline(never)]
pub fn iter_sum<C: ColumnReader<Value = i32>>(input: &Input<'_, C>) -> Result<i64> {
    input
        .column
        .selected_values(&input.rows)?
        .try_fold(0, |mut sum, row| {
            if let Some(value) = row?.1 {
                sum += i64::from(value);
            }
            Ok(sum)
        })
}

#[inline(never)]
pub fn word_sum<C: ColumnReader<Value = i32>>(input: &Input<'_, C>) -> Result<i64> {
    ensure!(
        input.column.nrows() == input.rows.nrows(),
        "row counts differ"
    );
    let mut sum = 0;
    for index in 0..input.rows.nrows().div_ceil(64) {
        let selected = input.rows.word(index).unwrap();
        if selected == 0 {
            continue;
        }
        for (_, value) in input.column.word_values(index, selected)? {
            if let Some(value) = value {
                sum += i64::from(value);
            }
        }
    }
    Ok(sum)
}

#[inline(never)]
pub fn dense_reference<C>(input: &Input<'_, C>) -> Result<i64> {
    reference::dense(
        input.values,
        input.reference_rows,
        input.prepared,
        input.non_nulls,
    )
}

#[inline(never)]
pub fn datum_reference<C>(input: &Input<'_, C>) -> Result<i64> {
    reference::datum(
        input.datums,
        input.nulls,
        input.reference_rows,
        input.prepared,
    )
}

fn case(nrows: usize, pattern: &str, nulls: &str, offset: Option<usize>, partial: bool) -> Fixture {
    let values = (0..nrows)
        .map(|row| (row as i32).wrapping_mul(7919).wrapping_sub(104729))
        .collect();
    let mut case = Fixture::from_values(values, pattern, nulls, offset, partial);
    case.quick = nrows == 1024
        && matches!(
            (pattern, nulls, offset, partial),
            ("all", "none" | "mixed" | "all", None, false)
                | ("sparse" | "empty", "none", None, false)
                | ("all", "mixed", None, true)
                | ("sparse", "mixed", Some(7), true)
        );
    case
}

pub fn cases() -> Vec<Fixture> {
    let mut cases = Vec::new();
    for nulls in ["none", "mixed"] {
        for pattern in ["all", "half", "sparse", "empty"] {
            cases.push(case(1024, pattern, nulls, None, false));
        }
    }
    for nrows in [0, 1, 63, 64, 65] {
        cases.push(case(nrows, "all", "mixed", None, false));
    }
    cases.push(case(1024, "all", "all", None, false));
    cases.push(case(1024, "all", "mixed", None, true));
    for (nrows, offset, pattern) in [(65, 3, "all"), (1024, 7, "all"), (1024, 7, "sparse")] {
        cases.push(case(nrows, pattern, "mixed", Some(offset), true));
    }
    cases
}

// This is the original clock loop, separate from shared sample scheduling.
fn time<C>(
    input: &Input<'_, C>,
    run: impl Fn(&Input<'_, C>) -> Result<i64>,
    iterations: usize,
) -> f64 {
    use std::{hint::black_box, time::Instant};
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(run(black_box(input)).unwrap());
    }
    start.elapsed().as_secs_f64() * 1e9 / iterations as f64
}

pub fn measure(case: &Fixture, format: &str, series: Series<'_>) -> Result<()> {
    match format {
        "dense" => measure_column(&case.dense_column()?, dense_reference, case, series),
        "datum" => measure_column(&case.datum_column()?, datum_reference, case, series),
        _ => unreachable!("unknown column format"),
    }
}

fn measure_column<C: ColumnReader<Value = i32>>(
    column: &C,
    direct: impl Fn(&Input<'_, C>) -> Result<i64> + Copy,
    case: &Fixture,
    series: Series<'_>,
) -> Result<()> {
    let input = Input::new(column, case);
    ensure!(
        direct(&input)? == case.expected,
        "reference result differs from scalar model"
    );
    ensure!(
        fold_sum(&input)? == case.expected,
        "fold reader result differs"
    );
    ensure!(
        iter_sum(&input)? == case.expected,
        "try_fold reader result differs"
    );
    ensure!(
        word_sum(&input)? == case.expected,
        "word reader result differs"
    );
    series.collect(&PATHS, |path, iterations| match path {
        "fold" => time(&input, fold_sum, iterations),
        "try_fold" => time(&input, iter_sum, iterations),
        "words" => time(&input, word_sum, iterations),
        "control" => time(&input, direct, iterations),
        _ => unreachable!("unknown reader path"),
    })
}

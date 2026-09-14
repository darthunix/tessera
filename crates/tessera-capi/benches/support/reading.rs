//! Timed summation kernels using each public column-reading path.
//!
//! Exercise bulk fold, short-circuiting try_fold, and copied-word iteration
//! separately: their implementations and generated loops differ. The reference
//! wrappers receive the same prebuilt input and use the same summation rule;
//! allocation and construction stay outside the common timing loop.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMaskView};

use crate::support::{
    fixture::Fixture,
    reference::{self, Mask},
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
    Fixture::from_values(values, pattern, nulls, offset, partial)
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

pub fn bench(criterion: &mut criterion::Criterion) {
    for case in cases() {
        measure_column(
            criterion,
            "dense",
            &case.dense_column().unwrap(),
            dense_reference,
            &case,
        )
        .unwrap();
        measure_column(
            criterion,
            "datum",
            &case.datum_column().unwrap(),
            datum_reference,
            &case,
        )
        .unwrap();
    }
}

fn measure_column<C: ColumnReader<Value = i32>>(
    criterion: &mut criterion::Criterion,
    format: &str,
    column: &C,
    direct: impl Fn(&Input<'_, C>) -> Result<i64>,
    case: &Fixture,
) -> Result<()> {
    use std::hint::black_box;
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
    let mut group = criterion.benchmark_group(format!("column_reader/{format}/{}", case.name));
    // Separate closures preserve static dispatch inside every measured loop.
    group.bench_function("fold", |b| {
        b.iter(|| black_box(fold_sum(black_box(&input)).unwrap()))
    });
    group.bench_function("try_fold", |b| {
        b.iter(|| black_box(iter_sum(black_box(&input)).unwrap()))
    });
    group.bench_function("words", |b| {
        b.iter(|| black_box(word_sum(black_box(&input)).unwrap()))
    });
    group.bench_function("reference", |b| {
        b.iter(|| black_box(direct(black_box(&input)).unwrap()))
    });
    group.finish();
    Ok(())
}

//! Measured summation kernels using each public column-reading path.
//!
//! Exercise the early-exit fold and copied-word iteration separately: their
//! implementations and generated loops differ. The reference wrappers receive
//! the same prebuilt input and use the same summation rule; allocation and
//! construction stay outside the counted loop.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMaskView};

use crate::support::{
    fixture::Fixture,
    reference::{self, Mask},
    runner::Runner,
};

// Every measured entry point takes the same borrowed input. It is built before
// counting and black-boxed once per invocation inside the counted loop.
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
pub fn fold_sum<C: ColumnReader<Value = i32>>(input: &Input<'_, C>) -> Result<i64> {
    // map_or keeps the accumulator free of a select on NULL rows.
    input
        .column
        .try_fold_selected(&input.rows, 0, |sum, _, value| {
            Ok(sum + value.map_or(0, i64::from))
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
        // fold is the bulk path of the word iterator, like the kernels use it.
        sum = input
            .column
            .word_values(index, selected)?
            .fold(sum, |sum, (_, value)| sum + value.map_or(0, i64::from));
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
    cases.push(case(1024, "all", "random", None, false));
    cases.push(case(1024, "random", "random", None, false));
    cases.push(case(1024, "all", "mixed", None, true));
    for (nrows, offset, pattern) in [(65, 3, "all"), (1024, 7, "all"), (1024, 7, "sparse")] {
        cases.push(case(nrows, pattern, "mixed", Some(offset), true));
    }
    cases
}

pub fn bench(runner: &mut Runner) -> Result<()> {
    for case in cases() {
        measure_column(
            runner,
            "dense",
            &case.dense_column()?,
            dense_reference,
            &case,
        )?;
        measure_column(
            runner,
            "datum",
            &case.datum_column()?,
            datum_reference,
            &case,
        )?;
    }
    Ok(())
}

fn measure_column<C: ColumnReader<Value = i32>>(
    runner: &mut Runner,
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
        word_sum(&input)? == case.expected,
        "word reader result differs"
    );
    let mut group = runner.group(format!("column_reader/{format}/{}", case.name));
    // Separate closures preserve static dispatch inside every measured loop.
    group.op("fold", || fold_sum(black_box(&input)).unwrap())?;
    group.op("words", || word_sum(black_box(&input)).unwrap())?;
    group.op("reference", || direct(black_box(&input)).unwrap())?;
    Ok(())
}

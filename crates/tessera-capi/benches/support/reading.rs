//! Timed summation kernels using each public column-reading path.
//!
//! Exercise bulk fold, short-circuiting try_fold, and copied-word iteration
//! separately: their implementations and generated loops differ. The reference
//! wrappers receive the same prebuilt input and use the same summation rule;
//! allocation and construction stay outside the common timing loop.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMaskView};

use super::{
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
                .map(super::fixture::Bitmap::reference),
            non_nulls: case
                .non_nulls
                .as_ref()
                .map(super::fixture::Bitmap::reference),
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

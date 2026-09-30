//! Measured int8 aggregate kernels over the int8 reading fixtures,
//! mirroring `aggregating.rs`.
//!
//! The same case matrix as the reading benchmark: the kernels aggregate the
//! selected rows of each column, the reference is the independent scalar
//! sum. Results are checked against a model of the fixture before counting.

use anyhow::{Result, ensure};
use tessera_core::ColumnReader;
use tessera_kernels::int64;

use crate::reading64::{self, Input};
use crate::support::{fixture::Fixture, runner::Runner};

#[inline(never)]
pub fn min<C: ColumnReader<Value = i64>>(input: &Input<'_, C>) -> Result<Option<i64>> {
    int64::min(input.column, &input.rows)
}

#[inline(never)]
pub fn max<C: ColumnReader<Value = i64>>(input: &Input<'_, C>) -> Result<Option<i64>> {
    int64::max(input.column, &input.rows)
}

#[inline(never)]
pub fn count<C: ColumnReader<Value = i64>>(input: &Input<'_, C>) -> Result<usize> {
    tessera_kernels::count::count(input.column, &input.rows)
}

/// The fixture's selected non-NULL values, in row order.
pub fn present(case: &Fixture<i64>) -> Vec<i64> {
    let words = case.selected.words();
    (0..case.values.len())
        .filter(|&row| words[row / 64] & (1 << (row % 64)) != 0 && !case.nulls[row])
        .map(|row| case.values[row])
        .collect()
}

/// Check every kernel against the model before anything is measured.
pub fn check<C: ColumnReader<Value = i64>>(
    input: &Input<'_, C>,
    case: &Fixture<i64>,
) -> Result<()> {
    let present = present(case);
    ensure!(
        count(input)? == present.len(),
        "count differs from the model"
    );
    ensure!(
        min(input)? == present.iter().copied().min(),
        "min differs from the model"
    );
    ensure!(
        max(input)? == present.iter().copied().max(),
        "max differs from the model"
    );
    Ok(())
}

pub fn bench(runner: &mut Runner) -> Result<()> {
    for case in reading64::cases() {
        measure_column(
            runner,
            "dense",
            &case.dense_column()?,
            reading64::dense_reference,
            &case,
        )?;
        measure_column(
            runner,
            "datum",
            &case.datum_column()?,
            reading64::datum_reference,
            &case,
        )?;
    }
    Ok(())
}

fn measure_column<C: ColumnReader<Value = i64>>(
    runner: &mut Runner,
    format: &str,
    column: &C,
    direct: impl Fn(&Input<'_, C>) -> Result<i64>,
    case: &Fixture<i64>,
) -> Result<()> {
    use std::hint::black_box;
    let input = Input::new(column, case);
    ensure!(
        direct(&input)? == case.expected,
        "reference result differs from scalar model"
    );
    check(&input, case)?;
    let mut group = runner.group(format!("aggregate_int64/{format}/{}", case.name));
    group.op("min", || min(black_box(&input)).unwrap())?;
    group.op("max", || max(black_box(&input)).unwrap())?;
    group.op("count", || count(black_box(&input)).unwrap())?;
    group.op("reference", || direct(black_box(&input)).unwrap())?;
    Ok(())
}

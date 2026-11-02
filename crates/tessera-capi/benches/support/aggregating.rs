//! Measured aggregate kernels over the reading fixtures.
//!
//! The same inputs as the reading benchmark: the kernels aggregate the
//! selected rows of each column, the reference is the independent scalar
//! sum. Results are checked against a model of the fixture before counting.

use anyhow::{Result, ensure};
use tessera_core::ColumnReader;
use tessera_kernels::int32;

use crate::reading::{self, Input};
use crate::support::{
    fixture::{self, Fixture},
    runner::Runner,
};

#[inline(never)]
pub fn sum<C: ColumnReader<Value = i32>>(input: &Input<'_, C>) -> Result<Option<i64>> {
    int32::sum(input.column, &input.rows)
}

#[inline(never)]
pub fn min<C: ColumnReader<Value = i32>>(input: &Input<'_, C>) -> Result<Option<i32>> {
    int32::min(input.column, &input.rows)
}

#[inline(never)]
pub fn count<C: ColumnReader<Value = i32>>(input: &Input<'_, C>) -> Result<usize> {
    tessera_kernels::count::count(input.column, &input.rows)
}

/// The fixture's selected non-NULL values, in row order.
pub fn present(case: &Fixture) -> Vec<i32> {
    let words = case.selected.words();
    (0..case.values.len())
        .filter(|&row| words[row / 64] & (1 << (row % 64)) != 0 && !case.nulls[row])
        .map(|row| case.values[row])
        .collect()
}

/// Check every kernel against the model before anything is measured.
pub fn check<C: ColumnReader<Value = i32>>(input: &Input<'_, C>, case: &Fixture) -> Result<()> {
    let present = present(case);
    ensure!(
        count(input)? == present.len(),
        "count differs from the model"
    );
    let expected_sum = (!present.is_empty()).then_some(case.expected);
    ensure!(sum(input)? == expected_sum, "sum differs from the model");
    ensure!(
        min(input)? == present.iter().copied().min(),
        "min differs from the model"
    );
    ensure!(
        int32::max(input.column, &input.rows)? == present.iter().copied().max(),
        "max differs from the model"
    );
    Ok(())
}

pub fn bench(runner: &mut Runner) -> Result<()> {
    for case in reading::cases() {
        let inputs = case.inputs();
        measure_column(
            runner,
            "dense",
            &fixture::columns(&inputs, Fixture::dense_column)?,
            reading::dense_reference,
            &inputs,
        )?;
        measure_column(
            runner,
            "datum",
            &fixture::columns(&inputs, Fixture::datum_column)?,
            reading::datum_reference,
            &inputs,
        )?;
    }
    Ok(())
}

fn measure_column<C: ColumnReader<Value = i32>>(
    runner: &mut Runner,
    format: &str,
    columns: &[C],
    direct: impl Fn(&Input<'_, C>) -> Result<i64>,
    cases: &[Fixture],
) -> Result<()> {
    let inputs = Input::each(columns, cases);
    for (input, case) in inputs.iter().zip(cases) {
        ensure!(
            direct(input)? == case.expected,
            "reference result differs from scalar model"
        );
        check(input, case)?;
    }
    let mut group = runner.group(format!("aggregate_int32/{format}/{}", cases[0].name));
    group.op_each("sum", &inputs, |input| sum(input).unwrap())?;
    group.op_each("min", &inputs, |input| min(input).unwrap())?;
    group.op_each("count", &inputs, |input| count(input).unwrap())?;
    group.op_each("reference", &inputs, |input| direct(input).unwrap())?;
    Ok(())
}

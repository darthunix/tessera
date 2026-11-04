//! Measured arithmetic kernels over the reading fixtures.
//!
//! The same inputs as the reading benchmark; the kernels write a dense
//! result column into buffers allocated once per case, and the reference is
//! an independent scalar loop over the fixture's buffers with checked
//! arithmetic. Results are checked against a model before counting.

use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask};
use tessera_kernels::int32::{self, ArithOp, ArithmeticError};
use tessera_kernels::smallint;

use crate::reading::{self, Input};
use crate::support::{
    fixture::{self, Fixture},
    runner::Runner,
};

/// The scalar operand of every measured operation.
pub const SCALAR: i32 = 7;

/// The result buffers of one case.
pub struct Output {
    pub values: Vec<MaybeUninit<i32>>,
    pub words: Vec<u64>,
}

impl Output {
    pub fn new(nrows: usize) -> Self {
        Self {
            values: vec![MaybeUninit::uninit(); nrows],
            words: vec![0; nrows.div_ceil(64)],
        }
    }

    /// The value buffer and the mask over the word buffer, borrowed apart.
    fn parts(&mut self) -> Result<(&mut [MaybeUninit<i32>], RowMask<'_>)> {
        let mask = RowMask::try_new(self.values.len(), &mut self.words)?;
        Ok((&mut self.values, mask))
    }

    /// The value of a row the mask marks non-NULL.
    pub fn value(&self, row: usize) -> i32 {
        assert!(
            self.words[row / 64] & (1 << (row % 64)) != 0,
            "row {row} is NULL"
        );
        // SAFETY: the kernels write every row they mark non-NULL.
        unsafe { self.values[row].assume_init() }
    }
}

#[inline(never)]
pub fn add_scalar<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    out: &mut Output,
) -> Result<()> {
    let (values, mut mask) = out.parts()?;
    int32::arith_scalar(
        ArithOp::Add,
        input.column,
        SCALAR,
        &input.rows,
        values,
        &mut mask,
    )
}

#[inline(never)]
pub fn mul_scalar<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    out: &mut Output,
) -> Result<()> {
    let (values, mut mask) = out.parts()?;
    int32::arith_scalar(
        ArithOp::Mul,
        input.column,
        SCALAR,
        &input.rows,
        values,
        &mut mask,
    )
}

#[inline(never)]
pub fn div_scalar<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    out: &mut Output,
) -> Result<()> {
    let (values, mut mask) = out.parts()?;
    int32::arith_scalar(
        ArithOp::Div,
        input.column,
        SCALAR,
        &input.rows,
        values,
        &mut mask,
    )
}

#[inline(never)]
pub fn mod_scalar<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    out: &mut Output,
) -> Result<()> {
    let (values, mut mask) = out.parts()?;
    int32::arith_scalar(
        ArithOp::Mod,
        input.column,
        SCALAR,
        &input.rows,
        values,
        &mut mask,
    )
}

#[inline(never)]
pub fn add_column<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    out: &mut Output,
) -> Result<()> {
    let (values, mut mask) = out.parts()?;
    int32::arith_columns(
        ArithOp::Add,
        input.column,
        input.column,
        &input.rows,
        values,
        &mut mask,
    )
}

/// The smallint forms of the same operations, over a case whose values
/// are smallints ([`smallint_inputs`]).
#[inline(never)]
pub fn add_scalar_smallint<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    out: &mut Output,
) -> Result<()> {
    let (values, mut mask) = out.parts()?;
    smallint::arith_scalar(
        ArithOp::Add,
        input.column,
        SCALAR,
        &input.rows,
        values,
        &mut mask,
    )
}

#[inline(never)]
pub fn mul_scalar_smallint<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    out: &mut Output,
) -> Result<()> {
    let (values, mut mask) = out.parts()?;
    smallint::arith_scalar(
        ArithOp::Mul,
        input.column,
        SCALAR,
        &input.rows,
        values,
        &mut mask,
    )
}

#[inline(never)]
pub fn div_scalar_smallint<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    out: &mut Output,
) -> Result<()> {
    let (values, mut mask) = out.parts()?;
    smallint::arith_scalar(
        ArithOp::Div,
        input.column,
        SCALAR,
        &input.rows,
        values,
        &mut mask,
    )
}

#[inline(never)]
pub fn add_column_smallint<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    out: &mut Output,
) -> Result<()> {
    let (values, mut mask) = out.parts()?;
    smallint::arith_columns(
        ArithOp::Add,
        input.column,
        input.column,
        &input.rows,
        values,
        &mut mask,
    )
}

/// The inputs of a case with smallint values: each modulo 4096, so that
/// `x * 7` and `x + x` stay within the range.
pub fn smallint_inputs(inputs: &[Fixture]) -> Vec<Fixture> {
    fixture::map_inputs(inputs, |value| value % 4096)
}

/// `value + SCALAR` for every selected non-NULL row, straight from the
/// dense fixture buffers and their masks.
#[inline(never)]
pub fn dense_reference<C>(input: &Input<'_, C>, out: &mut Output) -> Result<()> {
    let rows = input.reference_rows;
    ensure!(input.values.len() == rows.nrows, "row counts differ");
    for index in 0..rows.nrows.div_ceil(64) {
        let mut selected = rows.word(index);
        let mut present = 0;
        if selected != 0 {
            if let Some(prepared) = input.prepared {
                ensure!(selected & !prepared.word(index) == 0, "unprepared rows");
            }
            let non_nulls = input.non_nulls.map_or(u64::MAX, |mask| mask.word(index));
            while selected != 0 {
                let bit = selected.trailing_zeros() as usize;
                selected &= selected - 1;
                let row = index * 64 + bit;
                if non_nulls & (1 << bit) != 0 {
                    let value = input.values[row]
                        .checked_add(SCALAR)
                        .ok_or(ArithmeticError::IntegerOutOfRange)?;
                    out.values[row].write(value);
                    present |= 1 << bit;
                }
            }
        }
        out.words[index] = present;
    }
    Ok(())
}

/// The same over Datum storage and its flags.
#[inline(never)]
pub fn datum_reference<C>(input: &Input<'_, C>, out: &mut Output) -> Result<()> {
    let rows = input.reference_rows;
    ensure!(input.datums.len() == rows.nrows, "row counts differ");
    for index in 0..rows.nrows.div_ceil(64) {
        let mut selected = rows.word(index);
        let mut present = 0;
        if selected != 0 {
            if let Some(prepared) = input.prepared {
                ensure!(selected & !prepared.word(index) == 0, "unprepared rows");
            }
            while selected != 0 {
                let bit = selected.trailing_zeros() as usize;
                selected &= selected - 1;
                let row = index * 64 + bit;
                if !input.nulls[row] {
                    let value = (input.datums[row] as i32)
                        .checked_add(SCALAR)
                        .ok_or(ArithmeticError::IntegerOutOfRange)?;
                    out.values[row].write(value);
                    present |= 1 << bit;
                }
            }
        }
        out.words[index] = present;
    }
    Ok(())
}

/// Check one operation against the fixture model: the same rows marked,
/// the same values.
fn check_op(
    case: &Fixture,
    out: &Output,
    what: &str,
    expected: impl Fn(i32) -> Option<i32>,
) -> Result<()> {
    let words = case.selected.words();
    for row in 0..case.values.len() {
        let selected = words[row / 64] & (1 << (row % 64)) != 0;
        let present = out.words[row / 64] & (1 << (row % 64)) != 0;
        let model = (selected && !case.nulls[row])
            .then(|| expected(case.values[row]))
            .flatten();
        ensure!(
            present == model.is_some(),
            "{what}: row {row} marked {present}, model {}",
            model.is_some()
        );
        if let Some(value) = model {
            ensure!(
                out.value(row) == value,
                "{what}: row {row} differs from the model"
            );
        }
    }
    Ok(())
}

/// Check every kernel and the reference against the model before anything
/// is measured; the fixture values never overflow with these operations.
pub fn check<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    case: &Fixture,
    direct: impl Fn(&Input<'_, C>, &mut Output) -> Result<()>,
) -> Result<()> {
    let mut out = Output::new(case.values.len());
    direct(input, &mut out)?;
    check_op(case, &out, "reference", |x| x.checked_add(SCALAR))?;
    add_scalar(input, &mut out)?;
    check_op(case, &out, "add_scalar", |x| x.checked_add(SCALAR))?;
    mul_scalar(input, &mut out)?;
    check_op(case, &out, "mul_scalar", |x| x.checked_mul(SCALAR))?;
    div_scalar(input, &mut out)?;
    check_op(case, &out, "div_scalar", |x| Some(x / SCALAR))?;
    mod_scalar(input, &mut out)?;
    check_op(case, &out, "mod_scalar", |x| Some(x % SCALAR))?;
    add_column(input, &mut out)?;
    check_op(case, &out, "add_column", |x| x.checked_add(x))?;
    Ok(())
}

/// The smallint kernels against i16 arithmetic over a smallint case.
pub fn check_smallint<C: ColumnReader<Value = i32>>(
    input: &Input<'_, C>,
    case: &Fixture,
) -> Result<()> {
    let small = |value: Option<i16>| value.map(i32::from);
    let narrow = |x: i32| i16::try_from(x).unwrap();
    let seven = narrow(SCALAR);
    let mut out = Output::new(case.values.len());
    add_scalar_smallint(input, &mut out)?;
    check_op(case, &out, "add_scalar_smallint", |x| {
        small(narrow(x).checked_add(seven))
    })?;
    mul_scalar_smallint(input, &mut out)?;
    check_op(case, &out, "mul_scalar_smallint", |x| {
        small(narrow(x).checked_mul(seven))
    })?;
    div_scalar_smallint(input, &mut out)?;
    check_op(case, &out, "div_scalar_smallint", |x| {
        small(narrow(x).checked_div(seven))
    })?;
    add_column_smallint(input, &mut out)?;
    check_op(case, &out, "add_column_smallint", |x| {
        small(narrow(x).checked_add(narrow(x)))
    })?;
    Ok(())
}

pub fn bench(runner: &mut Runner) -> Result<()> {
    for case in reading::cases() {
        let inputs = case.inputs();
        measure_column(
            runner,
            "dense",
            &fixture::columns(&inputs, Fixture::dense_column)?,
            dense_reference,
            &inputs,
        )?;
        measure_column(
            runner,
            "datum",
            &fixture::columns(&inputs, Fixture::datum_column)?,
            datum_reference,
            &inputs,
        )?;
        let small = smallint_inputs(&inputs);
        measure_smallint(
            runner,
            "dense",
            &fixture::columns(&small, Fixture::dense_column)?,
            dense_reference,
            &small,
        )?;
        measure_smallint(
            runner,
            "datum",
            &fixture::columns(&small, Fixture::datum_column)?,
            datum_reference,
            &small,
        )?;
    }
    Ok(())
}

/// The smallint kernels, in a group of their own beside the int4 ones of
/// the same rows and masks, with the scalar loop of the int4 group as its
/// reference.
fn measure_smallint<C: ColumnReader<Value = i32>>(
    runner: &mut Runner,
    format: &str,
    columns: &[C],
    direct: impl Fn(&Input<'_, C>, &mut Output) -> Result<()> + Copy,
    cases: &[Fixture],
) -> Result<()> {
    let inputs = Input::each(columns, cases);
    for (input, case) in inputs.iter().zip(cases) {
        check_smallint(input, case)?;
    }
    let mut out = Output::new(cases[0].values.len());
    let mut group = runner.group(format!("arith_int32/{format}/smallint/{}", cases[0].name));
    group.op_each("add_scalar", &inputs, |input| {
        add_scalar_smallint(input, &mut out).unwrap()
    })?;
    group.op_each("mul_scalar", &inputs, |input| {
        mul_scalar_smallint(input, &mut out).unwrap()
    })?;
    group.op_each("div_scalar", &inputs, |input| {
        div_scalar_smallint(input, &mut out).unwrap()
    })?;
    group.op_each("add_column", &inputs, |input| {
        add_column_smallint(input, &mut out).unwrap()
    })?;
    group.op_each("reference", &inputs, |input| {
        direct(input, &mut out).unwrap()
    })?;
    Ok(())
}

fn measure_column<C: ColumnReader<Value = i32>>(
    runner: &mut Runner,
    format: &str,
    columns: &[C],
    direct: impl Fn(&Input<'_, C>, &mut Output) -> Result<()> + Copy,
    cases: &[Fixture],
) -> Result<()> {
    let inputs = Input::each(columns, cases);
    for (input, case) in inputs.iter().zip(cases) {
        check(input, case, direct)?;
    }
    let mut out = Output::new(cases[0].values.len());
    let mut group = runner.group(format!("arith_int32/{format}/{}", cases[0].name));
    group.op_each("add_scalar", &inputs, |input| {
        add_scalar(input, &mut out).unwrap()
    })?;
    group.op_each("mul_scalar", &inputs, |input| {
        mul_scalar(input, &mut out).unwrap()
    })?;
    group.op_each("div_scalar", &inputs, |input| {
        div_scalar(input, &mut out).unwrap()
    })?;
    group.op_each("mod_scalar", &inputs, |input| {
        mod_scalar(input, &mut out).unwrap()
    })?;
    group.op_each("add_column", &inputs, |input| {
        add_column(input, &mut out).unwrap()
    })?;
    group.op_each("reference", &inputs, |input| {
        direct(input, &mut out).unwrap()
    })?;
    Ok(())
}

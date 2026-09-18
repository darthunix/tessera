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

use crate::reading::{self, Input};
use crate::support::{fixture::Fixture, runner::Runner};

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

pub fn bench(runner: &mut Runner) -> Result<()> {
    for case in reading::cases() {
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
    direct: impl Fn(&Input<'_, C>, &mut Output) -> Result<()> + Copy,
    case: &Fixture,
) -> Result<()> {
    use std::hint::black_box;
    let input = Input::new(column, case);
    check(&input, case, direct)?;
    let mut out = Output::new(case.values.len());
    let mut group = runner.group(format!("arith_int32/{format}/{}", case.name));
    group.op("add_scalar", || {
        add_scalar(black_box(&input), &mut out).unwrap()
    })?;
    group.op("mul_scalar", || {
        mul_scalar(black_box(&input), &mut out).unwrap()
    })?;
    group.op("div_scalar", || {
        div_scalar(black_box(&input), &mut out).unwrap()
    })?;
    group.op("mod_scalar", || {
        mod_scalar(black_box(&input), &mut out).unwrap()
    })?;
    group.op("add_column", || {
        add_column(black_box(&input), &mut out).unwrap()
    })?;
    group.op("reference", || direct(black_box(&input), &mut out).unwrap())?;
    Ok(())
}

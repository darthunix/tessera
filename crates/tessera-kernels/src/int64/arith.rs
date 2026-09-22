//! int8 arithmetic with PostgreSQL's rules, producing a dense column.
//!
//! `+`, `-` and `*` fail with [`ArithmeticError::BigintOutOfRange`] on
//! overflow; `/` and `%` fail with [`ArithmeticError::DivisionByZero`] on a
//! zero divisor, `/` also with `BigintOutOfRange` for `i64::MIN / -1`, and
//! `x % -1` is 0, as PostgreSQL defines it to avoid that trap. Division
//! truncates toward zero and the remainder takes the dividend's sign, as in
//! C, Rust and PostgreSQL. A NULL operand makes a NULL result, and the
//! checks apply to selected non-NULL rows only. An error anywhere in the
//! selection fails the whole call; the result is then unspecified and the
//! caller discards it. The errors carry their SQLSTATE for a C boundary
//! that reports them as PostgreSQL does, without PostgreSQL being called
//! from here.
//!
//! The result is a dense column in the caller's buffers: for every word with
//! selected rows the kernel writes the word of `non_nulls` (selected rows
//! whose operands are non-NULL) and the values of those rows; NULL rows get
//! an initialized placeholder, rows outside the selection are unspecified
//! and may stay uninitialized, and words without selected rows have their
//! `non_nulls` word cleared. The result reads as a `DenseInt64Column` with
//! the selection as its readiness mask.
//!
//! When the first word of the selection is full, selects at least a dozen
//! rows and every column operand exposes its storage, the call computes
//! whole words from the blocks: `/` and `%` by a scalar divisor of
//! magnitude at least two through a multiplier prepared once per call
//! (`Divisor`), unable to fail; the other operations lane by lane over the
//! present lanes, with their checks. Single-row words, the tail and refused
//! words are read row by row; every other call reads every word row by row
//! through the word iterators.

use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView, WordBlock};

use super::{BULK_MIN_ROWS, Divisor};
use crate::ops::{ArithOp, ArithmeticError};

/// Compute `column op scalar` for the selected rows into `values` and
/// `non_nulls`.
///
/// `values` and `non_nulls` have the batch's row count, and so must the
/// column and the selection. Rows the selection excludes are unspecified
/// afterwards.
///
/// # Errors
///
/// Different row counts fail before any mutation. An arithmetic failure
/// or a reader error (an unprepared selected row) fails the call with the
/// outputs unspecified. Empty selections are valid.
pub fn arith_scalar<C: ColumnReader<Value = i64>>(
    op: ArithOp,
    column: &C,
    scalar: i64,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    let operands: Operands<'_, C, C> = Operands::ColumnScalar(column, scalar);
    run(op, operands, rows, values, non_nulls)
}

/// Compute `scalar op column` for the selected rows into `values` and
/// `non_nulls`, for the operations where the order matters.
///
/// # Errors
///
/// As for [`arith_scalar`].
pub fn arith_scalar_left<C: ColumnReader<Value = i64>>(
    op: ArithOp,
    scalar: i64,
    column: &C,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    let operands: Operands<'_, C, C> = Operands::ScalarColumn(scalar, column);
    run(op, operands, rows, values, non_nulls)
}

/// Compute `left op right` row by row for two columns of the batch; a NULL
/// on either side makes a NULL.
///
/// # Errors
///
/// As for [`arith_scalar`]; both columns must have the batch's row count.
pub fn arith_columns<L, R>(
    op: ArithOp,
    left: &L,
    right: &R,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()>
where
    L: ColumnReader<Value = i64>,
    R: ColumnReader<Value = i64>,
{
    run(op, Operands::Columns(left, right), rows, values, non_nulls)
}

/// A whole-word operand: the storage of a full prepared word, or a constant.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Side<'a> {
    Dense(&'a [i64; 64]),
    Datum(&'a [u64; 64]),
    Scalar(i64),
}

impl Side<'_> {
    /// One lane as int8; a Datum's whole word, as `DatumGetInt64`.
    #[inline(always)]
    fn lane(self, lane: usize) -> i64 {
        match self {
            Self::Dense(values) => values[lane],
            Self::Datum(values) => values[lane] as i64,
            Self::Scalar(value) => value,
        }
    }
}

/// The operands of one call.
enum Operands<'a, L, R> {
    ColumnScalar(&'a L, i64),
    ScalarColumn(i64, &'a R),
    Columns(&'a L, &'a R),
}

impl<L, R> Operands<'_, L, R>
where
    L: ColumnReader<Value = i64>,
    R: ColumnReader<Value = i64>,
{
    fn column_rows(&self) -> [Option<usize>; 2] {
        match self {
            Self::ColumnScalar(left, _) => [Some(left.nrows()), None],
            Self::ScalarColumn(_, right) => [None, Some(right.nrows())],
            Self::Columns(left, right) => [Some(left.nrows()), Some(right.nrows())],
        }
    }

    /// Every word row by row. Out of line, as in the int32 kernel, so that
    /// the loop keeps its registers when a whole-word path shares the call.
    #[inline(never)]
    fn rows<E: Evaluate>(&self, output: Output<'_, '_, '_, '_>, evaluate: &E) -> Result<()> {
        let rows = output.rows;
        let mut output = output;
        for index in 0..rows.nrows().div_ceil(64) {
            let selected = rows.word(index).unwrap();
            self.word(index, selected, &mut output, evaluate)?;
        }
        Ok(())
    }

    /// One word row by row. The three operand shapes give three loops of
    /// one body; two columns are zipped word by word, which the trait
    /// guarantees to yield the same rows in the same order. A NULL row is
    /// computed from a placeholder pair that no operation rejects, so that
    /// the loop has no branch on nullness; only the error check branches,
    /// and it never goes. The pair is built from two scalars, not an
    /// `Option` of a tuple, which the compiler kept on the stack.
    #[inline(always)]
    fn word<E: Evaluate>(
        &self,
        index: usize,
        selected: u64,
        output: &mut Output<'_, '_, '_, '_>,
        evaluate: &E,
    ) -> Result<()> {
        if selected == 0 {
            return output.non_nulls.set_word(index, 0);
        }
        let mut present = 0;
        match self {
            Self::ColumnScalar(left, scalar) => {
                for (row, value) in left.word_values(index, selected)? {
                    let some = value.is_some();
                    let b = if some { *scalar } else { 1 };
                    output.values[row].write(evaluate(value.unwrap_or(0), b)?);
                    present |= u64::from(some) << (row % 64);
                }
            }
            Self::ScalarColumn(scalar, right) => {
                for (row, value) in right.word_values(index, selected)? {
                    let some = value.is_some();
                    output.values[row].write(evaluate(*scalar, value.unwrap_or(1))?);
                    present |= u64::from(some) << (row % 64);
                }
            }
            Self::Columns(left, right) => {
                let pairs = left
                    .word_values(index, selected)?
                    .zip(right.word_values(index, selected)?);
                for ((row, a), (_, b)) in pairs {
                    let some = a.is_some() && b.is_some();
                    let b = if some { b.unwrap_or(1) } else { 1 };
                    output.values[row].write(evaluate(a.unwrap_or(0), b)?);
                    present |= u64::from(some) << (row % 64);
                }
            }
        }
        output.non_nulls.set_word(index, present)
    }

    /// The whole-word operands of `index` with the non-NULL rows of the
    /// column operands, when every column operand exposes the word.
    #[cfg(all(target_arch = "aarch64", not(miri)))]
    fn blocks(&self, index: usize) -> Option<(Side<'_>, Side<'_>, u64)> {
        fn side(block: WordBlock<'_, i64>) -> (Side<'_>, u64) {
            match block {
                WordBlock::Dense { values, non_nulls } => (Side::Dense(values), non_nulls),
                WordBlock::Datum { values, isnull } => {
                    (Side::Datum(values), crate::simd::non_null_bits(isnull))
                }
            }
        }
        Some(match self {
            Self::ColumnScalar(left, scalar) => {
                let (lhs, present) = side(left.word_block(index)?);
                (lhs, Side::Scalar(*scalar), present)
            }
            Self::ScalarColumn(scalar, right) => {
                let (rhs, present) = side(right.word_block(index)?);
                (Side::Scalar(*scalar), rhs, present)
            }
            Self::Columns(left, right) => {
                let (lhs, left_present) = side(left.word_block(index)?);
                let (rhs, right_present) = side(right.word_block(index)?);
                (lhs, rhs, left_present & right_present)
            }
        })
    }

    #[cfg(not(all(target_arch = "aarch64", not(miri))))]
    fn blocks(&self, _index: usize) -> Option<(Side<'_>, Side<'_>, u64)> {
        None
    }
}

/// Check the dimensions, choose the operation and the strategy, and run.
fn run<L, R>(
    op: ArithOp,
    operands: Operands<'_, L, R>,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()>
where
    L: ColumnReader<Value = i64>,
    R: ColumnReader<Value = i64>,
{
    let nrows = rows.nrows();
    ensure!(
        values.len() == nrows && non_nulls.as_view().nrows() == nrows,
        "result and selection row counts differ"
    );
    ensure!(
        operands
            .column_rows()
            .into_iter()
            .flatten()
            .all(|count| count == nrows),
        "column and selection row counts differ"
    );
    let output = Output {
        rows,
        values,
        non_nulls,
    };
    // The first word decides, as for the int32 kernel: selectivity is
    // roughly uniform within a batch, and a partly prepared batch refuses
    // its first word as it would the rest.
    let whole_words = cfg!(all(target_arch = "aarch64", not(miri)))
        && nrows >= 64
        && rows.word(0).is_some_and(|selected| {
            (selected == u64::MAX
                || (!selected.is_power_of_two() && selected.count_ones() >= BULK_MIN_ROWS))
                && operands.blocks(0).is_some()
        });
    // The operation is chosen once; each arm's closure is inlined into its
    // own loops, unlike a trait object, which called per row.
    match op {
        ArithOp::Add => execute(op, &operands, output, whole_words, &|a: i64, b: i64| {
            a.checked_add(b).ok_or(ArithmeticError::BigintOutOfRange)
        }),
        ArithOp::Sub => execute(op, &operands, output, whole_words, &|a: i64, b: i64| {
            a.checked_sub(b).ok_or(ArithmeticError::BigintOutOfRange)
        }),
        ArithOp::Mul => execute(op, &operands, output, whole_words, &|a: i64, b: i64| {
            a.checked_mul(b).ok_or(ArithmeticError::BigintOutOfRange)
        }),
        ArithOp::Div => execute(op, &operands, output, whole_words, &|a: i64, b: i64| {
            if b == 0 {
                Err(ArithmeticError::DivisionByZero)
            } else {
                a.checked_div(b).ok_or(ArithmeticError::BigintOutOfRange)
            }
        }),
        ArithOp::Mod => execute(op, &operands, output, whole_words, &|a: i64, b: i64| {
            if b == 0 {
                Err(ArithmeticError::DivisionByZero)
            } else {
                Ok(a.wrapping_rem(b))
            }
        }),
    }
}

/// One int8 operation on two non-NULL values.
trait Evaluate: Fn(i64, i64) -> Result<i64, ArithmeticError> {}
impl<F: Fn(i64, i64) -> Result<i64, ArithmeticError>> Evaluate for F {}

fn execute<L, R, E: Evaluate>(
    op: ArithOp,
    operands: &Operands<'_, L, R>,
    output: Output<'_, '_, '_, '_>,
    whole_words: bool,
    evaluate: &E,
) -> Result<()>
where
    L: ColumnReader<Value = i64>,
    R: ColumnReader<Value = i64>,
{
    if whole_words {
        bulk(op, operands, output, evaluate)
    } else {
        operands.rows(output, evaluate)
    }
}

/// Whole words where every column operand exposes them, rows elsewhere.
/// The lanes of a block are read straight from its storage: a scalar
/// divisor of magnitude at least two divides by the multiplier prepared
/// once per call, everything else goes through the operation's checks.
#[inline(never)]
fn bulk<L, R, E: Evaluate>(
    op: ArithOp,
    operands: &Operands<'_, L, R>,
    output: Output<'_, '_, '_, '_>,
    evaluate: &E,
) -> Result<()>
where
    L: ColumnReader<Value = i64>,
    R: ColumnReader<Value = i64>,
{
    let rows = output.rows;
    let mut output = output;
    let divisor = match (op, operands) {
        (ArithOp::Div | ArithOp::Mod, Operands::ColumnScalar(_, d)) => Divisor::new(*d),
        _ => None,
    };
    for index in 0..rows.nrows().div_ceil(64) {
        let selected = rows.word(index).unwrap();
        if selected == 0 || selected.is_power_of_two() {
            operands.word(index, selected, &mut output, evaluate)?;
            continue;
        }
        let Some((lhs, rhs, non_null)) = operands.blocks(index) else {
            operands.word(index, selected, &mut output, evaluate)?;
            continue;
        };
        let present = selected & non_null;
        let base = index * 64;
        let out: &mut [MaybeUninit<i64>; 64] = (&mut output.values[base..base + 64])
            .try_into()
            .expect("a whole-word operand implies a full word");
        let mut lanes = present;
        match (op, &divisor) {
            (ArithOp::Div, Some(divisor)) => {
                while lanes != 0 {
                    let lane = lanes.trailing_zeros() as usize;
                    lanes &= lanes - 1;
                    out[lane].write(divisor.quotient(lhs.lane(lane)));
                }
            }
            (ArithOp::Mod, Some(divisor)) => {
                while lanes != 0 {
                    let lane = lanes.trailing_zeros() as usize;
                    lanes &= lanes - 1;
                    out[lane].write(divisor.remainder(lhs.lane(lane)));
                }
            }
            _ => {
                while lanes != 0 {
                    let lane = lanes.trailing_zeros() as usize;
                    lanes &= lanes - 1;
                    out[lane].write(evaluate(lhs.lane(lane), rhs.lane(lane))?);
                }
            }
        }
        output.non_nulls.set_word(index, present)?;
    }
    Ok(())
}

/// The result buffers and the selection they follow.
struct Output<'r, 'v, 'm, 'w> {
    rows: &'r RowMaskView<'r>,
    values: &'v mut [MaybeUninit<i64>],
    non_nulls: &'m mut RowMask<'w>,
}

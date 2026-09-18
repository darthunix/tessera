//! int4 arithmetic with PostgreSQL's rules, producing a dense column.
//!
//! `+`, `-` and `*` fail with [`ArithmeticError::IntegerOutOfRange`] on
//! overflow; `/` and `%` fail with [`ArithmeticError::DivisionByZero`] on a
//! zero divisor, `/` also with `IntegerOutOfRange` for `i32::MIN / -1`, and
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
//! `non_nulls` word cleared. The result reads as a `DenseInt32Column` with
//! the selection as its readiness mask.
//!
//! When the first word of the selection is full, selects at least a dozen
//! rows and every column operand exposes its storage, the call computes
//! whole words: `+`, `-` and `*` with vector code on AArch64 (overflow
//! detected per lane and reported once per word); `/` and `%` by a scalar
//! divisor of magnitude at least two with a multiplier prepared once per
//! call (`Divisor`), applied to every lane by vector code and unable to
//! fail; other divisions lane by lane from the blocks, since NEON has no
//! integer division. Single-row words, the tail and refused words are read
//! row by row; every other call reads every word row by row through the
//! word iterators.

use std::fmt;
use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView, WordBlock};

use super::{BULK_MIN_ROWS, Divisor};

/// A binary int4 operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArithOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`, truncating toward zero.
    Div,
    /// `%`, with the dividend's sign.
    Mod,
}

/// An arithmetic failure with the SQLSTATE PostgreSQL reports for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArithmeticError {
    /// SQLSTATE 22003: an int4 result does not fit.
    IntegerOutOfRange,
    /// SQLSTATE 22012: a zero divisor.
    DivisionByZero,
}

impl ArithmeticError {
    /// The five-character SQLSTATE of the error.
    pub fn sqlstate(self) -> &'static str {
        match self {
            Self::IntegerOutOfRange => "22003",
            Self::DivisionByZero => "22012",
        }
    }
}

impl fmt::Display for ArithmeticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::IntegerOutOfRange => "integer out of range",
            Self::DivisionByZero => "division by zero",
        })
    }
}

impl std::error::Error for ArithmeticError {}

/// Compute `column op scalar` for the selected rows into `values` and
/// `non_nulls`.
///
/// `values` and `non_nulls` have the batch's row count, and so must the
/// column. The operation is chosen once per call.
///
/// # Errors
///
/// Dimension errors fail before any mutation. A reader error (including an
/// unprepared selected row) and an [`ArithmeticError`] fail the call with
/// the result unspecified; the arithmetic error is recoverable from the
/// returned error with `downcast_ref::<ArithmeticError>()`.
///
/// ```
/// use std::mem::MaybeUninit;
/// use tessera_core::{ColumnView, RowMask, RowMaskView};
/// use tessera_kernels::int32::{ArithOp, arith_scalar};
///
/// let values = [10, 20, 30, 40];
/// let non_nulls = RowMaskView::try_new(4, &[0b1101])?;
/// let column = ColumnView::try_new(&values, Some(non_nulls))?;
/// let rows = RowMaskView::try_new(4, &[0b0111])?;
/// let mut out = [MaybeUninit::uninit(); 4];
/// let mut out_words = [0];
/// let mut out_non_nulls = RowMask::try_new(4, &mut out_words)?;
/// arith_scalar(ArithOp::Mul, &column, 3, &rows, &mut out, &mut out_non_nulls)?;
/// assert_eq!(out_words, [0b0101]);
/// // SAFETY: rows 0 and 2 are selected and non-NULL, so they were written.
/// assert_eq!(unsafe { (out[0].assume_init(), out[2].assume_init()) }, (30, 90));
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn arith_scalar<C: ColumnReader<Value = i32>>(
    op: ArithOp,
    column: &C,
    scalar: i32,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i32>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    let operands: Operands<'_, C, C> = Operands::ColumnScalar(column, scalar);
    run(op, operands, rows, values, non_nulls)
}

/// Compute `scalar op column`, for the operations where the order matters.
///
/// # Errors
///
/// As for [`arith_scalar`].
pub fn arith_scalar_left<C: ColumnReader<Value = i32>>(
    op: ArithOp,
    scalar: i32,
    column: &C,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i32>],
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
    values: &mut [MaybeUninit<i32>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()>
where
    L: ColumnReader<Value = i32>,
    R: ColumnReader<Value = i32>,
{
    run(op, Operands::Columns(left, right), rows, values, non_nulls)
}

/// A whole-word operand: the storage of a full prepared word, or a constant.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Side<'a> {
    Dense(&'a [i32; 64]),
    Datum(&'a [u64; 64]),
    Scalar(i32),
}

impl Side<'_> {
    /// One lane as int4; a Datum's low 32 bits, as `DatumGetInt32`.
    #[inline(always)]
    fn lane(self, lane: usize) -> i32 {
        match self {
            Self::Dense(values) => values[lane],
            Self::Datum(values) => values[lane] as i32,
            Self::Scalar(value) => value,
        }
    }
}

/// The operands of one call.
enum Operands<'a, L, R> {
    ColumnScalar(&'a L, i32),
    ScalarColumn(i32, &'a R),
    Columns(&'a L, &'a R),
}

impl<L, R> Operands<'_, L, R>
where
    L: ColumnReader<Value = i32>,
    R: ColumnReader<Value = i32>,
{
    /// The right operand prepared for division by multiplication, when it
    /// is a scalar of magnitude at least two.
    fn scalar_divisor(&self) -> Option<Divisor> {
        match self {
            Self::ColumnScalar(_, d) => Divisor::new(*d),
            _ => None,
        }
    }

    fn column_rows(&self) -> [Option<usize>; 2] {
        match self {
            Self::ColumnScalar(left, _) => [Some(left.nrows()), None],
            Self::ScalarColumn(_, right) => [None, Some(right.nrows())],
            Self::Columns(left, right) => [Some(left.nrows()), Some(right.nrows())],
        }
    }

    /// Every word row by row. Out of line, like the whole-word loop: sharing
    /// a function with the other path's call cost this loop registers, and
    /// with them 5-20% of its instructions.
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
        fn side(block: WordBlock<'_, i32>) -> (Side<'_>, u64) {
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
    values: &mut [MaybeUninit<i32>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()>
where
    L: ColumnReader<Value = i32>,
    R: ColumnReader<Value = i32>,
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
    // The first word decides, as for the aggregates: selectivity is roughly
    // uniform within a batch, and a partly prepared batch refuses its first
    // word as it would the rest.
    let whole_words = cfg!(all(target_arch = "aarch64", not(miri)))
        && nrows >= 64
        && rows.word(0).is_some_and(|selected| {
            (selected == u64::MAX
                || (!selected.is_power_of_two() && selected.count_ones() >= BULK_MIN_ROWS))
                && operands.blocks(0).is_some()
        });
    // The operations are chosen once: each arm's closures, one per row and
    // one per whole word, are inlined into their own loops, unlike a trait
    // object or a match per word, which cost every row or word.
    match op {
        ArithOp::Add => execute(
            &operands,
            output,
            whole_words,
            &|a: i32, b: i32| a.checked_add(b).ok_or(ArithmeticError::IntegerOutOfRange),
            &mut |lhs, rhs, present, out| Ok(bulk_op::add(lhs, rhs, present, out)),
        ),
        ArithOp::Sub => execute(
            &operands,
            output,
            whole_words,
            &|a: i32, b: i32| a.checked_sub(b).ok_or(ArithmeticError::IntegerOutOfRange),
            &mut |lhs, rhs, present, out| Ok(bulk_op::sub(lhs, rhs, present, out)),
        ),
        ArithOp::Mul => execute(
            &operands,
            output,
            whole_words,
            &|a: i32, b: i32| a.checked_mul(b).ok_or(ArithmeticError::IntegerOutOfRange),
            &mut |lhs, rhs, present, out| Ok(bulk_op::mul(lhs, rhs, present, out)),
        ),
        ArithOp::Div => divide(
            &operands,
            output,
            whole_words,
            &|a: i32, b: i32| {
                if b == 0 {
                    Err(ArithmeticError::DivisionByZero)
                } else {
                    a.checked_div(b).ok_or(ArithmeticError::IntegerOutOfRange)
                }
            },
            bulk_op::div,
        ),
        ArithOp::Mod => divide(
            &operands,
            output,
            whole_words,
            &|a: i32, b: i32| {
                if b == 0 {
                    Err(ArithmeticError::DivisionByZero)
                } else {
                    Ok(a.wrapping_rem(b))
                }
            },
            bulk_op::rem,
        ),
    }
}

/// One int4 operation on two non-NULL values.
trait Evaluate: Fn(i32, i32) -> Result<i32, ArithmeticError> {}
impl<F: Fn(i32, i32) -> Result<i32, ArithmeticError>> Evaluate for F {}

/// One whole word of an operation on the blocks: every lane into the output
/// word, true when a present lane overflowed.
trait WholeWord: FnMut(Side<'_>, Side<'_>, u64, &mut [MaybeUninit<i32>; 64]) -> Result<bool> {}
impl<F: FnMut(Side<'_>, Side<'_>, u64, &mut [MaybeUninit<i32>; 64]) -> Result<bool>> WholeWord
    for F
{
}

fn execute<L, R, E: Evaluate, W: WholeWord>(
    operands: &Operands<'_, L, R>,
    output: Output<'_, '_, '_, '_>,
    whole_words: bool,
    evaluate: &E,
    whole: &mut W,
) -> Result<()>
where
    L: ColumnReader<Value = i32>,
    R: ColumnReader<Value = i32>,
{
    if whole_words {
        bulk(operands, output, evaluate, whole)
    } else {
        operands.rows(output, evaluate)
    }
}

/// `/` and `%`: whole words by multiplication through `vector` for a
/// scalar divisor of magnitude at least two, lane by lane through
/// `evaluate` otherwise (a column, zero or ±1). The divisor is prepared at
/// the first word with rows to divide, so that a selection of NULLs
/// prepares nothing, and a word without such rows computes nothing: a
/// division is too dear to spend on absent lanes, and a whole word of them
/// is a NULL column.
fn divide<L, R, E, V>(
    operands: &Operands<'_, L, R>,
    output: Output<'_, '_, '_, '_>,
    whole_words: bool,
    evaluate: &E,
    vector: V,
) -> Result<()>
where
    L: ColumnReader<Value = i32>,
    R: ColumnReader<Value = i32>,
    E: Evaluate,
    V: Fn(Side<'_>, &Divisor, &mut [MaybeUninit<i32>; 64]),
{
    let mut divisor: Option<Option<Divisor>> = None;
    execute(
        operands,
        output,
        whole_words,
        evaluate,
        &mut |lhs, rhs, present, out| {
            if present == 0 {
                return Ok(false);
            }
            match &*divisor.get_or_insert_with(|| operands.scalar_divisor()) {
                Some(divisor) => vector(lhs, divisor, out),
                None => {
                    let mut lanes = present;
                    while lanes != 0 {
                        let lane = lanes.trailing_zeros() as usize;
                        lanes &= lanes - 1;
                        out[lane].write(evaluate(lhs.lane(lane), rhs.lane(lane))?);
                    }
                }
            }
            Ok(false)
        },
    )
}

/// Whole words where every column operand exposes them, rows elsewhere.
#[inline(never)]
fn bulk<L, R, E: Evaluate, W: WholeWord>(
    operands: &Operands<'_, L, R>,
    output: Output<'_, '_, '_, '_>,
    evaluate: &E,
    whole: &mut W,
) -> Result<()>
where
    L: ColumnReader<Value = i32>,
    R: ColumnReader<Value = i32>,
{
    let rows = output.rows;
    let mut output = output;
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
        let out: &mut [MaybeUninit<i32>; 64] = (&mut output.values[base..base + 64])
            .try_into()
            .expect("a whole-word operand implies a full word");
        ensure!(
            !whole(lhs, rhs, present, out)?,
            ArithmeticError::IntegerOutOfRange
        );
        output.non_nulls.set_word(index, present)?;
    }
    Ok(())
}

#[cfg(all(target_arch = "aarch64", not(miri)))]
use crate::simd as bulk_op;

/// Without vector code no call takes the whole-word path; these keep the
/// callers compiling and are never reached.
#[cfg(not(all(target_arch = "aarch64", not(miri))))]
mod bulk_op {
    use std::mem::MaybeUninit;

    use super::{Divisor, Side};

    pub fn add(_: Side<'_>, _: Side<'_>, _: u64, _: &mut [MaybeUninit<i32>; 64]) -> bool {
        unreachable!("no whole-word kernels on this target")
    }

    pub fn div(_: Side<'_>, _: &Divisor, _: &mut [MaybeUninit<i32>; 64]) {
        unreachable!("no whole-word kernels on this target")
    }

    pub fn rem(_: Side<'_>, _: &Divisor, _: &mut [MaybeUninit<i32>; 64]) {
        unreachable!("no whole-word kernels on this target")
    }

    pub fn sub(_: Side<'_>, _: Side<'_>, _: u64, _: &mut [MaybeUninit<i32>; 64]) -> bool {
        unreachable!("no whole-word kernels on this target")
    }

    pub fn mul(_: Side<'_>, _: Side<'_>, _: u64, _: &mut [MaybeUninit<i32>; 64]) -> bool {
        unreachable!("no whole-word kernels on this target")
    }
}

/// The result buffers and the selection they follow.
struct Output<'r, 'v, 'm, 'w> {
    rows: &'r RowMaskView<'r>,
    values: &'v mut [MaybeUninit<i32>],
    non_nulls: &'m mut RowMask<'w>,
}

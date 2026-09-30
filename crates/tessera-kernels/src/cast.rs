//! Widening of int4 values to int8, as PostgreSQL's `int8(int4)` cast, and
//! narrowing of int8 values to int4, as its `int4(int8)` cast.
//!
//! Every selected non-NULL value is sign-extended into a dense int8
//! result in the caller's buffers, with the output contract of the
//! arithmetic: for every word with selected rows the kernel writes the
//! word of `non_nulls` (the selected rows whose value is non-NULL) and the
//! values of those rows, a NULL row gets an initialized placeholder, rows
//! outside the selection are unspecified and may stay uninitialized, and
//! words without selected rows have their `non_nulls` word cleared. An
//! int8 is its Datum, so the result is a column of Datums as well.
//!
//! When the first word of the selection is full, selects at least a dozen
//! rows and the reader exposes its storage, the call widens whole words
//! (vector code on AArch64) and reads only single-row words, the tail and
//! refused words row by row; otherwise every word is read through the
//! word iterators.

use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};

use crate::BULK_MIN_ROWS;
use crate::ops::ArithmeticError;

/// Widen the selected int4 values into `values` and `non_nulls`.
///
/// `values` and `non_nulls` have the batch's row count, and so must the
/// column and the selection.
///
/// # Errors
///
/// Different row counts fail before any mutation. A reader error (an
/// unprepared selected row) fails the call with the outputs unspecified.
/// Empty selections are valid.
///
/// ```
/// use std::mem::MaybeUninit;
/// use tessera_core::{ColumnView, RowMask, RowMaskView};
/// use tessera_kernels::cast::int4_to_int8;
///
/// let values = [i32::MIN, -1, 7, i32::MAX];
/// let column = ColumnView::try_new(&values, None)?;
/// let rows = RowMaskView::try_new(4, &[0b1011])?;
/// let mut out = [MaybeUninit::uninit(); 4];
/// let mut words = [0];
/// int4_to_int8(&column, &rows, &mut out, &mut RowMask::try_new(4, &mut words)?)?;
/// assert_eq!(words, [0b1011]);
/// // SAFETY: the mask marks the rows the kernel wrote.
/// assert_eq!(unsafe { out[3].assume_init() }, i64::from(i32::MAX));
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn int4_to_int8<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    let nrows = rows.nrows();
    ensure!(
        values.len() == nrows && non_nulls.as_view().nrows() == nrows,
        "result and selection row counts differ"
    );
    ensure!(
        column.nrows() == nrows,
        "column and selection row counts differ"
    );
    // The first word decides, as for the other kernels.
    let bulk = cfg!(all(target_arch = "aarch64", not(miri)))
        && nrows >= 64
        && rows.word(0).is_some_and(|selected| {
            (selected == u64::MAX
                || (!selected.is_power_of_two() && selected.count_ones() >= BULK_MIN_ROWS))
                && column.word_block(0).is_some()
        });
    if bulk {
        widen_bulk(column, rows, values, non_nulls)
    } else {
        widen_rows(column, rows, values, non_nulls)
    }
}

/// Every word row by row.
fn widen_rows<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    for index in 0..rows.nrows().div_ceil(64) {
        let selected = rows.word(index).unwrap();
        non_nulls.set_word(index, word_rows(column, index, selected, values)?)?;
    }
    Ok(())
}

/// One word row by row: the present bits of its selected rows.
#[inline(always)]
fn word_rows<C: ColumnReader<Value = i32>>(
    column: &C,
    index: usize,
    selected: u64,
    values: &mut [MaybeUninit<i64>],
) -> Result<u64> {
    if selected == 0 {
        return Ok(0);
    }
    let mut present = 0;
    for (row, value) in column.word_values(index, selected)? {
        // A NULL row gets a placeholder without a branch on nullness.
        values[row].write(i64::from(value.unwrap_or(0)));
        present |= u64::from(value.is_some()) << (row % 64);
    }
    Ok(present)
}

/// Narrow the selected int8 values into `values` and `non_nulls`, with the
/// output contract of [`int4_to_int8`]; a selected non-NULL value outside
/// the int4 range fails the call.
///
/// Row by row: the cast serves an explicit `bigint_column::int`, not the
/// functions over an int4 and an int8, which widen instead.
///
/// # Errors
///
/// Different row counts fail before any mutation. A reader error and
/// [`ArithmeticError::IntegerOutOfRange`] (SQLSTATE 22003, "integer out of
/// range", as PostgreSQL reports the cast) fail the call with the outputs
/// unspecified. Empty selections are valid.
///
/// ```
/// use std::mem::MaybeUninit;
/// use tessera_core::{ColumnView, RowMask, RowMaskView};
/// use tessera_kernels::cast::int8_to_int4;
///
/// let values = [i64::from(i32::MIN), -1, 1 << 40, i64::from(i32::MAX)];
/// let column = ColumnView::try_new(&values, None)?;
/// let rows = RowMaskView::try_new(4, &[0b1011])?;
/// let mut out = [MaybeUninit::uninit(); 4];
/// let mut words = [0];
/// int8_to_int4(&column, &rows, &mut out, &mut RowMask::try_new(4, &mut words)?)?;
/// assert_eq!(words, [0b1011]);
/// // SAFETY: the mask marks the rows the kernel wrote.
/// assert_eq!(unsafe { out[3].assume_init() }, i32::MAX);
/// // The row past the int4 range fails once it is selected.
/// let all = RowMaskView::try_new(4, &[0b1111])?;
/// assert!(int8_to_int4(&column, &all, &mut out, &mut RowMask::try_new(4, &mut words)?).is_err());
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn int8_to_int4<C: ColumnReader<Value = i64>>(
    column: &C,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i32>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    let nrows = rows.nrows();
    ensure!(
        values.len() == nrows && non_nulls.as_view().nrows() == nrows,
        "result and selection row counts differ"
    );
    ensure!(
        column.nrows() == nrows,
        "column and selection row counts differ"
    );
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.word(index).unwrap();
        let mut present = 0;
        if selected != 0 {
            for (row, value) in column.word_values(index, selected)? {
                let narrow = match value {
                    Some(value) => {
                        i32::try_from(value).map_err(|_| ArithmeticError::IntegerOutOfRange)?
                    }
                    None => 0,
                };
                values[row].write(narrow);
                present |= u64::from(value.is_some()) << (row % 64);
            }
        }
        non_nulls.set_word(index, present)?;
    }
    Ok(())
}

/// Whole words where the reader exposes them, rows elsewhere.
#[inline(never)]
fn widen_bulk<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    for index in 0..rows.nrows().div_ceil(64) {
        let selected = rows.word(index).unwrap();
        let present = if selected == 0 || selected.is_power_of_two() {
            word_rows(column, index, selected, values)?
        } else if let Some(block) = column.word_block(index) {
            let base = index * 64;
            let out: &mut [MaybeUninit<i64>; 64] = (&mut values[base..base + 64])
                .try_into()
                .expect("a whole-word operand implies a full word");
            bulk::widen(block, selected, out)
        } else {
            word_rows(column, index, selected, values)?
        };
        non_nulls.set_word(index, present)?;
    }
    Ok(())
}

/// The whole-word kernel: every lane widened, and the present bits of the
/// selected non-NULL rows returned.
#[cfg(all(target_arch = "aarch64", not(miri)))]
mod bulk {
    use std::mem::MaybeUninit;

    use tessera_core::WordBlock;

    use crate::simd;

    #[inline]
    pub fn widen(
        block: WordBlock<'_, i32>,
        selected: u64,
        out: &mut [MaybeUninit<i64>; 64],
    ) -> u64 {
        match block {
            WordBlock::Dense { values, non_nulls } => {
                simd::widen_dense(values, out);
                selected & non_nulls
            }
            WordBlock::Datum { values, isnull } => {
                simd::widen_datum(values, out);
                selected & simd::non_null_bits(isnull)
            }
        }
    }
}

/// Without vector code no call takes the whole-word path; this keeps the
/// callers compiling and is never reached.
#[cfg(not(all(target_arch = "aarch64", not(miri))))]
mod bulk {
    use std::mem::MaybeUninit;

    use tessera_core::WordBlock;

    pub fn widen(_: WordBlock<'_, i32>, _: u64, _: &mut [MaybeUninit<i64>; 64]) -> u64 {
        unreachable!("no whole-word kernels on this target")
    }
}

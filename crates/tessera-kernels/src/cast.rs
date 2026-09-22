//! Widening of int4 values to int8, as PostgreSQL's `int8(int4)` cast.
//!
//! Every selected non-NULL value is sign-extended into a dense int8
//! result in the caller's buffers, with the output contract of the
//! arithmetic: for every word with selected rows the kernel writes the
//! word of `non_nulls` (the selected rows whose value is non-NULL) and the
//! values of those rows, a NULL row gets an initialized placeholder, rows
//! outside the selection are unspecified and may stay uninitialized, and
//! words without selected rows have their `non_nulls` word cleared. An
//! int8 is its Datum, so the result is a column of Datums as well. Every
//! word is read through the word iterators; the whole-word path follows,
//! measured against this one.

use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};

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
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.word(index).unwrap();
        if selected == 0 {
            non_nulls.set_word(index, 0)?;
            continue;
        }
        let mut present = 0;
        for (row, value) in column.word_values(index, selected)? {
            // A NULL row gets a placeholder without a branch on nullness.
            values[row].write(i64::from(value.unwrap_or(0)));
            present |= u64::from(value.is_some()) << (row % 64);
        }
        non_nulls.set_word(index, present)?;
    }
    Ok(())
}

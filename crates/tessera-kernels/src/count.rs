//! The number of selected non-NULL rows, for values of any type.
//!
//! A count reads no value: NULL rows are skipped and the rest are counted,
//! so one kernel serves every width and every type whose column exposes
//! its NULL flags, `count(int8)` and `count(text)` alike. `count(*)` is
//! [`RowMaskView::selected_count`]. When the first word of the selection is
//! full, selects at least a dozen rows and the reader exposes its storage,
//! the call counts whole words: the flags of a Datum block with vector code
//! on AArch64, the non-NULL bits of a dense block with a population count;
//! otherwise it reads through [`ColumnReader::try_fold_selected`].

use anyhow::Result;
use tessera_core::{ColumnReader, RowMaskView, WordBlock};

use crate::BULK_MIN_ROWS;

/// Count the selected non-NULL rows.
///
/// # Errors
///
/// Different row counts and unprepared selected rows fail as in
/// [`ColumnReader::try_fold_selected`]; rows already counted are not reported.
///
/// ```
/// use tessera_core::{ColumnView, RowMaskView};
/// use tessera_kernels::count::count;
///
/// let values = [(), (), (), ()];
/// let non_nulls = RowMaskView::try_new(4, &[0b1101])?;
/// let column = ColumnView::try_new(&values, Some(non_nulls))?;
/// let rows = RowMaskView::try_new(4, &[0b0111])?;
/// assert_eq!(count(&column, &rows)?, 2);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn count<C: ColumnReader>(column: &C, rows: &RowMaskView<'_>) -> Result<usize> {
    let bulk = cfg!(all(target_arch = "aarch64", not(miri)))
        && rows.nrows() >= 64
        && rows.word(0).is_some_and(|selected| {
            (selected == u64::MAX
                || (!selected.is_power_of_two() && selected.count_ones() >= BULK_MIN_ROWS))
                && column.word_block(0).is_some()
        });
    if bulk {
        count_bulk(column, rows)
    } else {
        column.try_fold_selected(rows, 0, |count, _, value| {
            Ok(count + usize::from(value.is_some()))
        })
    }
}

/// Whole words where the reader exposes them; single rows through `get`,
/// the tail and refused words through the word iterator's bulk fold.
#[inline(never)]
fn count_bulk<C: ColumnReader>(column: &C, rows: &RowMaskView<'_>) -> Result<usize> {
    let mut count = 0;
    for index in 0..rows.nrows().div_ceil(64) {
        let selected = rows.word(index).unwrap();
        if selected == 0 {
            continue;
        }
        if selected.is_power_of_two() {
            let row = index * 64 + selected.trailing_zeros() as usize;
            count += usize::from(column.get(row)?.is_some());
        } else if let Some(block) = column.word_block(index) {
            count += block_count(block, selected);
        } else {
            count += column
                .word_values(index, selected)?
                .fold(0, |count, (_, value)| count + usize::from(value.is_some()));
        }
    }
    Ok(count)
}

/// The selected non-NULL rows of a block, from its flags alone.
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline]
fn block_count<T>(block: WordBlock<'_, T>, selected: u64) -> usize {
    match block {
        WordBlock::Dense { non_nulls, .. } => (selected & non_nulls).count_ones() as usize,
        WordBlock::Datum { isnull, .. } => crate::simd::count_datum(isnull, selected),
    }
}

/// Without vector code no call takes the whole-word path.
#[cfg(not(all(target_arch = "aarch64", not(miri))))]
fn block_count<T>(_: WordBlock<'_, T>, _: u64) -> usize {
    unreachable!("no whole-word kernels on this target")
}

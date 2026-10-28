//! The number of selected non-NULL rows, for values of any type.
//!
//! A count reads no value: NULL rows are skipped and the rest are counted,
//! so one kernel serves every width and every type whose column exposes
//! its NULL flags, `count(int8)` and `count(text)` alike. `count(*)` is
//! [`RowMaskView::selected_count`]. When the first word of the selection is
//! full, selects at least a dozen rows and the reader exposes its storage,
//! the call counts whole words: the flags of a Datum block with vector code
//! (NEON on AArch64, AVX2 on x86-64 where the processor has it), the
//! non-NULL bits of a dense block with a population count; otherwise it
//! reads through [`ColumnReader::try_fold_selected`].

use anyhow::Result;
use tessera_core::{ColumnReader, RowMaskView, WordBlock};

use crate::BULK_MIN_ROWS;
use crate::simd::{Simd, Token};

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
    let simd = Simd::get();
    let bulk = simd.is_some()
        && rows.nrows() >= 64
        && rows.get_word(0).is_some_and(|selected| {
            (selected == u64::MAX
                || (!selected.is_power_of_two() && selected.count_ones() >= BULK_MIN_ROWS))
                && column.word_block(0).is_some()
        });
    if let Some(simd) = simd
        && bulk
    {
        count_bulk(simd, column, rows)
    } else {
        column.try_fold_selected(rows, 0, |count, _, value| {
            Ok(count + usize::from(value.is_some()))
        })
    }
}

/// Whole words where the reader exposes them; single rows through `get`,
/// the tail and refused words through the word iterator's bulk fold.
#[inline(never)]
fn count_bulk<C: ColumnReader>(simd: Simd, column: &C, rows: &RowMaskView<'_>) -> Result<usize> {
    let mut count = 0;
    for index in 0..rows.nrows().div_ceil(64) {
        let selected = rows.word(index);
        if selected == 0 {
            continue;
        }
        if selected.is_power_of_two() {
            let row = index * 64 + selected.trailing_zeros() as usize;
            count += usize::from(column.get(row)?.is_some());
        } else if let Some(block) = column.word_block(index) {
            count += block_count(simd, block, selected);
        } else {
            count += column
                .word_values(index, selected)?
                .fold(0, |count, (_, value)| count + usize::from(value.is_some()));
        }
    }
    Ok(count)
}

/// The selected non-NULL rows of a block, from its flags alone.
#[inline]
fn block_count<T>(simd: Simd, block: WordBlock<'_, T>, selected: u64) -> usize {
    match block {
        WordBlock::Dense { non_nulls, .. } => (selected & non_nulls).count_ones() as usize,
        WordBlock::Datum { isnull, .. } => crate::simd::count_datum(simd, isnull, selected),
    }
}

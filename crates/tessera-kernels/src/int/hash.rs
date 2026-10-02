//! The drivers of the key hashes of both widths: the choice between whole
//! words and rows, the NULL policy and the chain of keys. A key's 32 bits
//! are the width's ([`IntLane::key`]); the whole-word hashes are its vector
//! code.

use anyhow::{Context, Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};

use super::{IntLane, Side};
use crate::BULK_MIN_ROWS;
use crate::int32::hash::NULL_KEY;
use crate::int32::{NullKeys, hash_combine, murmurhash32};

/// The driver of [`crate::int32::hash`] and [`crate::int64::hash`].
pub(crate) fn hash<T: IntLane, C: ColumnReader<Value = T>>(
    column: &C,
    rows: &RowMaskView<'_>,
    nulls: NullKeys,
    hashes: &mut [u32],
    valid: &mut RowMask<'_>,
) -> Result<()> {
    ensure!(
        rows.nrows() == valid.as_view().nrows(),
        "selection and mask row counts differ"
    );
    run(
        column,
        rows,
        nulls,
        hashes,
        valid,
        |_, key| key,
        Step::First,
    )
}

/// The driver of [`crate::int32::hash_next`] and [`crate::int64::hash_next`].
pub(crate) fn hash_next<T: IntLane, C: ColumnReader<Value = T>>(
    column: &C,
    nulls: NullKeys,
    hashes: &mut [u32],
    valid: &mut RowMask<'_>,
) -> Result<()> {
    run(
        column,
        &Valid,
        nulls,
        hashes,
        valid,
        hash_combine,
        Step::Next,
    )
}

/// Which key of the chain a call hashes: the whole-word form of the fold.
#[derive(Clone, Copy)]
enum Step {
    First,
    Next,
}

/// Where a call's selection words come from: a type, not a runtime choice,
/// so that each instance of the loops reads its source directly.
trait Selection {
    fn word(&self, valid: &RowMask<'_>, index: usize) -> u64;
}

/// The first key selects from the caller's rows.
impl Selection for RowMaskView<'_> {
    #[inline(always)]
    fn word(&self, _: &RowMask<'_>, index: usize) -> u64 {
        RowMaskView::word_at(self, index)
    }
}

/// The next keys select from the valid mask itself.
struct Valid;

impl Selection for Valid {
    #[inline(always)]
    fn word(&self, valid: &RowMask<'_>, index: usize) -> u64 {
        valid.as_view().word_at(index)
    }
}

/// Check the dimensions, choose the strategy by the first word, and run:
/// whole words out of line, rows here, where the row loop knows the
/// hashes' length and pays no bounds check per row. The fold is chosen once
/// per call and inlined into the row loop; `step` is its whole-word form.
fn run<T, C, F, S>(
    column: &C,
    selection: &S,
    nulls: NullKeys,
    hashes: &mut [u32],
    valid: &mut RowMask<'_>,
    fold: F,
    step: Step,
) -> Result<()>
where
    T: IntLane,
    C: ColumnReader<Value = T>,
    F: Fn(u32, u32) -> u32,
    S: Selection,
{
    let nrows = valid.as_view().nrows();
    ensure!(
        column.nrows() == nrows && hashes.len() == nrows,
        "column, hashes and mask row counts differ"
    );
    let reject = nulls == NullKeys::Reject;
    // The first word decides, as for the arithmetic.
    let whole_words = cfg!(all(target_arch = "aarch64", not(miri))) && nrows >= 64 && {
        let selected = selection.word(valid, 0);
        (selected == u64::MAX
            || (!selected.is_power_of_two() && selected.count_ones() >= BULK_MIN_ROWS))
            && block(column, 0).is_some()
    };
    if whole_words {
        return bulk(column, selection, reject, step, hashes, valid, &fold);
    }
    for index in 0..nrows.div_ceil(64) {
        let selected = selection.word(valid, index);
        let present = word(column, index, selected, reject, hashes, &fold)?;
        valid.set_word(index, present)?;
    }
    Ok(())
}

/// Whole words where the column exposes them, rows elsewhere.
#[inline(never)]
fn bulk<T, C, F, S>(
    column: &C,
    selection: &S,
    reject: bool,
    step: Step,
    hashes: &mut [u32],
    valid: &mut RowMask<'_>,
    fold: &F,
) -> Result<()>
where
    T: IntLane,
    C: ColumnReader<Value = T>,
    F: Fn(u32, u32) -> u32,
    S: Selection,
{
    for index in 0..valid.as_view().nrows().div_ceil(64) {
        let selected = selection.word(valid, index);
        if selected == 0 || selected.is_power_of_two() {
            let present = word(column, index, selected, reject, hashes, fold)?;
            valid.set_word(index, present)?;
            continue;
        }
        let Some((keys, non_null)) = block(column, index) else {
            let present = word(column, index, selected, reject, hashes, fold)?;
            valid.set_word(index, present)?;
            continue;
        };
        let base = index * 64;
        let out: &mut [u32; 64] = (&mut hashes[base..base + 64])
            .try_into()
            .context("a whole-word block implies a full word")?;
        let present = if reject {
            match step {
                Step::First => T::hash_block(keys, out),
                Step::Next => T::combine_block(keys, out),
            }
            selected & non_null
        } else {
            match step {
                Step::First => T::hash_nulls_block(keys, non_null, out),
                Step::Next => T::combine_nulls_block(keys, non_null, out),
            }
            selected
        };
        valid.set_word(index, present)?;
    }
    Ok(())
}

/// The storage of a whole word with its non-NULL rows, when exposed.
#[cfg(all(target_arch = "aarch64", not(miri)))]
fn block<T: IntLane, C: ColumnReader<Value = T>>(
    column: &C,
    index: usize,
) -> Option<(Side<'_, T>, u64)> {
    use tessera_core::WordBlock;

    Some(match column.word_block(index)? {
        WordBlock::Dense { values, non_nulls } => (Side::Dense(values), non_nulls),
        WordBlock::Datum { values, isnull } => {
            (Side::Datum(values), crate::simd::non_null_bits(isnull))
        }
    })
}

#[cfg(not(all(target_arch = "aarch64", not(miri))))]
fn block<T: IntLane, C: ColumnReader<Value = T>>(
    column: &C,
    index: usize,
) -> Option<(Side<'_, T>, u64)> {
    let _ = (column, index);
    None
}

/// One word row by row, without a branch on nullness: a NULL row hashes
/// the group key and keeps its bit only under the group policy.
#[inline(always)]
fn word<T, C, F>(
    column: &C,
    index: usize,
    selected: u64,
    reject: bool,
    hashes: &mut [u32],
    fold: &F,
) -> Result<u64>
where
    T: IntLane,
    C: ColumnReader<Value = T>,
    F: Fn(u32, u32) -> u32,
{
    if selected == 0 {
        return Ok(0);
    }
    let mut present = 0;
    for (row, value) in column.word_values(index, selected)? {
        let some = value.is_some();
        let key = value.map_or(NULL_KEY, T::key);
        hashes[row] = fold(hashes[row], murmurhash32(key));
        present |= u64::from(some | !reject) << (row % 64);
    }
    Ok(present)
}

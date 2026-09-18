//! Hashes of int4 keys for hash joins, grouping and partitioning, as pg_batch
//! computes them.
//!
//! A key's hash is [`murmurhash32`] of its value read as `u32` (MurmurHash3's
//! finalizer, PostgreSQL's `murmurhash32`); a batch's hashes are 32 bits
//! wide, one per row. Several keys combine in key order with
//! [`hash_combine`], the first key uncombined. Consumers take buckets from
//! the high bits (`hash >> shift & (n - 1)`) and partitions from the low
//! ones. The finalizer is a bijection and the combination is invertible in
//! its second argument, so equal combined hashes with equal leading keys
//! prove the last key equal, which pg_batch relies on to skip comparing it.
//! The hash is internal to Tessera and does not match PostgreSQL's
//! `hashint4`.
//!
//! NULL keys follow a policy: [`NullKeys::Reject`] removes the row from the
//! valid mask, for joins, where NULL matches nothing; [`NullKeys::Group`]
//! hashes it as the constant `0x9e3779b9`, so that NULLs form one group.
//!
//! [`hash`] hashes the first key of the selected rows and sets the valid
//! mask; [`hash_next`] folds a further key into the hashes of the valid rows
//! and narrows the mask, reading no row rejected before. `hashes` has the
//! batch's row count; rows outside the valid mask hold unspecified values,
//! and the buffer may be reused across batches. Dimension errors fail before
//! any mutation; a reader error leaves the hashes and the mask partly
//! updated, to be discarded.
//!
//! When the first word of the selection is full or selects at least a dozen
//! rows and the column exposes its storage, the call hashes whole words with
//! vector code on AArch64: every lane of the word, NULL lanes included (they
//! get the group key under the group policy and meaningless values, outside
//! the valid mask, under rejection). Single-row words, the tail and refused
//! words go row by row; every other call reads every word row by row.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView, WordBlock};

use super::{BULK_MIN_ROWS, Side};

/// What a NULL key hashes as under [`NullKeys::Group`], before finalizing.
pub(crate) const NULL_KEY: u32 = 0x9e37_79b9;

/// What a NULL key does to its row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NullKeys {
    /// The row leaves the valid mask: a NULL key matches nothing in a join.
    Reject,
    /// The row stays and NULL hashes as a fixed key: NULLs form one group.
    Group,
}

/// MurmurHash3's 32-bit finalizer, as PostgreSQL's `murmurhash32`: a
/// bijection on `u32` with full avalanche.
#[inline]
pub fn murmurhash32(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h
}

/// Fold the hash `b` of a further key into `a`, as PostgreSQL's
/// `hash_combine`; invertible in `b` for a fixed `a`.
#[inline]
pub fn hash_combine(a: u32, b: u32) -> u32 {
    a ^ b
        .wrapping_add(0x9e37_79b9)
        .wrapping_add(a << 6)
        .wrapping_add(a >> 2)
}

/// Hash the first key: `hashes[row]` for the selected rows and `valid` as
/// the selection narrowed by the NULL policy.
///
/// # Errors
///
/// Row counts of the column, the selection, `hashes` and `valid` must
/// agree; a reader error (including an unprepared selected row) fails the
/// call with the outputs partly written.
///
/// ```
/// use tessera_core::{ColumnView, RowMask, RowMaskView};
/// use tessera_kernels::int32::{NullKeys, hash, hash_next, murmurhash32, hash_combine};
///
/// let keys = [10, 20, 30];
/// let column = ColumnView::try_new(&keys, Some(RowMaskView::try_new(3, &[0b101])?))?;
/// let rows = RowMaskView::try_new(3, &[0b111])?;
/// let mut hashes = [0; 3];
/// let mut words = [0];
/// let mut valid = RowMask::try_new(3, &mut words)?;
/// hash(&column, &rows, NullKeys::Reject, &mut hashes, &mut valid)?;
/// assert_eq!(valid.as_view().word(0), Some(0b101));
/// assert_eq!(hashes[0], murmurhash32(10));
/// hash_next(&column, NullKeys::Reject, &mut hashes, &mut valid)?;
/// assert_eq!(hashes[2], hash_combine(murmurhash32(30), murmurhash32(30)));
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn hash<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &RowMaskView<'_>,
    nulls: NullKeys,
    hashes: &mut [u32],
    valid: &mut RowMask<'_>,
) -> Result<()> {
    run(
        column,
        Some(rows),
        nulls,
        hashes,
        valid,
        |_, key| key,
        Step::First,
    )
}

/// Fold the next key into the hashes of the valid rows, narrowing `valid`
/// by the NULL policy.
///
/// # Errors
///
/// As for [`hash`].
pub fn hash_next<C: ColumnReader<Value = i32>>(
    column: &C,
    nulls: NullKeys,
    hashes: &mut [u32],
    valid: &mut RowMask<'_>,
) -> Result<()> {
    run(column, None, nulls, hashes, valid, hash_combine, Step::Next)
}

/// Which key of the chain a call hashes: the whole-word form of the fold.
#[derive(Clone, Copy)]
enum Step {
    First,
    Next,
}

/// Check the dimensions, choose the strategy by the first word, and run.
/// The selection comes from `rows` for the first key and from `valid`
/// itself for the next ones. The fold is chosen once per call and inlined
/// into the row loop; `step` is its whole-word form.
fn run<C, F>(
    column: &C,
    rows: Option<&RowMaskView<'_>>,
    nulls: NullKeys,
    hashes: &mut [u32],
    valid: &mut RowMask<'_>,
    fold: F,
    step: Step,
) -> Result<()>
where
    C: ColumnReader<Value = i32>,
    F: Fn(u32, u32) -> u32,
{
    let nrows = valid.as_view().nrows();
    ensure!(
        column.nrows() == nrows
            && hashes.len() == nrows
            && rows.is_none_or(|rows| rows.nrows() == nrows),
        "column, hashes and mask row counts differ"
    );
    let reject = nulls == NullKeys::Reject;
    // The first word decides, as for the arithmetic.
    let whole_words = cfg!(all(target_arch = "aarch64", not(miri))) && nrows >= 64 && {
        let selected = selection(rows, valid, 0);
        (selected == u64::MAX
            || (!selected.is_power_of_two() && selected.count_ones() >= BULK_MIN_ROWS))
            && block(column, 0).is_some()
    };
    if whole_words {
        bulk(column, rows, reject, step, hashes, valid, &fold)
    } else {
        by_rows(column, rows, reject, hashes, valid, &fold)
    }
}

/// The selection word of the first key's rows or of the valid mask.
#[inline(always)]
fn selection(rows: Option<&RowMaskView<'_>>, valid: &RowMask<'_>, index: usize) -> u64 {
    match rows {
        Some(rows) => rows.word(index),
        None => valid.as_view().word(index),
    }
    .unwrap()
}

/// Every word row by row. Out of line, like the whole-word loop.
#[inline(never)]
fn by_rows<C, F>(
    column: &C,
    rows: Option<&RowMaskView<'_>>,
    reject: bool,
    hashes: &mut [u32],
    valid: &mut RowMask<'_>,
    fold: &F,
) -> Result<()>
where
    C: ColumnReader<Value = i32>,
    F: Fn(u32, u32) -> u32,
{
    for index in 0..valid.as_view().nrows().div_ceil(64) {
        let selected = selection(rows, valid, index);
        let present = word(column, index, selected, reject, hashes, fold)?;
        valid.set_word(index, present)?;
    }
    Ok(())
}

/// Whole words where the column exposes them, rows elsewhere.
#[inline(never)]
fn bulk<C, F>(
    column: &C,
    rows: Option<&RowMaskView<'_>>,
    reject: bool,
    step: Step,
    hashes: &mut [u32],
    valid: &mut RowMask<'_>,
    fold: &F,
) -> Result<()>
where
    C: ColumnReader<Value = i32>,
    F: Fn(u32, u32) -> u32,
{
    for index in 0..valid.as_view().nrows().div_ceil(64) {
        let selected = selection(rows, valid, index);
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
            .expect("a whole-word block implies a full word");
        let present = if reject {
            match step {
                Step::First => bulk_op::hash(keys, out),
                Step::Next => bulk_op::combine(keys, out),
            }
            selected & non_null
        } else {
            match step {
                Step::First => bulk_op::hash_nulls(keys, non_null, out),
                Step::Next => bulk_op::combine_nulls(keys, non_null, out),
            }
            selected
        };
        valid.set_word(index, present)?;
    }
    Ok(())
}

/// The storage of a whole word with its non-NULL rows, when exposed.
#[cfg(all(target_arch = "aarch64", not(miri)))]
fn block<C: ColumnReader<Value = i32>>(column: &C, index: usize) -> Option<(Side<'_>, u64)> {
    Some(match column.word_block(index)? {
        WordBlock::Dense { values, non_nulls } => (Side::Dense(values), non_nulls),
        WordBlock::Datum { values, isnull } => {
            (Side::Datum(values), crate::simd::non_null_bits(isnull))
        }
    })
}

#[cfg(not(all(target_arch = "aarch64", not(miri))))]
fn block<C: ColumnReader<Value = i32>>(column: &C, index: usize) -> Option<(Side<'_>, u64)> {
    let _ = (column, index);
    None
}

#[cfg(all(target_arch = "aarch64", not(miri)))]
use crate::simd as bulk_op;

/// Without vector code no call takes the whole-word path; these keep the
/// callers compiling and are never reached.
#[cfg(not(all(target_arch = "aarch64", not(miri))))]
mod bulk_op {
    use super::Side;

    pub fn hash(_: Side<'_>, _: &mut [u32; 64]) {
        unreachable!("no whole-word kernels on this target")
    }

    pub fn hash_nulls(_: Side<'_>, _: u64, _: &mut [u32; 64]) {
        unreachable!("no whole-word kernels on this target")
    }

    pub fn combine(_: Side<'_>, _: &mut [u32; 64]) {
        unreachable!("no whole-word kernels on this target")
    }

    pub fn combine_nulls(_: Side<'_>, _: u64, _: &mut [u32; 64]) {
        unreachable!("no whole-word kernels on this target")
    }
}

/// One word row by row, without a branch on nullness: a NULL row hashes
/// the group key and keeps its bit only under the group policy.
#[inline(always)]
fn word<C, F>(
    column: &C,
    index: usize,
    selected: u64,
    reject: bool,
    hashes: &mut [u32],
    fold: &F,
) -> Result<u64>
where
    C: ColumnReader<Value = i32>,
    F: Fn(u32, u32) -> u32,
{
    if selected == 0 {
        return Ok(0);
    }
    let mut present = 0;
    for (row, value) in column.word_values(index, selected)? {
        let some = value.is_some();
        let key = value.map_or(NULL_KEY, |value| value as u32);
        hashes[row] = fold(hashes[row], murmurhash32(key));
        present |= u64::from(some | !reject) << (row % 64);
    }
    Ok(present)
}

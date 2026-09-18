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

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};

/// What a NULL key hashes as under [`NullKeys::Group`], before finalizing.
const NULL_KEY: u32 = 0x9e37_79b9;

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
    run(column, Some(rows), nulls, hashes, valid, |_, key| key)
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
    run(column, None, nulls, hashes, valid, hash_combine)
}

/// Every word row by row: the selection comes from `rows` for the first key
/// and from `valid` itself for the next ones. The fold is chosen once per
/// call and inlined into the loop.
fn run<C, F>(
    column: &C,
    rows: Option<&RowMaskView<'_>>,
    nulls: NullKeys,
    hashes: &mut [u32],
    valid: &mut RowMask<'_>,
    fold: F,
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
    for index in 0..nrows.div_ceil(64) {
        let selected = match rows {
            Some(rows) => rows.word(index),
            None => valid.as_view().word(index),
        }
        .unwrap();
        let present = word(column, index, selected, reject, hashes, &fold)?;
        valid.set_word(index, present)?;
    }
    Ok(())
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

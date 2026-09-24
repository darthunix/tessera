//! Hashes of int8 keys, compatible with the int4 hashes of
//! [`crate::int32::hash`].
//!
//! A key's hash is [`murmurhash32`] of its value folded to 32 bits as
//! PostgreSQL's `hashint8` folds it: the low half xor the high half, the
//! high half inverted for negative values (`lo ^ (v >= 0 ? hi : !hi)`).
//! For a value inside the int4 range the high half is all zeros or all
//! ones and the fold gives the low half, the value read as `u32`, so an
//! int8 key hashes exactly like the int4 key of the same value: a join of
//! an int4 column with an int8 one uses one table, whose records keep keys
//! as i64 and compare them whole. The fold is not a bijection, so equal
//! hashes of int8 keys never prove the keys equal. Further keys combine
//! with [`hash_combine`]; NULL keys follow [`NullKeys`] as for int4, a NULL
//! under the group policy hashing like the int4 group key.
//!
//! The contract of [`hash`] and [`hash_next`] is that of the int4 kernels:
//! `hashes` has the batch's row count and rows outside the valid mask hold
//! unspecified values; dimension errors come before any change, and a
//! reader error leaves the outputs partly written.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};

use crate::int32::hash::NULL_KEY;
pub use crate::int32::hash::{NullKeys, hash_combine, murmurhash32};

/// An int8 folded to 32 bits as PostgreSQL's `hashint8` folds it; the
/// value itself for values in the int4 range.
#[inline(always)]
pub fn fold(value: i64) -> u32 {
    let low = value as u32;
    let high = (value >> 32) as u32;
    low ^ if value >= 0 { high } else { !high }
}

/// Hash the first key: `hashes[row]` for the selected rows and `valid` as
/// the selection narrowed by the NULL policy.
///
/// # Errors
///
/// Row counts of the column, the selection, `hashes` and `valid` must
/// agree; a reader error fails the call with the outputs partly written.
///
/// ```
/// use tessera_core::{ColumnView, RowMask, RowMaskView};
/// use tessera_kernels::{int32, int64};
///
/// let keys = [10_i64, -7, 1 << 40];
/// let column = ColumnView::try_new(&keys, None)?;
/// let rows = RowMaskView::try_new(3, &[0b111])?;
/// let mut hashes = [0; 3];
/// let mut words = [0];
/// let mut valid = RowMask::try_new(3, &mut words)?;
/// int64::hash(&column, &rows, int64::NullKeys::Reject, &mut hashes, &mut valid)?;
/// // Inside the int4 range an int8 hashes like the int4 of its value.
/// assert_eq!(hashes[0], int32::murmurhash32(10));
/// assert_eq!(hashes[1], int32::murmurhash32(-7_i32 as u32));
/// assert_eq!(hashes[2], int32::murmurhash32(1 << 8));
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn hash<C: ColumnReader<Value = i64>>(
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
    run(column, rows, nulls, hashes, valid, |_, key| key)
}

/// Fold the next key into the hashes of the valid rows, narrowing `valid`
/// by the NULL policy.
///
/// # Errors
///
/// As for [`hash`].
pub fn hash_next<C: ColumnReader<Value = i64>>(
    column: &C,
    nulls: NullKeys,
    hashes: &mut [u32],
    valid: &mut RowMask<'_>,
) -> Result<()> {
    run(column, &Valid, nulls, hashes, valid, hash_combine)
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
        RowMaskView::word(self, index).unwrap()
    }
}

/// The next keys select from the valid mask itself.
struct Valid;

impl Selection for Valid {
    #[inline(always)]
    fn word(&self, valid: &RowMask<'_>, index: usize) -> u64 {
        valid.as_view().word(index).unwrap()
    }
}

/// Check the dimensions and hash word by word, in the function that knows
/// the hashes' length, so that the row loop pays no bounds check per row.
fn run<C, F, S>(
    column: &C,
    selection: &S,
    nulls: NullKeys,
    hashes: &mut [u32],
    valid: &mut RowMask<'_>,
    fold_hash: F,
) -> Result<()>
where
    C: ColumnReader<Value = i64>,
    F: Fn(u32, u32) -> u32,
    S: Selection,
{
    let nrows = valid.as_view().nrows();
    ensure!(
        column.nrows() == nrows && hashes.len() == nrows,
        "column, hashes and mask row counts differ"
    );
    let reject = nulls == NullKeys::Reject;
    for index in 0..nrows.div_ceil(64) {
        let selected = selection.word(valid, index);
        let present = word(column, index, selected, reject, hashes, &fold_hash)?;
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
    fold_hash: &F,
) -> Result<u64>
where
    C: ColumnReader<Value = i64>,
    F: Fn(u32, u32) -> u32,
{
    if selected == 0 {
        return Ok(0);
    }
    let mut present = 0;
    for (row, value) in column.word_values(index, selected)? {
        let some = value.is_some();
        let key = value.map_or(NULL_KEY, fold);
        hashes[row] = fold_hash(hashes[row], murmurhash32(key));
        present |= u64::from(some | !reject) << (row % 64);
    }
    Ok(present)
}

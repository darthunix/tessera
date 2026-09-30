//! Comparisons of two int4 columns of a batch, narrowing a row mask.

use anyhow::Result;
use tessera_core::{ColumnReader, RowMask};

use super::CompareOp;

/// Keep the selected rows where both columns are non-NULL and
/// `left op right`.
///
/// Row indices remain physical; removed rows are never restored. The first
/// word with several selected rows decides the strategy, as for
/// [`super::filter`]: when it is full or selects at least a dozen rows and
/// both readers expose its storage, every multi-row word both expose is
/// compared whole (vector code on AArch64); every other word is read at its
/// selected rows, the two columns in lockstep. Nothing is allocated.
///
/// # Errors
///
/// Different row counts fail before any change. A reader error leaves the
/// current word and later ones intact and earlier ones filtered: the caller
/// discards the selection.
pub fn compare_columns<L, R>(
    left: &L,
    right: &R,
    rows: &mut RowMask<'_>,
    op: CompareOp,
) -> Result<()>
where
    L: ColumnReader<Value = i32>,
    R: ColumnReader<Value = i32>,
{
    crate::int::compare_columns(left, right, rows, op)
}

mod ints;

pub use ints::{
    DatumInt32Column, DatumInt64Column, DatumIntColumn, DatumNullsColumn, DenseInt32Column,
    DenseInt64Column, DenseIntColumn, FromDatum,
};

use anyhow::{Result, ensure};
use tessera_core::{RowMaskView, ones};

fn validate_mask(nrows: usize, mask: Option<RowMaskView<'_>>) -> Result<()> {
    ensure!(
        mask.is_none_or(|mask| mask.nrows() == nrows),
        "column and mask row counts differ"
    );
    Ok(())
}

#[inline(always)]
fn validate_ready(nrows: usize, prepared: Option<RowMaskView<'_>>, row: usize) -> Result<()> {
    ensure!(
        row < nrows,
        "physical row {row} is out of bounds for {nrows} rows"
    );
    if let Some(mask) = prepared {
        ensure!(mask.contains(row)?, "physical row {row} is unprepared");
    }
    Ok(())
}

#[inline]
fn mask_word(mask: Option<RowMaskView<'_>>, word_index: usize) -> u64 {
    mask.map_or(u64::MAX, |mask| mask.get_word(word_index).unwrap_or(0))
}

// Shared early-exit loop over prepared selection words. The caller chooses the
// NULL mode once and passes a matching read closure; the loop validates each
// nonempty word before reading it and stops at the first reader or consumer
// error without rolling back rows already folded.
//
// A full word walks its 64 rows with a counted loop: one loop exit per word
// instead of one taken branch per row, and no bit scan per row, which also
// lets the compiler vectorize the reads. Measured on Apple M5 Pro, the cost
// of the bit-scan loop depended on where it landed in the fetch blocks
// (20-30% of cycles, sometimes a branch miss per word); fewer taken branches
// per row make that layout lottery cheaper to lose.
// Full words live in a function of their own: the vector constants the
// compiler materializes for the counted loop are then paid per full word,
// not at the entry of every call, where empty, sparse and partial selections
// would carry them for nothing.
// Both were measured again on an Intel Core i5 with AVX2: there full words
// read by the bit scan took 2 to 7 times the cycles of the counted loop, and
// the loop inlined cost empty and sparse calls up to 22% more
// (compare-NUZTP5, compare-5YmvqE; compare-6OWqws, compare-RxRTzU).
#[inline(never)]
fn try_fold_full_word<B, V, E>(
    base: usize,
    bits: u64,
    read: &mut impl FnMut(usize, u64) -> Option<V>,
    mut acc: B,
    fold: &mut impl FnMut(B, usize, Option<V>) -> Result<B, E>,
) -> Result<B, E> {
    for bit in 0..64 {
        let row = base + bit;
        acc = fold(acc, row, read(row, bits))?;
    }
    Ok(acc)
}

#[inline(always)]
fn try_fold_words<B, V>(
    nrows: usize,
    rows: &RowMaskView<'_>,
    prepared: Option<RowMaskView<'_>>,
    non_nulls: Option<RowMaskView<'_>>,
    read: impl FnMut(usize, u64) -> Option<V>,
    acc: B,
    fold: impl FnMut(B, usize, Option<V>) -> Result<B>,
) -> Result<B> {
    // Each refusal builds its error in place, as `ensure!` did, so the
    // walk compiles as it did before it moved.
    walk_words(
        nrows,
        rows,
        prepared,
        non_nulls,
        read,
        acc,
        fold,
        |refusal| match refusal {
            WalkRefusal::Rows => anyhow::anyhow!("column and selection row counts differ"),
            WalkRefusal::Unprepared => anyhow::anyhow!("selection contains unprepared rows"),
        },
    )
}

/// Why [`walk_words`] refuses a selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalkRefusal {
    /// The selection's row count is not the column's.
    Rows,
    /// A word of the selection has a selected row that is not prepared.
    Unprepared,
}

/// The walk of [`try_fold_words`], with its errors of any type: a refusal
/// is what `refuse` makes of its reason, a failed fold step ends the walk
/// with its error. It builds no `anyhow` error of its own, which keeps it
/// within reach of the proofs.
#[inline(always)]
#[allow(
    clippy::too_many_arguments,
    reason = "the arguments of try_fold_words and its refusal"
)]
fn walk_words<B, V, E>(
    nrows: usize,
    rows: &RowMaskView<'_>,
    prepared: Option<RowMaskView<'_>>,
    non_nulls: Option<RowMaskView<'_>>,
    mut read: impl FnMut(usize, u64) -> Option<V>,
    mut acc: B,
    mut fold: impl FnMut(B, usize, Option<V>) -> Result<B, E>,
    refuse: impl Fn(WalkRefusal) -> E,
) -> Result<B, E> {
    if nrows != rows.nrows() {
        return Err(refuse(WalkRefusal::Rows));
    }
    for index in 0..nrows.div_ceil(64) {
        let remaining = rows.word(index);
        if remaining == 0 {
            continue;
        }
        if remaining & !mask_word(prepared, index) != 0 {
            return Err(refuse(WalkRefusal::Unprepared));
        }
        let bits = mask_word(non_nulls, index);
        let base = index * 64;
        if remaining == u64::MAX {
            acc = try_fold_full_word(base, bits, &mut read, acc, &mut fold)?;
        } else {
            for bit in ones(remaining) {
                let row = base + bit;
                acc = fold(acc, row, read(row, bits))?;
            }
        }
    }
    Ok(acc)
}

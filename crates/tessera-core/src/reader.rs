use std::iter::FusedIterator;

use anyhow::{Result, ensure};

use crate::RowMaskView;
use crate::bitmap::word_count;

/// Statically typed reading, independent of a column's physical storage.
///
/// `None` denotes NULL, not an unavailable row. Indices are physical rows.
/// Implementations return errors for out-of-bounds or unprepared rows before
/// reading their data. Generic consumers select a value type at compile time,
/// for example `C: ColumnReader<Value = i32>`; no dynamic dispatch is needed.
///
/// [`Self::try_fold_selected`] is the generic entry point for reducing one
/// column of any value type over a selection. Several columns are combined in
/// lockstep per selection word: for one `selected` word, the
/// [`Self::word_values`] iterators of every column yield the same rows in the
/// same order and can be zipped without per-row results. [`Self::get`] reads
/// single rows. [`Self::word_block`] exposes the storage of one full,
/// fully prepared word to kernels that process rows in bulk; representations
/// without such storage return `None` and are served by the row paths.
pub trait ColumnReader {
    /// A value or a reference borrowed from the representation's input buffers.
    /// No `Copy`, `Clone`, or `'static` bound is imposed on all representations.
    type Value;

    /// Return the number of physical rows, including NULL and unprepared rows.
    fn nrows(&self) -> usize;

    /// Read one row after checking bounds, readiness, and then nullness.
    fn get(&self, row: usize) -> Result<Option<Self::Value>>;

    /// Read selected rows of one 64-row word in increasing physical order.
    ///
    /// Reject an absent word, selected padding, or any unprepared selected row
    /// before reading any data in this word. The copied selection word permits
    /// a caller to modify its active mask independently of the iterator.
    fn word_values(
        &self,
        word_index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<Self::Value>)> + '_>;

    /// Storage of one full, fully prepared word for bulk kernels.
    ///
    /// `None` when the representation has no bulk storage, the word is the
    /// short tail or out of range, or any of its rows is unprepared; callers
    /// then fall back to [`Self::word_values`]. A returned block covers exactly
    /// the word's 64 rows; NULL rows hold initialized values of no meaning that
    /// must not be interpreted. The selection is not applied: the caller masks
    /// its own result. This operation reads no row data.
    fn word_block(&self, word_index: usize) -> Option<WordBlock<'_, Self::Value>> {
        let _ = word_index;
        None
    }

    /// Fold over selected rows in physical order, stopping at the first error.
    ///
    /// Different row counts fail immediately. Empty words are skipped; each
    /// nonempty word gets the same readiness validation as [`Self::word_values`]
    /// before any of its rows is read. The default implementation delegates to
    /// that method. A reader error or an `Err` from `fold` ends the operation
    /// and is returned; rows already folded are not rolled back. This
    /// operation mutates no data.
    fn try_fold_selected<B, F>(&self, rows: &RowMaskView<'_>, init: B, mut fold: F) -> Result<B>
    where
        F: FnMut(B, usize, Option<Self::Value>) -> Result<B>,
    {
        ensure!(
            self.nrows() == rows.nrows(),
            "column and selection row counts differ"
        );
        let mut acc = init;
        for index in 0..word_count(rows.nrows()) {
            let selected = rows.word(index).unwrap();
            if selected == 0 {
                continue;
            }
            for (row, value) in self.word_values(index, selected)? {
                acc = fold(acc, row, value)?;
            }
        }
        Ok(acc)
    }
}

/// The storage of one full, fully prepared 64-row word, in physical order.
///
/// Bulk kernels read it directly instead of through [`WordValues`]. Rows are
/// `base..base + 64` of the word; a NULL row's value is initialized but
/// meaningless and must not be interpreted. The value type `T` says how a
/// value is read, not what a `Datum` is: a Datum block encodes `T` in the low
/// bits of each `u64`, as the representation defines.
#[derive(Debug, Clone, Copy)]
pub enum WordBlock<'a, T> {
    /// One value per row; bit `i` of `non_nulls` set means row `i` is non-NULL.
    Dense { values: &'a [T; 64], non_nulls: u64 },
    /// PostgreSQL Datum storage with one NULL flag per row.
    Datum {
        values: &'a [u64; 64],
        isnull: &'a [bool; 64],
    },
}

/// A validated, allocation-free iterator over one selection word.
///
/// Its private state guarantees that `read` is called only for selected,
/// prepared rows below `nrows`, at most once each, in increasing order. This
/// guarantee also applies to callers relying on it for unsafe buffer access.
/// The reader must handle nullness before accessing a possibly NULL value.
pub struct WordValues<F> {
    base: usize,
    remaining: u64,
    read: F,
}

impl<F> WordValues<F> {
    /// Validate bounds, padding, and readiness without invoking `read`.
    ///
    /// `prepared` describes the same physical word as `selected`; use
    /// `u64::MAX` when all rows are prepared. Even an empty selection must
    /// refer to an existing word. No per-row readiness checks follow.
    #[inline]
    pub fn try_new<V>(
        nrows: usize,
        word_index: usize,
        selected: u64,
        prepared: u64,
        read: F,
    ) -> Result<Self>
    where
        F: FnMut(usize) -> Option<V>,
    {
        ensure!(
            word_index < word_count(nrows),
            "selection word is out of bounds"
        );
        let base = word_index * 64;
        let selected_width = 64 - selected.leading_zeros() as usize;
        ensure!(
            selected_width <= nrows - base,
            "selection has set padding bits"
        );
        ensure!(
            selected & !prepared == 0,
            "selection contains unprepared rows"
        );
        Ok(Self {
            base,
            remaining: selected,
            read,
        })
    }
}

impl<F, V> Iterator for WordValues<F>
where
    F: FnMut(usize) -> Option<V>,
{
    type Item = (usize, Option<V>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let row = self.base + self.remaining.trailing_zeros() as usize;
        self.remaining &= self.remaining - 1;
        Some((row, (self.read)(row)))
    }

    /// The bulk path: a full word walks its rows with a counted loop, one
    /// loop exit per word instead of a taken branch and a bit scan per row.
    /// Rows already taken with `next` are not revisited.
    #[inline]
    fn fold<B, G>(self, mut acc: B, mut fold: G) -> B
    where
        G: FnMut(B, Self::Item) -> B,
    {
        let Self {
            base,
            mut remaining,
            mut read,
        } = self;
        if remaining == u64::MAX {
            acc = fold_full_word(base, &mut read, acc, &mut fold);
        } else {
            while remaining != 0 {
                let row = base + remaining.trailing_zeros() as usize;
                remaining &= remaining - 1;
                acc = fold(acc, (row, read(row)));
            }
        }
        acc
    }
}

impl<F, V> FusedIterator for WordValues<F> where F: FnMut(usize) -> Option<V> {}

// Kept out of line so that the vector setup the compiler generates for the
// counted loop is paid per full word rather than at the entry of every caller.
#[inline(never)]
fn fold_full_word<B, V, F, G>(base: usize, read: &mut F, mut acc: B, fold: &mut G) -> B
where
    F: FnMut(usize) -> Option<V>,
    G: FnMut(B, (usize, Option<V>)) -> B,
{
    for bit in 0..64 {
        let row = base + bit;
        acc = fold(acc, (row, read(row)));
    }
    acc
}

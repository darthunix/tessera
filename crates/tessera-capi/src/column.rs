mod int32;

pub use int32::{DatumInt32Column, DenseInt32Column};

use anyhow::{Result, ensure};
use tessera_core::RowMaskView;

fn validate_mask(nrows: usize, mask: Option<RowMaskView<'_>>) -> Result<()> {
    ensure!(
        mask.is_none_or(|mask| mask.nrows() == nrows),
        "column and mask row counts differ"
    );
    Ok(())
}

#[inline]
fn validate_ready(nrows: usize, prepared: Option<RowMaskView<'_>>, row: usize) -> Result<()> {
    ensure!(
        row < nrows,
        "physical row {row} is out of bounds for {nrows} rows"
    );
    ensure!(
        prepared.is_none_or(|mask| mask.contains(row).unwrap()),
        "physical row {row} is unprepared"
    );
    Ok(())
}

#[inline]
fn mask_word(mask: Option<RowMaskView<'_>>, word_index: usize) -> u64 {
    mask.map_or(u64::MAX, |mask| mask.word(word_index).unwrap_or(0))
}

// Both adapters share a scalar cursor. Bulk fold keeps the value loop inside
// each prepared word rather than repeatedly advancing an outer iterator.
struct SelectedValues<'a, F> {
    rows: RowMaskView<'a>,
    prepared: Option<RowMaskView<'a>>,
    non_nulls: Option<RowMaskView<'a>>,
    read: F,
    next_word: usize,
    end_word: usize,
    remaining: u64,
    base: usize,
    non_null_bits: u64,
}

#[inline]
fn selected_values<'a, V, F>(
    nrows: usize,
    rows: RowMaskView<'a>,
    prepared: Option<RowMaskView<'a>>,
    non_nulls: Option<RowMaskView<'a>>,
    read: F,
) -> Result<impl Iterator<Item = Result<(usize, Option<V>)>> + 'a>
where
    F: FnMut(usize, u64) -> Option<V> + 'a,
{
    ensure!(
        nrows == rows.nrows(),
        "column and selection row counts differ"
    );
    Ok(SelectedValues {
        rows,
        prepared,
        non_nulls,
        read,
        next_word: 0,
        end_word: nrows.div_ceil(64),
        remaining: 0,
        base: 0,
        non_null_bits: u64::MAX,
    })
}

impl<F> SelectedValues<'_, F> {
    #[inline(always)]
    fn advance_word(&mut self) -> Result<bool> {
        while self.next_word < self.end_word {
            let index = self.next_word;
            self.next_word += 1;
            let selected = self.rows.word(index).unwrap();
            if selected == 0 {
                continue;
            }
            if selected & !mask_word(self.prepared, index) != 0 {
                self.next_word = self.end_word;
                anyhow::bail!("selection contains unprepared rows");
            }
            self.non_null_bits = mask_word(self.non_nulls, index);
            self.base = index * 64;
            self.remaining = selected;
            return Ok(true);
        }
        Ok(false)
    }
}

impl<F, V> Iterator for SelectedValues<'_, F>
where
    F: FnMut(usize, u64) -> Option<V>,
{
    type Item = Result<(usize, Option<V>)>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            match self.advance_word() {
                Ok(true) => {}
                Ok(false) => return None,
                Err(error) => return Some(Err(error)),
            }
        }
        let row = self.base + self.remaining.trailing_zeros() as usize;
        self.remaining &= self.remaining - 1;
        Some(Ok((row, (self.read)(row, self.non_null_bits))))
    }

    #[inline(always)]
    fn fold<B, G>(mut self, mut acc: B, mut fold: G) -> B
    where
        G: FnMut(B, Self::Item) -> B,
    {
        loop {
            while self.remaining != 0 {
                let row = self.base + self.remaining.trailing_zeros() as usize;
                self.remaining &= self.remaining - 1;
                acc = fold(acc, Ok((row, (self.read)(row, self.non_null_bits))));
            }
            match self.advance_word() {
                Ok(true) => {}
                Ok(false) => return acc,
                Err(error) => return fold(acc, Err(error)),
            }
        }
    }
}

// Private Rust iterator dispatch, not a C object or part of the C ABI.
// The variants keep differently typed iterators behind one return type without
// boxing: Plain checks readiness but has no NULL mask; Nullable also checks
// nullness; Ready has neither mask and reads selected indices directly.
// The mode is chosen at construction and never changes during iteration.
// Bulk fold dispatches once to its specialized loop, avoiding a per-row test
// for whether a NULL mask exists. next() still dispatches to the active variant.
enum DenseSelected<P, N, R> {
    Plain(P),
    Nullable(N),
    Ready(R),
}

impl<P: Iterator, N: Iterator<Item = P::Item>, R: Iterator<Item = P::Item>> Iterator
    for DenseSelected<P, N, R>
{
    type Item = P::Item;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Plain(iter) => iter.next(),
            Self::Nullable(iter) => iter.next(),
            Self::Ready(iter) => iter.next(),
        }
    }

    #[inline(always)]
    fn fold<B, F>(self, acc: B, fold: F) -> B
    where
        F: FnMut(B, Self::Item) -> B,
    {
        match self {
            Self::Plain(iter) => iter.fold(acc, fold),
            Self::Nullable(iter) => iter.fold(acc, fold),
            Self::Ready(iter) => iter.fold(acc, fold),
        }
    }
}

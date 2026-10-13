//! The ones of a word: the numbers of its bits that are 1, lowest first.
//!
//! This is how a caller visits the rows a mask selects. Bit `bit` of word
//! `index` of a mask stands for row `64 * index + bit`, so the ones of
//! each word are the selected rows:
//!
//! ```
//! use relhash::{RowMaskView, ones};
//!
//! let rows = RowMaskView::try_new(70, &[0b1010_0100, 0b10])?;
//! let mut selected = Vec::new();
//! for index in 0..2 {
//!     for bit in ones(rows.word_at(index)) {
//!         selected.push(64 * index + bit);
//!     }
//! }
//! assert_eq!(selected, [2, 5, 7, 65]);
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! Each step takes the lowest one and clears it (`word &= word - 1`). A
//! loop that takes the step by hand gives mutation testing mutants of the
//! step that make the loop endless, a test timeout each; through [`ones`]
//! the step has one place, which `.cargo/mutants.toml` leaves out and the
//! tests below check.

use core::iter::FusedIterator;

/// The ones of `word`, lowest first; `len()` tells how many are left.
#[inline(always)]
pub fn ones(word: u64) -> impl ExactSizeIterator<Item = usize> + FusedIterator {
    Ones(word)
}

/// The walk of [`ones`]: the bits of the word not visited yet.
struct Ones(u64);

impl Iterator for Ones {
    type Item = usize;

    #[inline(always)]
    fn next(&mut self) -> Option<usize> {
        if self.0 == 0 {
            return None;
        }
        let bit = self.0.trailing_zeros() as usize;
        self.0 &= self.0 - 1;
        Some(bit)
    }

    #[inline(always)]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let count = self.0.count_ones() as usize;
        (count, Some(count))
    }
}

impl ExactSizeIterator for Ones {}

impl FusedIterator for Ones {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_set_bit_comes_once_lowest_first() {
        for word in [0, 1, 0b1011_0000, u64::MAX, 1 << 63, 0x8000_0000_0000_0001] {
            let expected: Vec<usize> = (0..64).filter(|&bit| word >> bit & 1 == 1).collect();
            let mut walk = ones(word);
            assert_eq!(walk.len(), expected.len());
            assert_eq!(walk.by_ref().collect::<Vec<_>>(), expected);
            assert_eq!((walk.next(), walk.len()), (None, 0), "{word:#x} stays over");
        }
        let mut walk = ones(0b1010_0100);
        assert_eq!(walk.next(), Some(2));
        assert_eq!(walk.len(), 2);
        assert_eq!(walk.collect::<Vec<_>>(), [5, 7]);
    }
}

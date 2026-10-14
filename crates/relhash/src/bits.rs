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
//!     for bit in ones(rows.word(index)) {
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

/// The ones of `word`, lowest first.
#[inline(always)]
pub fn ones(word: u64) -> impl Iterator<Item = usize> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Each word gives the numbers of its bits that are 1, lowest first,
    /// and then nothing: a word with none, the lowest bit alone, a few
    /// bits, all 64, the highest bit alone, the lowest and highest.
    #[test]
    fn ones_gives_the_numbers_of_the_bits_that_are_1_lowest_first() {
        for (word, expected) in [
            (0, vec![]),
            (1, vec![0]),
            (0b1011_0000, vec![4, 5, 7]),
            (u64::MAX, (0..64).collect::<Vec<usize>>()),
            (1 << 63, vec![63]),
            (0x8000_0000_0000_0001, vec![0, 63]),
        ] {
            let mut walk = ones(word);
            assert_eq!(walk.by_ref().collect::<Vec<_>>(), expected, "{word:#x}");
            assert_eq!(walk.next(), None, "{word:#x} gives nothing more");
        }
    }

    /// The walk's hint of how many ones are left is exact at every step.
    #[test]
    fn the_size_hint_counts_the_ones_left() {
        let mut walk = ones(0b1010_0100);
        assert_eq!(walk.size_hint(), (3, Some(3)));
        assert_eq!(walk.next(), Some(2));
        assert_eq!(walk.size_hint(), (2, Some(2)));
        assert_eq!(walk.collect::<Vec<_>>(), [5, 7]);
    }
}

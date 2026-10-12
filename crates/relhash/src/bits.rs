//! The set bits of a word, lowest first: the walk every batch loop takes
//! over a word of a row mask, a word of lanes or a word of NULL flags.
//!
//! The step lives here alone. A loop that takes it by hand
//! (`x &= x - 1`) gives mutation testing three mutants of the step that
//! make the loop endless, each costing a test timeout; through [`ones`]
//! the step has one place, which `.cargo/mutants.toml` leaves out and the
//! tests below check, and every loop body stays a mutant's target.

use core::iter::FusedIterator;

/// The indices of the set bits of a word not visited yet, lowest first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ones(u64);

/// The set bits of `word`, lowest first.
#[inline(always)]
pub fn ones(word: u64) -> Ones {
    Ones(word)
}

impl Ones {
    /// The bits not visited yet.
    #[inline(always)]
    pub fn rest(self) -> u64 {
        self.0
    }
}

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
            assert_eq!(
                (walk.next(), walk.rest()),
                (None, 0),
                "{word:#x} stays over"
            );
        }
        let mut walk = ones(0b1010_0100);
        assert_eq!(walk.next(), Some(2));
        assert_eq!((walk.rest(), walk.len()), (0b1010_0000, 2));
    }
}

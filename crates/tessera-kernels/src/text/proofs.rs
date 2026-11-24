//! The proofs by Kani of a string's sort word: for any two strings of up
//! to [`MAX`] bytes, [`order_by_word`], their words compared as signed
//! integers, then their bytes after the eighth, then their lengths,
//! orders them as their unsigned bytes do, compared here by hand, since
//! Kani cannot follow the library's comparison of slices.
//!
//! Bounds: strings of 0 to 16 bytes, which reach a word of fewer than
//! eight bytes, of eight, and one with bytes after it on either side. A
//! longer string adds bytes after the eighth, which the property test of
//! the module covers.

use std::cmp::Ordering;

use super::{order_by_word, sort_word};

/// The longest string of the proofs.
const MAX: usize = 16;

/// The order of two strings' unsigned bytes, by hand: the first byte
/// that differs, else the shorter first.
fn bytes_order(a: &[u8], b: &[u8]) -> Ordering {
    let mut at = 0;
    while at < a.len() && at < b.len() {
        if a[at] != b[at] {
            return a[at].cmp(&b[at]);
        }
        at += 1;
    }
    a.len().cmp(&b.len())
}

/// Any string of up to [`MAX`] bytes, in `storage`.
fn any_string(storage: &mut [u8; MAX]) -> &[u8] {
    *storage = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= MAX);
    &storage[..len]
}

#[kani::proof]
#[kani::unwind(18)]
fn strings_order_by_their_words_as_by_their_bytes() {
    let (mut left, mut right) = ([0_u8; MAX], [0_u8; MAX]);
    let (a, b) = (any_string(&mut left), any_string(&mut right));
    let order = order_by_word(a, b);
    assert_eq!(order, bytes_order(a, b));
    let same_word = sort_word(a) == sort_word(b);
    kani::cover!(same_word && a.len() > 8 && b.len() > 8 && order != Ordering::Equal);
    kani::cover!(same_word && a.len() != b.len());
    kani::cover!(!a.is_empty() && !b.is_empty() && a[0] >= 0x80 && b[0] < 0x80);
    kani::cover!(a.len() < 8 && b.len() > 8 && order == Ordering::Less);
}

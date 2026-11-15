//! The one layer through which the crate reads words from bytes it did
//! not write in the same call: a block read back from a file, the pages
//! of a file's lists, and a chunk its host gives it. Every call of a
//! function here carries a mark `// trust: <point>`, at the end of its
//! line or in the comments right above it, that names a point of "Where
//! the crate trusts its bytes" in the crate's design: what the bytes
//! decide and what checks them. `.github/scripts/check-trust.sh` fails
//! on a call without a mark, and on a word read from bytes outside this
//! module.

use crate::set::Entry;

/// The byte at `at` of `bytes`.
#[inline(always)]
pub(crate) fn u8_at(bytes: &[u8], at: usize) -> u8 {
    bytes[at]
}

/// The `u32` at byte `at` of `bytes`, in the machine's order.
#[inline(always)]
pub(crate) fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_ne_bytes(word)
}

/// The `u64` at byte `at` of `bytes`, in the machine's order.
#[inline(always)]
pub(crate) fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_ne_bytes(word)
}

/// The whole values of `W` bytes that `bytes` holds, one after another.
#[inline(always)]
pub(crate) fn values<const W: usize>(bytes: &[u8]) -> &[[u8; W]] {
    bytes.as_chunks::<W>().0
}

/// The `u32` of a value of 4 bytes, in the machine's order.
#[inline(always)]
pub(crate) fn u32_of(value: [u8; 4]) -> u32 {
    u32::from_ne_bytes(value)
}

/// The `u64` of a value of 8 bytes, in the machine's order.
#[inline(always)]
pub(crate) fn u64_of(value: [u8; 8]) -> u64 {
    u64::from_ne_bytes(value)
}

/// The entries of a page the host filled from a file.
#[inline(always)]
pub(crate) fn entries(page: &[Entry]) -> &[Entry] {
    page
}

/// Entry `index` of a page the host filled from a file.
#[inline(always)]
pub(crate) fn entry(page: &[Entry], index: usize) -> Entry {
    page[index]
}

/// Entry `index` of a page, when the page holds it.
#[inline(always)]
pub(crate) fn entry_in(page: &[Entry], index: usize) -> Option<Entry> {
    page.get(index).copied()
}

/// Word `index` of a page, read as words of 8 bytes, two to an entry:
/// its `offset` then its `stored`.
#[inline(always)]
pub(crate) fn page_word(page: &[Entry], index: usize) -> u64 {
    let entry = page[index / 2];
    if index.is_multiple_of(2) {
        entry.offset
    } else {
        entry.stored
    }
}

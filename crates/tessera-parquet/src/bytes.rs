//! The bytes form of a window in its cheaper check: the places of its
//! values' bytes, taken from the offsets of a dictionary's values or from
//! the views of a plain window, and the check of a plain window's text
//! once a window, which shows that every value of the window passes the
//! checks of UTF-8 and of zero bytes that each value's own check by its
//! read plan would make. Where either cannot show it, the caller reads
//! the window row by row through the plan, so that a value that fails,
//! or offsets that do not hold, fail as the plan fails.

use arrow_array::{Array, BinaryViewArray};

/// The start and the length of the bytes of `slot` of an array whose
/// `offsets` point into a buffer of `len` bytes, or `None` where they do
/// not hold: a slot past the offsets, an offset below 0, going back or
/// past the buffer.
pub(crate) fn slot_range(offsets: &[i64], len: usize, slot: usize) -> Option<(usize, usize)> {
    let start = usize::try_from(*offsets.get(slot)?).ok()?;
    let end = usize::try_from(*offsets.get(slot.checked_add(1)?)?).ok()?;
    (start <= end && end <= len).then(|| (start, end - start))
}

/// Whether every value of a view array that is not `NULL` is UTF-8
/// without a zero byte, each value checked by itself: the values of a
/// view array do not lie together, a short one in its view and a long one
/// in the decompressed page it was read from.
pub(crate) fn views_pass(views: &BinaryViewArray) -> bool {
    views.iter().flatten().all(value_passes)
}

/// Whether `bytes` are UTF-8 without a zero byte, in one pass by words of
/// 8 bytes: a zero byte found exactly, and a byte at or above 0x80 sending
/// the value to the full check of UTF-8, which a value of ASCII never
/// takes. A call a value: a search for a zero byte and a check of UTF-8
/// of their own cost more than the bytes of a short value.
#[inline]
pub(crate) fn value_passes(bytes: &[u8]) -> bool {
    const LOW: u64 = 0x7f7f_7f7f_7f7f_7f7f;
    // The high bit of each byte that is zero, and no other bit: the low
    // seven bits plus 0x7f carry into the high bit unless they are zero,
    // never into the next byte.
    let zeros = |w: u64| !((w & LOW).wrapping_add(LOW) | w | LOW);
    let (mut zero, mut high) = (0u64, 0u64);
    let (words, tail) = bytes.as_chunks::<8>();
    for &word in words {
        let w = u64::from_le_bytes(word);
        zero |= zeros(w);
        high |= w;
    }
    // The tail padded with bytes that are neither zero nor high.
    let mut last = [1u8; 8];
    last[..tail.len()].copy_from_slice(tail);
    let w = u64::from_le_bytes(last);
    zero |= zeros(w);
    high |= w;
    zero == 0 && (high & !LOW == 0 || core::str::from_utf8(bytes).is_ok())
}

/// The bytes of a window's rows where its cheaper check passed, read in
/// place: through a dictionary, the `offsets` into the `data` of its
/// values and the `keys` that give each row the slot of its value; a
/// plain window, by the views of its array, each row its own slot.
pub(crate) enum Slots<'a> {
    Dictionary {
        offsets: &'a [i64],
        data: &'a [u8],
        keys: &'a [i32],
    },
    Views(&'a BinaryViewArray),
}

impl<'a> Slots<'a> {
    /// The bytes of `row`: `None` where its key or its slot's offsets do
    /// not hold, for a row past the window, or where the value is longer
    /// than `max`, so that the caller reads the window row by row and
    /// fails as the plan fails.
    pub(crate) fn value(&self, row: usize, max: usize) -> Option<&'a [u8]> {
        let value = match *self {
            Slots::Dictionary {
                offsets,
                data,
                keys,
            } => {
                let slot = usize::try_from(*keys.get(row)?).ok()?;
                let (start, len) = slot_range(offsets, data.len(), slot)?;
                &data[start..start + len]
            }
            // The crate's views of a valid array lie within its buffers.
            Slots::Views(views) => (row < views.len()).then(|| views.value(row))?,
        };
        (value.len() <= max).then_some(value)
    }
}

/// Sets in `made` the bit of each row of `asked` below `rows` that
/// `is_null` says is `NULL`; `None` where `made` is shorter than a row
/// asked.
pub(crate) fn mark_nulls(
    rows: usize,
    asked: &[u64],
    is_null: impl Fn(usize) -> bool,
    made: &mut [u64],
) -> Option<()> {
    // The rows asked alone, by the set bits of their words: a batch asks
    // 64 rows of a window of 1024.
    for (word, &bits) in asked.iter().enumerate() {
        let mut bits = bits;
        while bits != 0 {
            let row = word * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            if row >= rows {
                break;
            }
            if is_null(row) {
                *made.get_mut(word)? |= 1 << (row % 64);
            }
        }
    }
    Some(())
}

#[cfg(test)]
mod tests;

#[cfg(kani)]
mod proofs;

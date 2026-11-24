//! The bytes form of a window in its cheaper check: the places of its
//! values' bytes, taken from the offsets of its Arrow arrays, and the
//! check of a window's text in one pass, which shows that every value of
//! the window passes the checks of UTF-8 and of zero bytes that each
//! value's own check by its read plan would make. Where either cannot
//! show it, the caller reads the window row by row through the plan, so
//! that a value that fails, or offsets that do not hold, fail as they do
//! today.

/// The start and the length of the bytes of `slot` of an array whose
/// `offsets` point into a buffer of `len` bytes, or `None` where they do
/// not hold: a slot past the offsets, an offset below 0, going back or
/// past the buffer.
pub(crate) fn slot_range(offsets: &[i64], len: usize, slot: usize) -> Option<(usize, usize)> {
    let start = usize::try_from(*offsets.get(slot)?).ok()?;
    let end = usize::try_from(*offsets.get(slot.checked_add(1)?)?).ok()?;
    (start <= end && end <= len).then(|| (start, end - start))
}

/// Whether every value of an array whose `offsets` point into `data`,
/// the bytes between two neighbouring offsets, is UTF-8 without a zero
/// byte, decided in one pass: the bytes from the first offset to the last
/// are UTF-8 without a zero byte, and no value that is not empty begins
/// with a byte that continues a character. False also where the offsets
/// do not hold, as [`slot_range`] reads them.
pub(crate) fn window_passes(offsets: &[i64], data: &[u8]) -> bool {
    let (Some(&first), Some(&last)) = (offsets.first(), offsets.last()) else {
        return true;
    };
    let (Ok(first), Ok(last)) = (usize::try_from(first), usize::try_from(last)) else {
        return false;
    };
    if first > last || last > data.len() {
        return false;
    }
    let bytes = &data[first..last];
    if bytes.contains(&0) || core::str::from_utf8(bytes).is_err() {
        return false;
    }
    (0..offsets.len() - 1).all(|slot| match slot_range(offsets, data.len(), slot) {
        Some((start, len)) => {
            start >= first && start + len <= last && (len == 0 || data[start] & 0xC0 != 0x80)
        }
        None => false,
    })
}

/// The bytes of a window's rows where its cheaper check passed, read in
/// place: the `offsets` into the `data` of its strings, and, through a
/// dictionary, the `keys` that give each row the slot of its value; a
/// plain window's row is its own slot.
pub(crate) struct Slots<'a> {
    pub(crate) offsets: &'a [i64],
    pub(crate) data: &'a [u8],
    pub(crate) keys: Option<&'a [i32]>,
}

impl<'a> Slots<'a> {
    /// The bytes of `row`: `None` where its key or its slot's offsets do
    /// not hold, or where the value is longer than `max`, so that the
    /// caller reads the window row by row and fails as the plan fails.
    pub(crate) fn value(&self, row: usize, max: usize) -> Option<&'a [u8]> {
        let slot = match self.keys {
            Some(keys) => usize::try_from(*keys.get(row)?).ok()?,
            None => row,
        };
        let (start, len) = slot_range(self.offsets, self.data.len(), slot)?;
        (len <= max).then(|| &self.data[start..start + len])
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

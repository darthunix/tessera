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

/// The bytes of the rows of `asked` that are not `NULL`, sliced from the
/// `offsets` into the `data` of `strings`, for a window of `rows` rows
/// whose values passed their checks, with each row's bit of `NULL` set in
/// `made`; `None` where a slot's offsets do not hold or a value is longer
/// than `max`, so that the caller reads the window row by row and fails as
/// the plan fails. `slot_of` gives the slot of a row: itself, or its index
/// in a dictionary.
pub(crate) fn slices<'a>(
    rows: usize,
    asked: &[u64],
    is_null: impl Fn(usize) -> bool,
    slot_of: impl Fn(usize) -> usize,
    strings: (&[i64], &'a [u8]),
    max: usize,
    made: &mut [u64],
) -> Option<Vec<&'a [u8]>> {
    let (offsets, data) = strings;
    let mut values: Vec<&[u8]> = vec![&[]; rows];
    for (row, value) in values.iter_mut().enumerate() {
        if asked.get(row / 64).is_none_or(|w| w >> (row % 64) & 1 == 0) {
            continue;
        }
        if is_null(row) {
            *made.get_mut(row / 64)? |= 1 << (row % 64);
            continue;
        }
        let (start, len) = slot_range(offsets, data.len(), slot_of(row))?;
        if len > max {
            return None;
        }
        *value = &data[start..start + len];
    }
    Some(values)
}

#[cfg(test)]
mod tests;

#[cfg(kani)]
mod proofs;

//! The columns a conversion takes and gives: Arrow's buffers, borrowed
//! from the caller, and the slots a call asks for.
//!
//! A column is given as Arrow's buffers hold it, each slice starting at
//! the column's first slot: a slice of the values of a fixed width, the
//! offsets and the bytes of strings or their views, or the bits of
//! booleans from their bit offset. Its [`Bitmap`] of `NULL` values, where it has one, starts
//! at its own bit offset. The slots a call asks for are a mask of 64-bit
//! words, bit `i % 64` of word `i / 64` for slot `i`.

/// A bitmap of Arrow's form: a bit a slot, the lowest bit of each byte
/// first, from `offset` bits into `bytes`.
///
/// As the bitmap of a column's validity, a set bit means that the slot
/// holds a value and a clear bit that it is `NULL`. As the values of a
/// `Boolean` column, a set bit is true.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bitmap<'a> {
    /// The bytes of the bitmap.
    pub bytes: &'a [u8],
    /// The bit of `bytes` at which the column's first slot is.
    pub offset: usize,
}

impl<'a> Bitmap<'a> {
    /// The bitmap of `bytes` from bit `offset`.
    pub const fn new(bytes: &'a [u8], offset: usize) -> Self {
        Bitmap { bytes, offset }
    }

    /// Whether the bitmap holds the bits of `len` slots.
    pub(crate) fn covers(&self, len: usize) -> bool {
        self.offset
            .checked_add(len)
            .is_some_and(|end| end <= self.bytes.len().saturating_mul(8))
    }

    /// The bit of slot `slot`; the bitmap must cover it.
    #[inline]
    pub(crate) fn get(&self, slot: usize) -> bool {
        let bit = self.offset + slot;
        self.bytes[bit / 8] >> (bit % 8) & 1 == 1
    }

    /// The bits of the 64 slots from `first`, as a word, the bits of the
    /// slots past `len` cleared; the bitmap must cover `len` slots.
    #[inline]
    pub(crate) fn word(&self, first: usize, len: usize) -> u64 {
        let count = (len - first).min(64);
        let bit = self.offset + first;
        let byte = bit / 8;
        let shift = bit % 8;
        // The bytes that hold the bits, at most nine, read as one wide
        // integer, the lowest byte first.
        let last = (bit + count).div_ceil(8);
        let mut wide: u128 = 0;
        for (i, &value) in self.bytes[byte..last].iter().enumerate() {
            wide |= u128::from(value) << (8 * i);
        }
        let word = (wide >> shift) as u64;
        if count == 64 {
            word
        } else {
            word & ((1 << count) - 1)
        }
    }
}

/// The values of a plain column, as Arrow's buffers hold them, each
/// slice from the column's first slot.
///
/// A `Date32` column gives its days as [`Values::Int32`], and a
/// `Timestamp` column its counts as [`Values::Int64`]. Strings and bytes
/// give `len + 1` offsets into their bytes: slot `i`'s bytes are
/// `data[offsets[i]..offsets[i + 1]]`; or, for `Utf8View` and
/// `BinaryView`, a view a slot ([`Values::Views`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Values<'a> {
    /// `Boolean`: a bit a value.
    Boolean(Bitmap<'a>),
    /// `Int8`.
    Int8(&'a [i8]),
    /// `Int16`.
    Int16(&'a [i16]),
    /// `Int32` and `Date32`.
    Int32(&'a [i32]),
    /// `Int64` and `Timestamp`.
    Int64(&'a [i64]),
    /// `UInt8`.
    UInt8(&'a [u8]),
    /// `UInt16`.
    UInt16(&'a [u16]),
    /// `UInt32`.
    UInt32(&'a [u32]),
    /// `Float32`.
    Float32(&'a [f32]),
    /// `Float64`.
    Float64(&'a [f64]),
    /// `Utf8` and `Binary`: 32-bit offsets and the bytes.
    Bytes {
        /// The offsets, `len + 1` of them.
        offsets: &'a [i32],
        /// The bytes.
        data: &'a [u8],
    },
    /// `LargeUtf8` and `LargeBinary`: 64-bit offsets and the bytes.
    LargeBytes {
        /// The offsets, `len + 1` of them.
        offsets: &'a [i64],
        /// The bytes.
        data: &'a [u8],
    },
    /// `Utf8View` and `BinaryView`: a view of 16 bytes a slot, and the
    /// buffers a long value's view points into.
    ///
    /// A view is laid out as Arrow's format lays it out, each of its
    /// integers of 4 bytes little-endian. A value of up to 12 bytes lies
    /// in its view; a longer one in a buffer, from an offset:
    ///
    /// ```text
    ///  0          4                                  16
    ///  ┌──────────┬───────────────────────────────────┐
    ///  │ length   │ the value, up to 12 bytes         │  length <= 12
    ///  └──────────┴───────────────────────────────────┘
    ///  ┌──────────┬───────────┬───────────┬───────────┐
    ///  │ length   │ prefix    │ buffer    │ offset    │  length > 12
    ///  └──────────┴───────────┴───────────┴───────────┘
    ///  0          4           8           12          16
    /// ```
    ///
    /// A long value's bytes are `buffers[buffer][offset..offset +
    /// length]`. The prefix, the value's first 4 bytes, is not read, nor
    /// are the bytes of a short view past its value. A slot that is
    /// `NULL` may hold any view.
    Views {
        /// The views, 16 bytes a slot, `16 * len` of them at least.
        views: &'a [u8],
        /// The buffers, by their index in a view.
        buffers: &'a [&'a [u8]],
    },
    /// `FixedSizeBinary`: `size` bytes a slot.
    FixedBytes {
        /// The bytes of a slot.
        size: usize,
        /// The bytes, `size * len` of them at least.
        data: &'a [u8],
    },
}

/// A plain column: its count of slots, its values, and its bitmap of
/// `NULL` values, `None` where no slot is `NULL`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Column<'a> {
    /// The count of slots.
    pub len: usize,
    /// The values.
    pub values: Values<'a>,
    /// The bitmap of `NULL` values: a set bit where the slot holds a
    /// value. `None` where no slot is `NULL`.
    pub nulls: Option<Bitmap<'a>>,
}

/// The indexes of a dictionary column, each slice from the column's
/// first slot: an index a slot into the dictionary's values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keys<'a> {
    /// `Int8` indexes.
    Int8(&'a [i8]),
    /// `Int16` indexes.
    Int16(&'a [i16]),
    /// `Int32` indexes.
    Int32(&'a [i32]),
    /// `Int64` indexes.
    Int64(&'a [i64]),
    /// `UInt8` indexes.
    UInt8(&'a [u8]),
    /// `UInt16` indexes.
    UInt16(&'a [u16]),
    /// `UInt32` indexes.
    UInt32(&'a [u32]),
    /// `UInt64` indexes.
    UInt64(&'a [u64]),
}

impl Keys<'_> {
    pub(crate) fn len(&self) -> usize {
        match self {
            Keys::Int8(k) => k.len(),
            Keys::Int16(k) => k.len(),
            Keys::Int32(k) => k.len(),
            Keys::Int64(k) => k.len(),
            Keys::UInt8(k) => k.len(),
            Keys::UInt16(k) => k.len(),
            Keys::UInt32(k) => k.len(),
            Keys::UInt64(k) => k.len(),
        }
    }

    /// The index of slot `slot` as a position in the dictionary, `None`
    /// for a negative index or one no `usize` holds.
    #[inline]
    pub(crate) fn get(&self, slot: usize) -> Option<usize> {
        match self {
            Keys::Int8(k) => usize::try_from(k[slot]).ok(),
            Keys::Int16(k) => usize::try_from(k[slot]).ok(),
            Keys::Int32(k) => usize::try_from(k[slot]).ok(),
            Keys::Int64(k) => usize::try_from(k[slot]).ok(),
            Keys::UInt8(k) => Some(usize::from(k[slot])),
            Keys::UInt16(k) => Some(usize::from(k[slot])),
            Keys::UInt32(k) => usize::try_from(k[slot]).ok(),
            Keys::UInt64(k) => usize::try_from(k[slot]).ok(),
        }
    }
}

/// A column a conversion reads: a plain column, or a dictionary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Input<'a> {
    /// A plain column.
    Plain(Column<'a>),
    /// A dictionary column: an index a slot into a plain column of the
    /// dictionary's values. A slot is `NULL` where its own bitmap says
    /// so, and where the value it refers to is `NULL` by the values'
    /// bitmap.
    Dictionary {
        /// The count of slots.
        len: usize,
        /// The indexes, `len` of them at least.
        keys: Keys<'a>,
        /// The bitmap of `NULL` slots, `None` where no slot is `NULL`.
        nulls: Option<Bitmap<'a>>,
        /// The dictionary's values.
        values: Column<'a>,
    },
}

impl Input<'_> {
    /// The count of slots.
    pub const fn len(&self) -> usize {
        match self {
            Input::Plain(column) => column.len,
            Input::Dictionary { len, .. } => *len,
        }
    }

    /// Whether the column has no slot.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The count of 64-bit words a mask of `len` slots takes.
pub const fn mask_words(len: usize) -> usize {
    len.div_ceil(64)
}

/// Whether `mask` covers `len` slots and asks for none past them.
pub(crate) fn mask_fits(mask: &[u64], len: usize) -> bool {
    let words = mask_words(len);
    if mask.len() < words {
        return false;
    }
    let tail = len % 64;
    let last_ok = tail == 0 || words == 0 || mask[words - 1] >> tail == 0;
    last_ok && mask[words..].iter().all(|&w| w == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_of_a_bitmap_from_any_offset() {
        let bytes: Vec<u8> = (0..20u8).map(|i| i.wrapping_mul(37) ^ 0x5a).collect();
        for offset in 0..9 {
            let bitmap = Bitmap::new(&bytes, offset);
            let len = bytes.len() * 8 - offset;
            assert!(bitmap.covers(len) && !bitmap.covers(len + 1));
            for first in (0..len).step_by(64) {
                let word = bitmap.word(first, len);
                for i in 0..64 {
                    let expected = first + i < len && bitmap.get(first + i);
                    assert_eq!(
                        word >> i & 1 == 1,
                        expected,
                        "offset {offset} slot {}",
                        first + i
                    );
                }
            }
        }
        assert!(!Bitmap::new(&[], usize::MAX).covers(1));
    }

    #[test]
    fn a_mask_fits_its_column() {
        assert!(mask_fits(&[], 0));
        assert!(mask_fits(&[u64::MAX], 64));
        assert!(mask_fits(&[u64::MAX, 1], 65));
        assert!(!mask_fits(&[u64::MAX, 2], 65));
        assert!(!mask_fits(&[u64::MAX], 65));
        assert!(mask_fits(&[0b111, 0], 3));
        assert!(!mask_fits(&[0b1111], 3));
        assert!(!mask_fits(&[0, 1], 3));
        assert_eq!(mask_words(0), 0);
        assert_eq!(mask_words(64), 1);
        assert_eq!(mask_words(65), 2);
    }

    #[test]
    fn indexes_of_every_type() {
        assert_eq!(Keys::Int8(&[-1, 3]).get(0), None);
        assert_eq!(Keys::Int8(&[-1, 3]).get(1), Some(3));
        assert_eq!(
            Keys::UInt64(&[u64::MAX]).get(0),
            usize::try_from(u64::MAX).ok()
        );
        assert_eq!(Keys::UInt16(&[7]).len(), 1);
        let one = Input::Plain(Column {
            len: 1,
            values: Values::Int8(&[1]),
            nulls: None,
        });
        assert!(!one.is_empty() && one.len() == 1);
    }
}

//! Keys of a batch as the table compares and stores them.
//!
//! Whatever a key column's representation, the table works with one form:
//! per 64-row word, one `i64` slot per row and key (an int4 sign-extended,
//! a NULL as 0) and the bits of the rows that are NULL in each key. A
//! [`KeySource`] fills the slots of one key of one word; [`normalize_word`]
//! does it for any [`ColumnReader`] of int4 or int8 values ([`KeyValue`]), and a
//! slice of such columns is a source of as many keys. Mixed kinds (an int4
//! key next to an int8 one) are a type on the caller's side, an enum of
//! columns in a wrapper, whose `word` dispatches to `normalize_word`. The
//! comparison loops of the table then run over `i64` alone, whatever the
//! columns were.

use core::mem::MaybeUninit;

use anyhow::Result;
use tessera_core::{ColumnReader, WordBlock};

use super::header::MAX_KEYS;
use super::record::View;

/// The keys of a batch, one word at a time.
pub trait KeySource {
    /// How many keys a row has.
    fn nkeys(&self) -> usize;

    /// How many physical rows the batch has.
    fn nrows(&self) -> usize;

    /// Copy key `key` of the rows in `selected` of word `index` into
    /// `out`, indexed by row within the word, a NULL as 0, and return the
    /// bits of the selected rows that are not NULL. Other slots of `out`
    /// keep their contents.
    fn word(&self, key: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Result<u64>;
}

/// A key value the table stores: an integer that widens into a slot and
/// that a PostgreSQL Datum word encodes.
pub trait KeyValue: Copy {
    /// The value a Datum word holds, as its column's representation reads
    /// it ([`tessera_core::WordBlock::Datum`]).
    fn from_datum(word: u64) -> Self;

    /// The value in a key slot.
    fn widen(self) -> i64;
}

impl KeyValue for i32 {
    /// An int4 Datum holds the value in its low 32 bits.
    #[inline(always)]
    fn from_datum(word: u64) -> Self {
        word as i32
    }

    #[inline(always)]
    fn widen(self) -> i64 {
        i64::from(self)
    }
}

impl KeyValue for i64 {
    /// An int8 Datum is the whole word.
    #[inline(always)]
    fn from_datum(word: u64) -> Self {
        word as i64
    }

    #[inline(always)]
    fn widen(self) -> i64 {
        self
    }
}

/// Fill the slots of one word from a column, as [`KeySource::word`] asks.
///
/// A full, prepared word whose storage the column exposes
/// ([`ColumnReader::word_block`]) is read straight from that storage for
/// the selected rows only, a NULL as 0, without a call per row; other
/// words go through the column's row iterator. Only selected rows are
/// visited either way, so a sparse selection costs its rows, not the word.
#[inline]
pub fn normalize_word<C, V>(
    column: &C,
    index: usize,
    selected: u64,
    out: &mut [i64; 64],
) -> Result<u64>
where
    C: ColumnReader<Value = V>,
    V: KeyValue,
{
    match column.word_block(index) {
        Some(WordBlock::Dense { values, non_nulls }) => {
            let mut bits = selected;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let present = non_nulls >> bit & 1 != 0;
                out[bit] = if present { values[bit].widen() } else { 0 };
            }
            Ok(non_nulls & selected)
        }
        Some(WordBlock::Datum { values, isnull }) => {
            let mut non_nulls = 0;
            let mut bits = selected;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let null = isnull[bit];
                out[bit] = if null {
                    0
                } else {
                    V::from_datum(values[bit]).widen()
                };
                non_nulls |= u64::from(!null) << bit;
            }
            Ok(non_nulls)
        }
        None => {
            let mut non_nulls = 0;
            for (row, value) in column.word_values(index, selected)? {
                let bit = row % 64;
                out[bit] = value.map_or(0, |value| {
                    non_nulls |= 1 << bit;
                    value.widen()
                });
            }
            Ok(non_nulls)
        }
    }
}

impl<C, V> KeySource for [C]
where
    C: ColumnReader<Value = V>,
    V: KeyValue,
{
    fn nkeys(&self) -> usize {
        self.len()
    }

    fn nrows(&self) -> usize {
        self.first().map_or(0, ColumnReader::nrows)
    }

    fn word(&self, key: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Result<u64> {
        normalize_word(&self[key], index, selected, out)
    }
}

/// The keys of one word in the table's form.
///
/// Only the slots of the table's keys are initialized, when the buffer is
/// made: zeroing all [`MAX_KEYS`] of them, 8 KiB, cost more than a short
/// batch's rows.
pub(super) struct WordKeys {
    slots: [MaybeUninit<[i64; 64]>; MAX_KEYS],
    nkeys: usize,
    null_bits: [u32; 64],
}

impl WordKeys {
    /// A buffer for `nkeys` keys, at most [`MAX_KEYS`] (the header checked).
    #[inline]
    pub(super) fn new(nkeys: usize) -> Self {
        let nkeys = nkeys.min(MAX_KEYS);
        let mut slots = [const { MaybeUninit::uninit() }; MAX_KEYS];
        for slot in &mut slots[..nkeys] {
            slot.write([0; 64]);
        }
        Self {
            slots,
            nkeys,
            null_bits: [0; 64],
        }
    }

    /// The initialized slots, one array per key.
    #[inline(always)]
    fn keys(&self) -> &[[i64; 64]] {
        let slots = &self.slots[..self.nkeys];
        // SAFETY: `new` wrote the first `nkeys` arrays, and nothing makes
        // them uninitialized again; `MaybeUninit<T>` has `T`'s layout.
        unsafe { &*(core::ptr::from_ref(slots) as *const [[i64; 64]]) }
    }

    /// Load the keys of the selected rows of one word.
    #[inline]
    pub(super) fn load<K: KeySource + ?Sized>(
        &mut self,
        keys: &K,
        index: usize,
        selected: u64,
    ) -> Result<()> {
        self.null_bits = [0; 64];
        for (key, slot) in self.slots[..self.nkeys].iter_mut().enumerate() {
            // SAFETY: as in `keys`: the first `nkeys` arrays are written.
            let slots = unsafe { slot.assume_init_mut() };
            let mut nulls = selected & !keys.word(key, index, selected, slots)?;
            while nulls != 0 {
                self.null_bits[nulls.trailing_zeros() as usize] |= 1 << key;
                nulls &= nulls - 1;
            }
        }
        Ok(())
    }

    /// The bits of the keys of row `bit` that are NULL.
    #[inline]
    pub(super) fn null_bits(&self, bit: usize) -> u32 {
        self.null_bits[bit & 63]
    }

    /// Slot `key` of row `bit`.
    #[inline(always)]
    pub(super) fn key(&self, key: usize, bit: usize) -> i64 {
        self.keys()[key][bit & 63]
    }

    /// Whether row `bit` has the null bits and keys of a record.
    #[inline]
    pub(super) fn equal(&self, bit: usize, record: &View<'_>) -> bool {
        record.null_bits() == self.null_bits[bit & 63]
            && record
                .keys()
                .iter()
                .zip(self.keys())
                .all(|(&key, slots)| key == slots[bit & 63])
    }
}

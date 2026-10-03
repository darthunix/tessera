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
        // The walk of `ones` written out in the loops of these keys, which
        // the table's loops inline: through the iterator they took an
        // instruction more; `.cargo/mutants.toml` leaves their step out.
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

/// Storage for the slots of a word's keys: made uninitialized on the
/// caller's stack, where it stays, and lent to [`WordKeys`].
///
/// `L` arrays of 64 slots: the key count of a specialized shape, or
/// [`MAX_KEYS`](super::MAX_KEYS) for any count. A shape's own length keeps the loop's
/// stack frame small; a frame of 8 KiB is probed page by page on every
/// call, which a call of no rows pays in full.
pub(super) type SlotBuffer<const L: usize> = [MaybeUninit<[i64; 64]>; L];

/// An uninitialized slot buffer; making it costs nothing.
#[inline(always)]
pub(super) fn slot_buffer<const L: usize>() -> SlotBuffer<L> {
    [const { MaybeUninit::uninit() }; L]
}

/// The keys of one word in the table's form.
///
/// The slots live in a [`SlotBuffer`] the caller keeps in place, and only
/// the table's keys are initialized: a buffer owned by value was copied
/// whole, 8 KiB, whenever it moved, and zeroing all [`MAX_KEYS`](super::MAX_KEYS) of them
/// cost more than a short batch's rows.
pub(super) struct WordKeys<'a> {
    slots: &'a mut [[i64; 64]],
    null_bits: [u32; 64],
}

impl<'a> WordKeys<'a> {
    /// Keys of `nkeys` columns in `buffer`, which holds at least as many
    /// arrays (the header checked `nkeys` against [`MAX_KEYS`](super::MAX_KEYS), and a
    /// specialized shape's buffer has its key count).
    #[inline(always)]
    pub(super) fn new(buffer: &'a mut [MaybeUninit<[i64; 64]>], nkeys: usize) -> Self {
        let slots = &mut buffer[..nkeys];
        for slot in slots.iter_mut() {
            slot.write([0; 64]);
        }
        // SAFETY: every array of `slots` was just written, and
        // `MaybeUninit<T>` has `T`'s layout.
        let slots = unsafe { &mut *(core::ptr::from_mut(slots) as *mut [[i64; 64]]) };
        Self {
            slots,
            null_bits: [0; 64],
        }
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
        for (key, slots) in self.slots.iter_mut().enumerate() {
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

    /// Slot `key` of row `bit`, with `key` below the buffer's key count.
    ///
    /// # Safety
    ///
    /// `key` is below the `nkeys` the buffer was made for.
    #[inline(always)]
    pub(super) unsafe fn key(&self, key: usize, bit: usize) -> i64 {
        debug_assert!(key < self.slots.len());
        // SAFETY: `key < nkeys`, the slots' length, by the caller's contract.
        unsafe { self.slots.get_unchecked(key)[bit & 63] }
    }

    /// Whether row `bit` has the null bits and keys of a record; `N` is
    /// the key count when the caller knows it, 0 for this buffer's.
    ///
    /// The keys are read unchecked: the record's key count is known only
    /// at run time, so indexing keeps a bounds check per key even when `N`
    /// is a constant (at `N = 1`, three more panic paths in each
    /// `probe_rows` and `resolve_rows` and 53 more instructions in
    /// `probe_rows` with one codegen unit; a slice of `nkeys` keys zipped
    /// with the slots, 52 more).
    ///
    /// # Safety
    ///
    /// The record belongs to a table of this buffer's key count, and `N`
    /// is 0 or that count.
    #[inline(always)]
    pub(super) unsafe fn equal<const N: usize>(&self, bit: usize, record: &View<'_>) -> bool {
        let nkeys = if N > 0 { N } else { self.slots.len() };
        let keys = record.keys();
        debug_assert!(keys.len() == self.slots.len() && nkeys == self.slots.len());
        record.null_bits() == self.null_bits[bit & 63]
            && (0..nkeys).all(|key| {
                // SAFETY: `key < nkeys`, the record's key count and this
                // buffer's, by the caller's contract.
                unsafe { *keys.get_unchecked(key) == self.key(key, bit) }
            })
    }
}

//! Keys of a batch as the table compares and stores them.
//!
//! Whatever a key column's representation, the table works with one form:
//! per 64-row word, one `i64` slot per row and key (the value as the
//! caller puts it in a word, a NULL as 0) and the bits of the rows that
//! are NULL in each key. A
//! [`KeySource`] fills the slots of one key of one word. The comparison
//! loops of the table then run over `i64` alone, whatever the columns
//! were.

use core::mem::MaybeUninit;

use super::record::View;
use crate::error::{Error, Result, Rule};

/// The keys of a batch, one word at a time.
pub trait KeySource {
    /// How many keys a row has.
    fn nkeys(&self) -> usize;

    /// How many physical rows the batch has.
    fn nrows(&self) -> usize;

    /// Copy key `key` of the rows in `selected` of word `index` into
    /// `out`, indexed by row within the word, and return the bits of the
    /// selected rows that are not NULL. The slot of a NULL may hold
    /// anything: the table stores 0 for it. Other slots of `out` keep their
    /// contents. `None` when the source cannot give the key of a selected
    /// row: the call then fails with
    /// [`Rule::KeyRead`](crate::Rule::KeyRead), and the words of the batch
    /// before stay placed, as [`Batch`](crate::Batch) says.
    fn word(&self, key: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Option<u64>;
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
/// This is not a second buffer: the slots live in the [`SlotBuffer`] the
/// caller keeps in place, and this type borrows the arrays of the table's
/// keys from it, once they are written. The buffer is the memory, made
/// uninitialized and never moved; this is the view the comparisons read.
/// A buffer owned by value was copied whole, 8 KiB, whenever it moved,
/// and zeroing all [`MAX_KEYS`](super::MAX_KEYS) of them cost more than a
/// short batch's rows.
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
            let present = keys
                .word(key, index, selected, slots)
                .ok_or_else(|| Error::wrong_call(Rule::KeyRead).with(index).and(key))?;
            let mut nulls = selected & !present;
            // The walk of `ones` written out, as the row loops that inline
            // this one write theirs; `.cargo/mutants.toml` leaves the step
            // out.
            while nulls != 0 {
                let bit = nulls.trailing_zeros() as usize;
                self.null_bits[bit] |= 1 << key;
                // A NULL key is 0 in its slot, whatever the source wrote.
                slots[bit] = 0;
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

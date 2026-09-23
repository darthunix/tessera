//! Keys of a batch as the table compares and stores them.
//!
//! Whatever a key column's representation, the table works with one form:
//! per 64-row word, one `i64` slot per row and key (an int4 sign-extended,
//! a NULL as 0) and the bits of the rows that are NULL in each key. A
//! [`KeySource`] fills the slots of one key of one word; [`normalize_word`]
//! does it for any [`ColumnReader`] whose values widen to `i64`, and a
//! slice of such columns is a source of as many keys. Mixed kinds (an int4
//! key next to an int8 one) are a type on the caller's side, an enum of
//! columns in a wrapper, whose `word` dispatches to `normalize_word`. The
//! comparison loops of the table then run over `i64` alone, whatever the
//! columns were.

use anyhow::Result;
use tessera_core::ColumnReader;

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

/// Fill the slots of one word from a column, as [`KeySource::word`] asks.
pub fn normalize_word<C, V>(
    column: &C,
    index: usize,
    selected: u64,
    out: &mut [i64; 64],
) -> Result<u64>
where
    C: ColumnReader<Value = V>,
    V: Into<i64>,
{
    let mut non_nulls = 0;
    for (row, value) in column.word_values(index, selected)? {
        let bit = row % 64;
        out[bit] = value.map_or(0, |value| {
            non_nulls |= 1 << bit;
            value.into()
        });
    }
    Ok(non_nulls)
}

impl<C, V> KeySource for [C]
where
    C: ColumnReader<Value = V>,
    V: Into<i64>,
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
pub(super) struct WordKeys {
    slots: [[i64; 64]; MAX_KEYS],
    null_bits: [u32; 64],
}

impl WordKeys {
    pub(super) fn new() -> Self {
        Self {
            slots: [[0; 64]; MAX_KEYS],
            null_bits: [0; 64],
        }
    }

    /// Load the `nkeys` keys of the selected rows of one word.
    pub(super) fn load<K: KeySource + ?Sized>(
        &mut self,
        keys: &K,
        nkeys: usize,
        index: usize,
        selected: u64,
    ) -> Result<()> {
        self.null_bits = [0; 64];
        for (key, slots) in self.slots[..nkeys].iter_mut().enumerate() {
            let mut nulls = selected & !keys.word(key, index, selected, slots)?;
            while nulls != 0 {
                self.null_bits[nulls.trailing_zeros() as usize] |= 1 << key;
                nulls &= nulls - 1;
            }
        }
        Ok(())
    }

    /// The bits of the keys of row `bit` that are NULL.
    pub(super) fn null_bits(&self, bit: usize) -> u32 {
        self.null_bits[bit]
    }

    /// The `nkeys` slots of row `bit`, in key order.
    pub(super) fn keys(&self, bit: usize, nkeys: usize) -> impl Iterator<Item = i64> + '_ {
        self.slots[..nkeys].iter().map(move |slots| slots[bit])
    }

    /// Whether row `bit` has the null bits and keys of a record.
    pub(super) fn equal(&self, bit: usize, record: &View<'_>) -> bool {
        record.null_bits() == self.null_bits[bit]
            && record
                .keys()
                .iter()
                .zip(&self.slots)
                .all(|(&key, slots)| key == slots[bit])
    }
}

//! Keys of a batch read from Tessera's columns, in the form the table
//! compares and stores them (see [`KeySource`]): [`normalize_word`]
//! fills the slots of one key of one word for any [`ColumnReader`] of int4
//! or int8 values ([`KeyValue`]), and [`KeyColumns`] of such columns is a
//! source of as many keys. Mixed kinds (an int4 key next to an int8 one)
//! are a type on the caller's side, an enum of columns in a wrapper, whose
//! `word` dispatches to `normalize_word`.

use tessera_core::{ColumnReader, WordBlock};

use crate::table::KeySource;

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
/// `None` when the column cannot give a selected row, one it has not
/// prepared.
#[inline]
pub fn normalize_word<C, V>(
    column: &C,
    index: usize,
    selected: u64,
    out: &mut [i64; 64],
) -> Option<u64>
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
            Some(non_nulls & selected)
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
            Some(non_nulls)
        }
        None => {
            let mut non_nulls = 0;
            for (row, value) in column.word_values(index, selected).ok()? {
                let bit = row % 64;
                out[bit] = value.map_or(0, |value| {
                    non_nulls |= 1 << bit;
                    value.widen()
                });
            }
            Some(non_nulls)
        }
    }
}

/// Columns of int4 or int8 values ([`KeyValue`]) as a source of as many
/// keys, one a column.
pub struct KeyColumns<'a, C>(pub &'a [C]);

impl<C, V> KeySource for KeyColumns<'_, C>
where
    C: ColumnReader<Value = V>,
    V: KeyValue,
{
    fn nkeys(&self) -> usize {
        self.0.len()
    }

    fn nrows(&self) -> usize {
        self.0.first().map_or(0, ColumnReader::nrows)
    }

    fn word(&self, key: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Option<u64> {
        normalize_word(&self.0[key], index, selected, out)
    }
}

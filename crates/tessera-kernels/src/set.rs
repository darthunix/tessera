//! Membership of integer values in a set of constants: `x IN (…)` and
//! `x NOT IN (…)` over integer words (int2, int4, int8, date, timestamp,
//! boolean), each value read as an int4 or an int8 ([`SetValue`]) and the
//! constants given in its width.
//!
//! A [`KeySet`] holds the constants sorted without repeats, as the caller
//! prepares them once a plan. A full word of many selected rows is
//! compared with vector code, every lane against every key, while the keys
//! are few enough for that to pay ([`SetValue::WORD_KEYS`]). Every other
//! word is read at its selected rows: a few keys compared all, by an or of
//! equalities without a branch a key; more searched by halving with the
//! comparison picking the half as data, not as a jump, so that a row costs
//! the same whatever its value and the branch predictor has nothing to
//! miss. The batch function ([`in_set`]) runs over the selected rows of a
//! column.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView, WordBlock};

/// At most this many keys are compared all at a row; more are searched.
const LINEAR_KEYS: usize = 16;

/// Selected rows of a word from which it is compared whole, as the
/// filters decide (`int32::BULK_MIN_ROWS`).
const BULK_MIN_ROWS: u32 = 12;

/// Constants sorted in increasing order, without repeats.
#[derive(Clone, Copy, Debug)]
pub struct KeySet<'a, T> {
    keys: &'a [T],
}

impl<'a, T: SetValue> KeySet<'a, T> {
    /// The set of `keys`, which must be sorted in strictly increasing
    /// order: another order makes membership answers wrong, not unsafe, and
    /// is checked only in debug builds, since a long list checked every
    /// batch would cost more than the batch.
    pub fn new(keys: &'a [T]) -> Self {
        debug_assert!(
            keys.windows(2).all(|pair| pair[0] < pair[1]),
            "the keys of a set are not sorted without repeats"
        );
        Self { keys }
    }

    /// Whether the set holds `value`.
    #[inline(always)]
    pub fn contains(&self, value: T) -> bool {
        let keys = self.keys;
        if keys.len() <= LINEAR_KEYS {
            return keys
                .iter()
                .fold(false, |found, &key| found | (key == value));
        }
        // The last key at most value: each step keeps the half that holds it.
        let mut base = 0;
        let mut size = keys.len();
        while size > 1 {
            let half = size / 2;
            let middle = base + half;
            base = if keys[middle] <= value { middle } else { base };
            size -= half;
        }
        keys[base] == value
    }
}

/// A value a set's column holds, an int4 or an int8, compared with keys of
/// its own width.
pub trait SetValue: Copy + Ord {
    /// At most this many keys are compared all in a whole word: from here
    /// on halving costs a row less. Four int4 lanes go to a vector against
    /// two int8 lanes, so the int4 bound is higher (measured on an M5 Pro:
    /// 60 int4 keys compared all 0.93 of halving, 24 int8 keys 1.01, 60
    /// int8 keys 1.21).
    const WORD_KEYS: usize;

    /// The rows of a whole word whose value is one of `keys`, NULL rows
    /// compared like any other, and the word's non-NULL rows; `None`
    /// where no vector code is built.
    fn block_hits(block: WordBlock<'_, Self>, keys: &[Self]) -> Option<(u64, u64)>;
}

impl SetValue for i32 {
    const WORD_KEYS: usize = 64;

    #[inline(always)]
    fn block_hits(block: WordBlock<'_, Self>, keys: &[Self]) -> Option<(u64, u64)> {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        {
            use crate::simd;

            Some(match block {
                WordBlock::Dense { values, non_nulls } => {
                    (simd::set_dense(values, keys), non_nulls)
                }
                WordBlock::Datum { values, isnull } => {
                    (simd::set_datum(values, keys), simd::non_null_bits(isnull))
                }
            })
        }
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (block, keys);
            None
        }
    }
}

impl SetValue for i64 {
    const WORD_KEYS: usize = 16;

    #[inline(always)]
    fn block_hits(block: WordBlock<'_, Self>, keys: &[Self]) -> Option<(u64, u64)> {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        {
            use crate::simd;

            Some(match block {
                WordBlock::Dense { values, non_nulls } => {
                    (simd::set_dense64(values, keys), non_nulls)
                }
                WordBlock::Datum { values, isnull } => {
                    (simd::set_datum64(values, keys), simd::non_null_bits(isnull))
                }
            })
        }
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (block, keys);
            None
        }
    }
}

/// Membership of the selected rows' values: `found` gets the rows whose
/// value the set holds, `present` the rows whose value is not NULL; every
/// word of both is written. The caller makes IN and NOT IN, and their
/// NULLs, of the two.
///
/// While the keys are few enough, a full word of at least a dozen
/// selected rows whose storage the reader exposes is compared whole; every
/// other word is read at its selected rows.
///
/// # Errors
///
/// Different row counts fail before any mutation; a reader error (an
/// unprepared selected row) leaves the current word and the later ones
/// unwritten.
pub fn in_set<T, C>(
    set: &KeySet<'_, T>,
    column: &C,
    rows: RowMaskView<'_>,
    found: &mut RowMask<'_>,
    present: &mut RowMask<'_>,
) -> Result<()>
where
    T: SetValue,
    C: ColumnReader<Value = T>,
{
    let nrows = rows.nrows();
    ensure!(
        column.nrows() == nrows
            && found.as_view().nrows() == nrows
            && present.as_view().nrows() == nrows,
        "the column and masks of a set call have different row counts"
    );
    let whole_words = set.keys.len() <= T::WORD_KEYS;
    for word in 0..nrows.div_ceil(64) {
        let selected = rows.word(word).unwrap();
        let whole = if whole_words
            && (selected == u64::MAX || selected.count_ones() >= BULK_MIN_ROWS)
            && let Some(block) = column.word_block(word)
        {
            T::block_hits(block, set.keys)
        } else {
            None
        };
        let (hits, values) = match whole {
            Some((hits, non_nulls)) => {
                let values = selected & non_nulls;
                (hits & values, values)
            }
            None if selected == 0 => (0, 0),
            None => column.word_values(word, selected)?.fold(
                (0_u64, 0_u64),
                |(hits, values), (row, value)| match value {
                    Some(value) => (
                        hits | (u64::from(set.contains(value)) << (row % 64)),
                        values | (1 << (row % 64)),
                    ),
                    None => (hits, values),
                },
            ),
        };
        found.set_word(word, hits)?;
        present.set_word(word, values)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tessera_core::ColumnView;

    use super::*;

    /// xorshift64*, fixed seed.
    fn random(state: &mut u64) -> u64 {
        *state ^= *state >> 12;
        *state ^= *state << 25;
        *state ^= *state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    #[test]
    fn membership_as_a_linear_scan() {
        let mut state = 0x05E7_0F12_3456_78AB;
        for count in [0, 1, 2, 7, 16, 17, 33, 64, 100, 1000] {
            let mut keys: Vec<i64> = (0..count)
                .map(|_| (random(&mut state) % 3000) as i64 - 1500)
                .collect();
            keys.sort_unstable();
            keys.dedup();
            keys.extend([i64::MIN, i64::MAX].iter().filter(|_| count > 50));
            keys.sort_unstable();
            keys.dedup();
            let set = KeySet::new(&keys);
            for value in (-1600..1600).chain([i64::MIN, i64::MAX, i64::MIN + 1]) {
                assert_eq!(
                    set.contains(value),
                    keys.contains(&value),
                    "{count} {value}"
                );
            }
        }
    }

    /// Values of a column of either storage, NULLs among them.
    fn values(nrows: usize) -> Vec<Option<i64>> {
        (0..nrows)
            .map(|row| (row % 7 != 0).then_some(row as i64 * 13 % 700))
            .collect()
    }

    /// A value in a Datum word: an int4 in the low half, an int8 whole.
    trait FromWord: SetValue + std::fmt::Debug {
        fn from_word(word: u64) -> Self;
    }

    impl FromWord for i32 {
        fn from_word(word: u64) -> Self {
            word as i32
        }
    }

    impl FromWord for i64 {
        fn from_word(word: u64) -> Self {
            word as i64
        }
    }

    /// Values in Datum storage, whose high half beside an int4 holds
    /// garbage as a Datum may.
    struct Datums<T> {
        values: Vec<u64>,
        isnull: Vec<bool>,
        value: std::marker::PhantomData<T>,
    }

    impl<T: FromWord> ColumnReader for Datums<T> {
        type Value = T;
        fn nrows(&self) -> usize {
            self.values.len()
        }
        fn get(&self, row: usize) -> Result<Option<T>> {
            Ok((!self.isnull[row]).then(|| T::from_word(self.values[row])))
        }
        fn word_values(
            &self,
            word_index: usize,
            selected: u64,
        ) -> Result<impl Iterator<Item = (usize, Option<T>)> + '_> {
            Ok((0..64)
                .filter(move |bit| selected >> bit & 1 == 1)
                .map(move |bit| {
                    let row = word_index * 64 + bit;
                    (
                        row,
                        (!self.isnull[row]).then(|| T::from_word(self.values[row])),
                    )
                }))
        }
        fn word_block(&self, word_index: usize) -> Option<WordBlock<'_, T>> {
            let base = word_index * 64;
            (base + 64 <= self.values.len()).then(|| WordBlock::Datum {
                values: self.values[base..base + 64].try_into().unwrap(),
                isnull: self.isnull[base..base + 64].try_into().unwrap(),
            })
        }
    }

    /// Every size of set, below and above both whole-word bounds, over both
    /// storages and a selection whose words are full, dense, sparse and
    /// empty, against a scan of the keys.
    #[test]
    fn a_batch_writes_found_and_present() {
        let nrows = 300;
        let values = values(nrows);
        let dense: Vec<i32> = values
            .iter()
            .map(|value| value.unwrap_or(-5) as i32)
            .collect();
        let wide: Vec<i64> = values.iter().map(|value| value.unwrap_or(9)).collect();
        let mut non_null_words = vec![0_u64; nrows.div_ceil(64)];
        for row in (0..nrows).filter(|&row| values[row].is_some()) {
            non_null_words[row / 64] |= 1 << (row % 64);
        }
        let non_nulls = RowMaskView::try_new(nrows, &non_null_words).unwrap();
        let narrow_column = ColumnView::try_new(&dense, Some(non_nulls)).unwrap();
        let wide_column = ColumnView::try_new(&wide, Some(non_nulls)).unwrap();
        let isnull: Vec<bool> = values.iter().map(Option::is_none).collect();
        let datums = Datums::<i32> {
            values: values
                .iter()
                .enumerate()
                .map(|(row, value)| {
                    u64::from(value.unwrap_or(-5) as i32 as u32) | ((row as u64) << 40)
                })
                .collect(),
            isnull: isnull.clone(),
            value: std::marker::PhantomData,
        };
        let wide_datums = Datums::<i64> {
            values: values
                .iter()
                .map(|value| value.unwrap_or(7) as u64)
                .collect(),
            isnull,
            value: std::marker::PhantomData,
        };
        let mut selected = vec![u64::MAX, 0, 0, 0, 0];
        for row in (64..nrows).filter(|row| row % 3 != 1 && !(192..256).contains(row)) {
            selected[row / 64] |= 1 << (row % 64);
        }
        selected[3] = 1 << 5 | 1 << 40;
        let rows = RowMaskView::try_new(nrows, &selected).unwrap();
        let fibonacci = [
            3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377, 610, 987, 1597, 2584, 4181, 6765,
        ];
        let many: Vec<i64> = (0..70).map(|key| key * 11 - 20).collect();
        let lists: [&[i64]; 7] = [
            &[],
            &fibonacci[..5],
            &fibonacci[..16],
            &fibonacci,
            &many[..64],
            &many,
            &[-1 << 40, 0, 13, 1 << 33],
        ];
        for keys in lists {
            let check = |name: &str, found: &[u64], present: &[u64]| {
                for row in 0..nrows {
                    let chosen = selected[row / 64] >> (row % 64) & 1 == 1;
                    let bit = |words: &[u64]| words[row / 64] >> (row % 64) & 1 == 1;
                    assert_eq!(
                        bit(present),
                        chosen && values[row].is_some(),
                        "{name} {row}"
                    );
                    let hit = values[row].is_some_and(|value| keys.contains(&value));
                    assert_eq!(bit(found), chosen && hit, "{name} {keys:?} {row}");
                }
            };
            let run = |call: &dyn Fn(&mut RowMask<'_>, &mut RowMask<'_>)| {
                // Stale bits of the outputs are overwritten.
                let (mut found, mut present) = (vec![u64::MAX; 5], vec![u64::MAX; 5]);
                (found[4], present[4]) = (0, 0);
                call(
                    &mut RowMask::try_new(nrows, &mut found).unwrap(),
                    &mut RowMask::try_new(nrows, &mut present).unwrap(),
                );
                (found, present)
            };
            // An int4 value equals no key beyond its range: those are dropped.
            let narrow_keys: Vec<i32> = keys
                .iter()
                .filter_map(|&key| i32::try_from(key).ok())
                .collect();
            let narrow = KeySet::new(&narrow_keys);
            let (found, present) = run(&|found, present| {
                in_set(&narrow, &narrow_column, rows, found, present).unwrap()
            });
            check("dense", &found, &present);
            let (found, present) =
                run(&|found, present| in_set(&narrow, &datums, rows, found, present).unwrap());
            check("datum", &found, &present);
            let (found, present) = run(&|found, present| {
                in_set(&KeySet::new(keys), &wide_column, rows, found, present).unwrap()
            });
            check("wide", &found, &present);
            let (found, present) = run(&|found, present| {
                in_set(&KeySet::new(keys), &wide_datums, rows, found, present).unwrap()
            });
            check("wide datum", &found, &present);
        }
    }
}

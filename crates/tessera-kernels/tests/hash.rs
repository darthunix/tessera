#![forbid(unsafe_code)]

use anyhow::{Result, ensure};
use proptest::prelude::*;
use proptest::sample::select;
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView, WordValues};
use tessera_kernels::int32::{NullKeys, hash_combine, murmurhash32};
use tessera_kernels::{int32, int64};
use tessera_testing::{Int, flags, integer, nrows, property, values, words};

/// What an untouched hash slot holds.
const SENTINEL: u32 = 0x5a5a_5a5a;
/// The hash of a NULL key under the group policy: `murmurhash32(0x9e3779b9)`.
const NULL_HASH: u32 = 0x92ca_2f0e;

/// An independent port of PostgreSQL's `murmurhash32`.
fn model_murmur(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h
}

/// An independent port of PostgreSQL's `hash_combine`.
fn model_combine(a: u32, b: u32) -> u32 {
    a ^ (b
        .wrapping_add(0x9e37_79b9)
        .wrapping_add(a << 6)
        .wrapping_add(a >> 2))
}

/// PostgreSQL's `hashint8` fold, written from its source.
fn model_fold(value: i64) -> u32 {
    let mut low = value as u32;
    let high = (value >> 32) as u32;
    low ^= if value >= 0 { high } else { !high };
    low
}

/// The words of a mask still borrowed by its `RowMask`.
fn mask_words(valid: &RowMask<'_>) -> Vec<u64> {
    let view = valid.as_view();
    (0..view.nrows().div_ceil(64))
        .map(|index| view.word(index).unwrap())
        .collect()
}

fn all_rows(nrows: usize) -> Vec<u64> {
    let mut words = vec![u64::MAX; nrows.div_ceil(64)];
    if !nrows.is_multiple_of(64) {
        *words.last_mut().unwrap() = (1 << (nrows % 64)) - 1;
    }
    words
}

/// Values where the `hashint8` fold breaks: around the int4 range and the
/// 32-bit halves, besides the edges of the type.
fn fold_edges() -> BoxedStrategy<i64> {
    select(vec![
        i64::from(u32::MAX),
        1 << 32,
        -(1 << 32),
        1 << 40,
        -(1 << 40),
    ])
    .boxed()
}

/// A key width under test: its kernels, the model of its hash and the
/// values its tests draw. An int4 key hashes as its value read as `u32`, an
/// int8 key as its `hashint8` fold, which agrees with the int4 on the int4
/// range.
trait Key: Int + From<i32> {
    /// What the model hashes for a non-NULL key.
    fn model_key(self) -> u32;
    /// A small value as a key: the int4 as is, the int8 scaled by
    /// `2^33 + 1`, past the int4 range with both halves in play.
    fn spread(value: i32) -> Self;
    /// The keys a property draws: values leaning to the edges, and for int8
    /// also the edges of the fold.
    fn keys() -> BoxedStrategy<Self>;
    /// The width's `hash`.
    fn hash<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
        nulls: NullKeys,
        hashes: &mut [u32],
        valid: &mut RowMask<'_>,
    ) -> Result<()>;
    /// The width's `hash_next`.
    fn hash_next<C: ColumnReader<Value = Self>>(
        column: &C,
        nulls: NullKeys,
        hashes: &mut [u32],
        valid: &mut RowMask<'_>,
    ) -> Result<()>;
}

impl Key for i32 {
    fn model_key(self) -> u32 {
        self as u32
    }

    fn spread(value: i32) -> Self {
        value
    }

    fn keys() -> BoxedStrategy<Self> {
        integer::<i32>()
    }

    fn hash<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
        nulls: NullKeys,
        hashes: &mut [u32],
        valid: &mut RowMask<'_>,
    ) -> Result<()> {
        int32::hash(column, rows, nulls, hashes, valid)
    }

    fn hash_next<C: ColumnReader<Value = Self>>(
        column: &C,
        nulls: NullKeys,
        hashes: &mut [u32],
        valid: &mut RowMask<'_>,
    ) -> Result<()> {
        int32::hash_next(column, nulls, hashes, valid)
    }
}

impl Key for i64 {
    fn model_key(self) -> u32 {
        model_fold(self)
    }

    fn spread(value: i32) -> Self {
        i64::from(value) * ((1 << 33) + 1)
    }

    fn keys() -> BoxedStrategy<Self> {
        prop_oneof![3 => integer::<i64>(), 1 => fold_edges()].boxed()
    }

    fn hash<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
        nulls: NullKeys,
        hashes: &mut [u32],
        valid: &mut RowMask<'_>,
    ) -> Result<()> {
        int64::hash(column, rows, nulls, hashes, valid)
    }

    fn hash_next<C: ColumnReader<Value = Self>>(
        column: &C,
        nulls: NullKeys,
        hashes: &mut [u32],
        valid: &mut RowMask<'_>,
    ) -> Result<()> {
        int64::hash_next(column, nulls, hashes, valid)
    }
}

/// The hashes and valid mask of a chain of keys, per the model: NULL under
/// Reject drops the row for good; under Group it hashes the group key.
fn model<T: Key>(
    keys: &[(&[T], &[bool])],
    selected: &[bool],
    nulls: NullKeys,
) -> (Vec<Option<u32>>, Vec<bool>) {
    let nrows = selected.len();
    let mut hashes = vec![None; nrows];
    let mut valid = selected.to_vec();
    for (index, (values, non_null)) in keys.iter().enumerate() {
        for row in 0..nrows {
            if !valid[row] {
                continue;
            }
            if !non_null[row] && nulls == NullKeys::Reject {
                valid[row] = false;
                hashes[row] = None;
                continue;
            }
            let key = if non_null[row] {
                values[row].model_key()
            } else {
                0x9e37_79b9
            };
            let key_hash = model_murmur(key);
            hashes[row] = Some(if index == 0 {
                key_hash
            } else {
                model_combine(hashes[row].unwrap(), key_hash)
            });
        }
    }
    (hashes, valid)
}

/// The same values without bulk storage: every call takes the row path.
struct RowsOnly<'a, T>(&'a ColumnView<'a, T>);

impl<T: Copy> ColumnReader for RowsOnly<'_, T> {
    type Value = T;
    fn nrows(&self) -> usize {
        self.0.nrows()
    }
    fn get(&self, row: usize) -> Result<Option<T>> {
        ColumnReader::get(self.0, row)
    }
    fn word_values(
        &self,
        word_index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<T>)> + '_> {
        self.0.word_values(word_index, selected)
    }
}

/// A reader that refuses unprepared rows, to show which rows a call reads.
struct Prepared<'a, T> {
    values: &'a [T],
    non_null: &'a [bool],
    prepared: &'a [u64],
}

impl<T: Copy> ColumnReader for Prepared<'_, T> {
    type Value = T;

    fn nrows(&self) -> usize {
        self.values.len()
    }

    fn get(&self, row: usize) -> Result<Option<T>> {
        ensure!(row < self.values.len(), "row is out of bounds");
        ensure!(
            self.prepared[row / 64] & (1 << (row % 64)) != 0,
            "row is not prepared"
        );
        Ok(self.non_null[row].then(|| self.values[row]))
    }

    fn word_values(
        &self,
        index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<T>)> + '_> {
        WordValues::try_new(
            self.values.len(),
            index,
            selected,
            self.prepared[index],
            |row| self.non_null[row].then(|| self.values[row]),
        )
    }
}

#[test]
fn known_values_match_postgresql() {
    // Computed with PostgreSQL's murmurhash32 and hash_combine from
    // src/include/common/hashfn.h.
    for (key, expected) in [
        (0, 0x0000_0000),
        (1, 0x514e_28b7),
        (17, 0xd8d0_9ee8),
        (42, 0x087f_cd5c),
        (550_273, 0x075c_fdf7),
        (207_112_489, 0xde66_492c),
        (-1, 0x81f1_6f39),
        (i32::MIN, 0x6d3c_65a0),
        (i32::MAX, 0xf9cc_0ea8),
        (7, 0x18c9_aec4),
    ] {
        assert_eq!(murmurhash32(key as u32), expected, "{key}");
        assert_eq!(model_murmur(key as u32), expected, "{key}");
    }
    assert_eq!(murmurhash32(0x9e37_79b9), NULL_HASH);
    assert_eq!(hash_combine(0x1234_5678, 0x9abc_def0), 0xd8a3_5a3f);
    assert_eq!(model_combine(0x1234_5678, 0x9abc_def0), 0xd8a3_5a3f);
    assert_eq!(hash_combine(murmurhash32(1), murmurhash32(2)), 0x6647_dc1b);
    assert_eq!(
        hash_combine(
            hash_combine(murmurhash32(1), murmurhash32(2)),
            murmurhash32(3)
        ),
        0xa9f6_f7bd
    );
    assert_eq!(hash_combine(murmurhash32(42), NULL_HASH), 0x5b6b_3e42);
}

#[test]
fn the_fold_is_hashint8s() {
    property(
        proptest::collection::vec(i64::keys(), 0..64),
        |values| -> Result<()> {
            for value in values {
                assert_eq!(int64::fold(value), model_fold(value), "{value}");
            }
            Ok(())
        },
    );
    assert_eq!(int64::fold(1 << 40), 1 << 8);
    assert_eq!(int64::fold(-(1 << 40)), 0xff);
}

#[test]
fn an_int8_in_int4_range_hashes_like_the_int4() {
    property(
        nrows().prop_flat_map(|nrows| proptest::collection::vec(integer::<i32>(), nrows)),
        |small| an_int8_in_int4_range_hashes_like_its_int4(&small),
    );
}

fn an_int8_in_int4_range_hashes_like_its_int4(small: &[i32]) -> Result<()> {
    let wide: Vec<i64> = small.iter().map(|&value| i64::from(value)).collect();
    let nrows = small.len();
    let rows_words = all_rows(nrows);
    let rows = RowMaskView::try_new(nrows, &rows_words)?;
    let (mut narrow, mut broad) = (vec![0; nrows], vec![0; nrows]);
    let (mut narrow_words, mut broad_words) =
        (vec![0; nrows.div_ceil(64)], vec![0; nrows.div_ceil(64)]);
    let narrow_column = ColumnView::try_new(small, None)?;
    let broad_column = ColumnView::try_new(&wide, None)?;
    {
        let mut valid = RowMask::try_new(nrows, &mut narrow_words)?;
        int32::hash(
            &narrow_column,
            &rows,
            int32::NullKeys::Reject,
            &mut narrow,
            &mut valid,
        )?;
        int32::hash_next(
            &narrow_column,
            int32::NullKeys::Reject,
            &mut narrow,
            &mut valid,
        )?;
    }
    {
        let mut valid = RowMask::try_new(nrows, &mut broad_words)?;
        int64::hash(
            &broad_column,
            &rows,
            int64::NullKeys::Reject,
            &mut broad,
            &mut valid,
        )?;
        int64::hash_next(
            &broad_column,
            int64::NullKeys::Reject,
            &mut broad,
            &mut valid,
        )?;
    }
    assert_eq!(narrow, broad, "one key and a chain of two");
    assert_eq!(narrow_words, broad_words);
    // Mixed kinds in one chain: an int4 first key and an int8 second key
    // hash as two int4 keys.
    let mut mixed = vec![0; nrows];
    let mut mixed_words = vec![0; nrows.div_ceil(64)];
    let mut valid = RowMask::try_new(nrows, &mut mixed_words)?;
    int32::hash(
        &narrow_column,
        &rows,
        int32::NullKeys::Reject,
        &mut mixed,
        &mut valid,
    )?;
    int64::hash_next(
        &broad_column,
        int64::NullKeys::Reject,
        &mut mixed,
        &mut valid,
    )?;
    assert_eq!(mixed, narrow);
    Ok(())
}

#[test]
fn further_keys_combine_in_order() -> Result<()> {
    let first = [5_i64, 1 << 33, -9, 42];
    let second = [7_i64, -(1 << 50), 11, 42];
    let non_nulls = [0b1011];
    let first_column = ColumnView::try_new(&first, None)?;
    let second_column = ColumnView::try_new(&second, Some(RowMaskView::try_new(4, &non_nulls)?))?;
    let rows = RowMaskView::try_new(4, &[0b1111])?;
    let mut hashes = [0; 4];
    let mut words = [0];
    let mut valid = RowMask::try_new(4, &mut words)?;
    int64::hash(
        &first_column,
        &rows,
        int64::NullKeys::Reject,
        &mut hashes,
        &mut valid,
    )?;
    int64::hash_next(
        &second_column,
        int64::NullKeys::Reject,
        &mut hashes,
        &mut valid,
    )?;
    assert_eq!(words, [0b1011], "the NULL second key rejects row 2");
    for row in [0, 1, 3] {
        let expected = model_combine(
            model_murmur(model_fold(first[row])),
            model_murmur(model_fold(second[row])),
        );
        assert_eq!(hashes[row], expected, "row {row}");
    }
    Ok(())
}

fn reject_narrows_valid_and_group_hashes_nulls_as_one_key<T: Key>() -> Result<()> {
    let key = |value| murmurhash32(T::spread(value).model_key());
    // Row 1 is NULL, row 3 is not selected.
    let keys = [10, 20, 30, 40].map(T::spread);
    let column = ColumnView::try_new(&keys, Some(RowMaskView::try_new(4, &[0b1101])?))?;
    let rows = RowMaskView::try_new(4, &[0b0111])?;
    let mut hashes = [SENTINEL; 4];
    // The mask's prior contents do not matter: every word is written.
    let mut words = [0b1111];
    let mut valid = RowMask::try_new(4, &mut words)?;
    T::hash(&column, &rows, NullKeys::Reject, &mut hashes, &mut valid)?;
    assert_eq!(mask_words(&valid), [0b0101]);
    assert_eq!(hashes[0], key(10));
    assert_eq!(hashes[2], key(30));
    assert_eq!(hashes[3], SENTINEL, "unselected rows are not written");
    let mut hashes = [SENTINEL; 4];
    let mut words = [0];
    let mut valid = RowMask::try_new(4, &mut words)?;
    T::hash(&column, &rows, NullKeys::Group, &mut hashes, &mut valid)?;
    assert_eq!(mask_words(&valid), [0b0111]);
    assert_eq!(hashes[1], NULL_HASH);
    assert_eq!(hashes[3], SENTINEL);
    // The next key folds into the valid rows and drops row 2, NULL here.
    let second = [1, 2, 3, 4].map(T::spread);
    let column = ColumnView::try_new(&second, Some(RowMaskView::try_new(4, &[0b1011])?))?;
    T::hash_next(&column, NullKeys::Reject, &mut hashes, &mut valid)?;
    assert_eq!(mask_words(&valid), [0b0011]);
    assert_eq!(hashes[0], hash_combine(key(10), key(1)));
    assert_eq!(hashes[1], hash_combine(NULL_HASH, key(2)));
    assert_eq!(hashes[3], SENTINEL);
    Ok(())
}

#[test]
fn reject_narrows_valid_and_group_hashes_nulls_as_one_key_int4() -> Result<()> {
    reject_narrows_valid_and_group_hashes_nulls_as_one_key::<i32>()
}

#[test]
fn reject_narrows_valid_and_group_hashes_nulls_as_one_key_int8() -> Result<()> {
    reject_narrows_valid_and_group_hashes_nulls_as_one_key::<i64>()
}

fn a_rejected_row_is_not_read_by_later_keys<T: Key>() -> Result<()> {
    // Row 5 is NULL in the second key and unprepared in the third: reading
    // it there would fail, so the third key must skip it.
    let nrows = 70;
    let first: Vec<T> = (0..nrows as i32).map(T::spread).collect();
    let second: Vec<T> = (0..nrows as i32).map(|v| T::spread(v * 3)).collect();
    let third: Vec<T> = (0..nrows as i32).map(|v| T::spread(v - 100)).collect();
    let all = vec![true; nrows];
    let mut second_non_null = all.clone();
    second_non_null[5] = false;
    let selected: Vec<bool> = (0..nrows).map(|row| row % 7 != 3).collect();
    let selection = words(&selected);
    let rows = RowMaskView::try_new(nrows, &selection)?;
    let mut hashes = vec![SENTINEL; nrows];
    let mut valid_words = vec![0; 2];
    let mut valid = RowMask::try_new(nrows, &mut valid_words)?;
    T::hash(
        &ColumnView::try_new(&first, None)?,
        &rows,
        NullKeys::Reject,
        &mut hashes,
        &mut valid,
    )?;
    let second_words = words(&second_non_null);
    T::hash_next(
        &ColumnView::try_new(&second, Some(RowMaskView::try_new(nrows, &second_words)?))?,
        NullKeys::Reject,
        &mut hashes,
        &mut valid,
    )?;
    T::hash_next(
        &Prepared {
            values: &third,
            non_null: &all,
            prepared: &[!(1 << 5), u64::MAX],
        },
        NullKeys::Reject,
        &mut hashes,
        &mut valid,
    )?;
    let (expected, expected_valid) = model(
        &[(&first, &all), (&second, &second_non_null), (&third, &all)],
        &selected,
        NullKeys::Reject,
    );
    assert_eq!(mask_words(&valid), words(&expected_valid));
    for row in 0..nrows {
        if let Some(expected) = expected[row] {
            assert_eq!(hashes[row], expected, "row {row}");
        }
        // The whole-word path writes every lane of the first word; the
        // tail goes row by row and leaves unselected rows alone.
        if !selected[row] && row >= 64 {
            assert_eq!(hashes[row], SENTINEL, "row {row}");
        }
    }
    // A selected unprepared row is an error.
    assert!(
        T::hash(
            &Prepared {
                values: &third,
                non_null: &all,
                prepared: &[!(1 << 5), u64::MAX],
            },
            &rows,
            NullKeys::Reject,
            &mut hashes,
            &mut valid,
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn a_rejected_row_is_not_read_by_later_keys_int4() -> Result<()> {
    a_rejected_row_is_not_read_by_later_keys::<i32>()
}

#[test]
fn a_rejected_row_is_not_read_by_later_keys_int8() -> Result<()> {
    a_rejected_row_is_not_read_by_later_keys::<i64>()
}

/// A chain of one to three key columns with their non-NULL flags, a
/// selection, and what the valid mask held before the first key, which it
/// must not matter; the rows a key reads hold keys leaning to the edges,
/// the others edges.
#[derive(Clone, Debug)]
struct Chain<T> {
    keys: Vec<(Vec<T>, Vec<bool>)>,
    selected: Vec<bool>,
    prior: Vec<bool>,
}

fn chains<T: Key>() -> impl Strategy<Value = Chain<T>> {
    (nrows(), 1..=3_usize)
        .prop_flat_map(|(nrows, nkeys)| {
            (
                flags(nrows),
                flags(nrows),
                proptest::collection::vec(flags(nrows), nkeys),
            )
        })
        .prop_flat_map(|(selected, prior, non_nulls)| {
            let keys: Vec<_> = non_nulls
                .into_iter()
                .map(|non_null| {
                    let read: Vec<bool> = selected
                        .iter()
                        .zip(&non_null)
                        .map(|(&s, &n)| s && n)
                        .collect();
                    (values(&read, &T::keys()), Just(non_null))
                })
                .collect();
            (keys, Just(selected), Just(prior))
        })
        .prop_map(|(keys, selected, prior)| Chain {
            keys,
            selected,
            prior,
        })
}

/// Each key of a chain leaves the hashes and the valid mask of the model,
/// under both NULL policies, on the whole-word and the row path.
fn chains_match_the_model_and_the_row_path<T: Key>() {
    property(chains::<T>(), |chain| -> Result<()> {
        let nrows = chain.selected.len();
        let selection = words(&chain.selected);
        let rows = RowMaskView::try_new(nrows, &selection)?;
        for nulls in [NullKeys::Reject, NullKeys::Group] {
            let mut whole = vec![SENTINEL; nrows];
            let mut whole_words = words(&chain.prior);
            let mut whole_valid = RowMask::try_new(nrows, &mut whole_words)?;
            let mut by_rows = vec![SENTINEL; nrows];
            let mut by_rows_words = words(&chain.prior);
            let mut by_rows_valid = RowMask::try_new(nrows, &mut by_rows_words)?;
            let mut keys = Vec::new();
            for (index, (values, non_null)) in chain.keys.iter().enumerate() {
                let non_null_words = words(non_null);
                let column = ColumnView::try_new(
                    values,
                    Some(RowMaskView::try_new(nrows, &non_null_words)?),
                )?;
                if index == 0 {
                    T::hash(&column, &rows, nulls, &mut whole, &mut whole_valid)?;
                    T::hash(
                        &RowsOnly(&column),
                        &rows,
                        nulls,
                        &mut by_rows,
                        &mut by_rows_valid,
                    )?;
                } else {
                    T::hash_next(&column, nulls, &mut whole, &mut whole_valid)?;
                    T::hash_next(&RowsOnly(&column), nulls, &mut by_rows, &mut by_rows_valid)?;
                }
                keys.push((values.as_slice(), non_null.as_slice()));
                let (expected, expected_valid) = model(&keys, &chain.selected, nulls);
                let expected_valid = words(&expected_valid);
                assert_eq!(
                    mask_words(&whole_valid),
                    expected_valid,
                    "{nulls:?} key {index}"
                );
                assert_eq!(
                    mask_words(&by_rows_valid),
                    expected_valid,
                    "{nulls:?} key {index} rows"
                );
                for (row, expected) in expected.iter().enumerate() {
                    if let Some(expected) = *expected {
                        assert_eq!(whole[row], expected, "{nulls:?} key {index} row {row}");
                        assert_eq!(
                            by_rows[row], expected,
                            "{nulls:?} key {index} rows row {row}"
                        );
                    }
                }
            }
        }
        Ok(())
    });
}

#[test]
fn chains_match_the_model_and_the_row_path_int4() {
    chains_match_the_model_and_the_row_path::<i32>();
}

#[test]
fn chains_match_the_model_and_the_row_path_int8() {
    chains_match_the_model_and_the_row_path::<i64>();
}

fn dimension_errors_come_before_any_change<T: Key>() -> Result<()> {
    let keys = [1_i32, 2, 3].map(T::from);
    let column = ColumnView::try_new(&keys, None)?;
    let rows = RowMaskView::try_new(3, &[0b111])?;
    let short_rows = RowMaskView::try_new(2, &[0b11])?;
    let mut hashes = [SENTINEL; 3];
    let mut words = [0b111];
    let mut valid = RowMask::try_new(3, &mut words)?;
    assert!(
        T::hash(
            &column,
            &short_rows,
            NullKeys::Reject,
            &mut hashes,
            &mut valid
        )
        .is_err()
    );
    let mut short_hashes = [SENTINEL; 2];
    assert!(
        T::hash(
            &column,
            &rows,
            NullKeys::Reject,
            &mut short_hashes,
            &mut valid
        )
        .is_err()
    );
    let short_keys = [1_i32, 2].map(T::from);
    let short_column = ColumnView::try_new(&short_keys, None)?;
    assert!(
        T::hash(
            &short_column,
            &rows,
            NullKeys::Group,
            &mut hashes,
            &mut valid
        )
        .is_err()
    );
    assert!(T::hash_next(&short_column, NullKeys::Group, &mut hashes, &mut valid).is_err());
    let mut short_words = [0b11];
    let mut short_valid = RowMask::try_new(2, &mut short_words)?;
    assert!(
        T::hash(
            &column,
            &rows,
            NullKeys::Reject,
            &mut hashes,
            &mut short_valid
        )
        .is_err()
    );
    assert_eq!(mask_words(&valid), [0b111]);
    assert_eq!(hashes, [SENTINEL; 3]);
    // Short hashes fail both calls and leave a partial mask as it was.
    let mut hashes = [7; 2];
    let mut words = [0b101];
    let mut valid = RowMask::try_new(3, &mut words)?;
    assert!(T::hash(&column, &rows, NullKeys::Reject, &mut hashes, &mut valid).is_err());
    assert!(T::hash_next(&column, NullKeys::Reject, &mut hashes, &mut valid).is_err());
    assert_eq!(hashes, [7; 2]);
    assert_eq!(words, [0b101]);
    Ok(())
}

#[test]
fn dimension_errors_come_before_any_change_int4() -> Result<()> {
    dimension_errors_come_before_any_change::<i32>()
}

#[test]
fn dimension_errors_come_before_any_change_int8() -> Result<()> {
    dimension_errors_come_before_any_change::<i64>()
}

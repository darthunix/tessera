#![forbid(unsafe_code)]

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView, WordValues};
use tessera_kernels::int32::{NullKeys, hash_combine, murmurhash32};
use tessera_kernels::{int32, int64};

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

/// The packed words of a flag per row.
fn words_for(flags: &[bool]) -> Vec<u64> {
    let mut words = vec![0; flags.len().div_ceil(64)];
    for (row, &flag) in flags.iter().enumerate() {
        if flag {
            words[row / 64] |= 1 << (row % 64);
        }
    }
    words
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

fn random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Values at and around every edge that matters to the fold, and random
/// ones across the whole range.
fn values(state: &mut u64, count: usize) -> Vec<i64> {
    let mut values = vec![
        0,
        1,
        -1,
        i64::from(i32::MAX),
        i64::from(i32::MIN),
        i64::from(i32::MAX) + 1,
        i64::from(i32::MIN) - 1,
        i64::from(u32::MAX),
        i64::MAX,
        i64::MIN,
        1 << 40,
        -(1 << 40),
    ];
    while values.len() < count {
        values.push(random(state) as i64 >> (random(state) % 63));
    }
    values
}

/// A key width under test: its kernels, the model of its hash and the
/// values its tests draw. An int4 key hashes as its value read as `u32`, an
/// int8 key as its `hashint8` fold, which agrees with the int4 on the int4
/// range.
trait Key: Copy + From<i32> {
    /// What the model hashes for a non-NULL key.
    fn model_key(self) -> u32;
    /// A small value as a key: the int4 as is, the int8 scaled by
    /// `2^33 + 1`, past the int4 range with both halves in play.
    fn spread(value: i32) -> Self;
    /// `count` random keys: the int4 from the high half of a draw, the int8
    /// from [`values`].
    fn random_keys(state: &mut u64, count: usize) -> Vec<Self>;
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

    fn random_keys(state: &mut u64, count: usize) -> Vec<Self> {
        (0..count).map(|_| (random(state) >> 32) as i32).collect()
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

    fn random_keys(state: &mut u64, count: usize) -> Vec<Self> {
        let mut keys = values(state, count);
        keys.truncate(count);
        keys
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
    let mut state = 0x1234_5678_9abc_def1;
    for value in values(&mut state, 10_000) {
        assert_eq!(int64::fold(value), model_fold(value), "{value}");
    }
    assert_eq!(int64::fold(1 << 40), 1 << 8);
    assert_eq!(int64::fold(-(1 << 40)), 0xff);
}

#[test]
fn an_int8_in_int4_range_hashes_like_the_int4() -> Result<()> {
    let mut state = 0x0bad_cafe_0000_0007;
    let mut small: Vec<i32> = vec![0, 1, -1, i32::MAX, i32::MIN, i32::MAX - 1, i32::MIN + 1];
    while small.len() < 300 {
        small.push(random(&mut state) as i32);
    }
    let wide: Vec<i64> = small.iter().map(|&value| i64::from(value)).collect();
    let nrows = small.len();
    let rows_words = all_rows(nrows);
    let rows = RowMaskView::try_new(nrows, &rows_words)?;
    let (mut narrow, mut broad) = (vec![0; nrows], vec![0; nrows]);
    let (mut narrow_words, mut broad_words) =
        (vec![0; nrows.div_ceil(64)], vec![0; nrows.div_ceil(64)]);
    let narrow_column = ColumnView::try_new(&small, None)?;
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
    let words = words_for(&selected);
    let rows = RowMaskView::try_new(nrows, &words)?;
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
    let second_words = words_for(&second_non_null);
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
    assert_eq!(mask_words(&valid), words_for(&expected_valid));
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

fn random_keys_match_the_model_in_both_policies<T: Key>() -> Result<()> {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    for nrows in [0, 1, 63, 64, 65, 200] {
        let columns: Vec<(Vec<T>, Vec<bool>)> = (0..3)
            .map(|_| {
                let values = T::random_keys(&mut state, nrows);
                let non_null = (0..nrows)
                    .map(|_| !random(&mut state).is_multiple_of(5))
                    .collect();
                (values, non_null)
            })
            .collect();
        let selected: Vec<bool> = (0..nrows)
            .map(|_| random(&mut state).is_multiple_of(2))
            .collect();
        let words = words_for(&selected);
        let rows = RowMaskView::try_new(nrows, &words)?;
        for nulls in [NullKeys::Reject, NullKeys::Group] {
            let mut hashes = vec![SENTINEL; nrows];
            let mut valid_words = words_for(&vec![true; nrows]);
            let mut valid = RowMask::try_new(nrows, &mut valid_words)?;
            let mut keys = Vec::new();
            for (index, (values, non_null)) in columns.iter().enumerate() {
                let non_null_words = words_for(non_null);
                let column = ColumnView::try_new(
                    values,
                    Some(RowMaskView::try_new(nrows, &non_null_words)?),
                )?;
                if index == 0 {
                    T::hash(&column, &rows, nulls, &mut hashes, &mut valid)?;
                } else {
                    T::hash_next(&column, nulls, &mut hashes, &mut valid)?;
                }
                keys.push((values.as_slice(), non_null.as_slice()));
                let (expected, expected_valid) = model(&keys, &selected, nulls);
                assert_eq!(
                    mask_words(&valid),
                    words_for(&expected_valid),
                    "{nrows} {nulls:?}"
                );
                for row in 0..nrows {
                    if let Some(expected) = expected[row] {
                        assert_eq!(hashes[row], expected, "{nrows} {nulls:?} row {row}");
                    }
                }
            }
        }
    }
    Ok(())
}

#[test]
fn random_keys_match_the_model_in_both_policies_int4() -> Result<()> {
    random_keys_match_the_model_in_both_policies::<i32>()
}

#[test]
fn random_keys_match_the_model_in_both_policies_int8() -> Result<()> {
    random_keys_match_the_model_in_both_policies::<i64>()
}

fn hashes_match_the_model_under_both_policies<T: Key>() -> Result<()> {
    let mut state = 0xfeed_beef_0000_0001;
    for nrows in [0, 1, 63, 64, 65, 200] {
        let keys = T::random_keys(&mut state, nrows);
        let mut non_nulls = all_rows(nrows);
        let mut selected = all_rows(nrows);
        for word in non_nulls.iter_mut().chain(selected.iter_mut()) {
            *word &= random(&mut state) | random(&mut state);
        }
        let column = ColumnView::try_new(&keys, Some(RowMaskView::try_new(nrows, &non_nulls)?))?;
        let rows = RowMaskView::try_new(nrows, &selected)?;
        for (policy, group) in [(NullKeys::Reject, false), (NullKeys::Group, true)] {
            let mut hashes = vec![SENTINEL; nrows];
            let mut words = vec![0; nrows.div_ceil(64)];
            let mut valid = RowMask::try_new(nrows, &mut words)?;
            T::hash(&column, &rows, policy, &mut hashes, &mut valid)?;
            for row in 0..nrows {
                let chosen = selected[row / 64] >> (row % 64) & 1 == 1;
                let present = non_nulls[row / 64] >> (row % 64) & 1 == 1;
                let expected = chosen && (present || group);
                assert_eq!(
                    words[row / 64] >> (row % 64) & 1 == 1,
                    expected,
                    "{nrows} rows, row {row}"
                );
                if expected {
                    let hash = if present {
                        model_murmur(keys[row].model_key())
                    } else {
                        NULL_HASH
                    };
                    assert_eq!(hashes[row], hash, "{nrows} rows, row {row}");
                }
            }
        }
    }
    Ok(())
}

#[test]
fn hashes_match_the_model_under_both_policies_int4() -> Result<()> {
    hashes_match_the_model_under_both_policies::<i32>()
}

#[test]
fn hashes_match_the_model_under_both_policies_int8() -> Result<()> {
    hashes_match_the_model_under_both_policies::<i64>()
}

fn whole_words_agree_with_the_row_path<T: Key>() -> Result<()> {
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let nrows = 4 * 64 + 11;
    let columns: Vec<(Vec<T>, Vec<bool>)> = (0..3)
        .map(|_| {
            let values = T::random_keys(&mut state, nrows);
            let non_null = (0..nrows)
                .map(|_| !random(&mut state).is_multiple_of(4))
                .collect();
            (values, non_null)
        })
        .collect();
    // A full first word puts the call on the whole-word path; later words
    // range from full to sparse, single-row and empty, then the tail.
    let selected: Vec<bool> = (0..nrows)
        .map(|row| match row / 64 {
            0 => true,
            1 => random(&mut state).is_multiple_of(2),
            2 => row % 64 == 5,
            3 => false,
            _ => row % 2 == 0,
        })
        .collect();
    let words = words_for(&selected);
    let rows = RowMaskView::try_new(nrows, &words)?;
    for nulls in [NullKeys::Reject, NullKeys::Group] {
        let mut whole = vec![SENTINEL; nrows];
        let mut whole_words = vec![0; nrows.div_ceil(64)];
        let mut whole_valid = RowMask::try_new(nrows, &mut whole_words)?;
        let mut by_rows = vec![SENTINEL; nrows];
        let mut by_rows_words = vec![0; nrows.div_ceil(64)];
        let mut by_rows_valid = RowMask::try_new(nrows, &mut by_rows_words)?;
        for (index, (values, non_null)) in columns.iter().enumerate() {
            let non_null_words = words_for(non_null);
            let column =
                ColumnView::try_new(values, Some(RowMaskView::try_new(nrows, &non_null_words)?))?;
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
            assert_eq!(
                mask_words(&whole_valid),
                mask_words(&by_rows_valid),
                "{nulls:?} key {index}"
            );
            for row in whole_valid.as_view().selected_indices() {
                assert_eq!(whole[row], by_rows[row], "{nulls:?} key {index} row {row}");
            }
        }
    }
    Ok(())
}

#[test]
fn whole_words_agree_with_the_row_path_int4() -> Result<()> {
    whole_words_agree_with_the_row_path::<i32>()
}

#[test]
fn whole_words_agree_with_the_row_path_int8() -> Result<()> {
    whole_words_agree_with_the_row_path::<i64>()
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

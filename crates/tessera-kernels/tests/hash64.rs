#![forbid(unsafe_code)]

use anyhow::Result;
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView};
use tessera_kernels::{int32, int64};

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

fn random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn all_rows(nrows: usize) -> Vec<u64> {
    let mut words = vec![u64::MAX; nrows.div_ceil(64)];
    if !nrows.is_multiple_of(64) {
        *words.last_mut().unwrap() = (1 << (nrows % 64)) - 1;
    }
    words
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
fn hashes_match_the_model_under_both_policies() -> Result<()> {
    let mut state = 0xfeed_beef_0000_0001;
    for nrows in [0, 1, 63, 64, 65, 200] {
        let keys = values(&mut state, nrows.max(12))[..nrows].to_vec();
        let mut non_nulls = all_rows(nrows);
        let mut selected = all_rows(nrows);
        for word in non_nulls.iter_mut().chain(selected.iter_mut()) {
            *word &= random(&mut state) | random(&mut state);
        }
        let column = ColumnView::try_new(&keys, Some(RowMaskView::try_new(nrows, &non_nulls)?))?;
        let rows = RowMaskView::try_new(nrows, &selected)?;
        for (policy, group) in [
            (int64::NullKeys::Reject, false),
            (int64::NullKeys::Group, true),
        ] {
            let mut hashes = vec![0x5a5a_5a5a; nrows];
            let mut words = vec![0; nrows.div_ceil(64)];
            let mut valid = RowMask::try_new(nrows, &mut words)?;
            int64::hash(&column, &rows, policy, &mut hashes, &mut valid)?;
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
                        model_murmur(model_fold(keys[row]))
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

/// The same values without bulk storage: every call takes the row path.
struct RowsOnly<'a>(&'a ColumnView<'a, i64>);

impl ColumnReader for RowsOnly<'_> {
    type Value = i64;
    fn nrows(&self) -> usize {
        self.0.nrows()
    }
    fn get(&self, row: usize) -> Result<Option<i64>> {
        ColumnReader::get(self.0, row)
    }
    fn word_values(
        &self,
        word_index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<i64>)> + '_> {
        self.0.word_values(word_index, selected)
    }
}

/// The packed words of a flag per row.
fn words_for(flags: &[bool]) -> Vec<u64> {
    let mut words = vec![0; flags.len().div_ceil(64)];
    for (row, _) in flags.iter().enumerate().filter(|(_, flag)| **flag) {
        words[row / 64] |= 1 << (row % 64);
    }
    words
}

#[test]
fn whole_words_agree_with_the_row_path() -> Result<()> {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let nrows = 4 * 64 + 11;
    let columns: Vec<(Vec<i64>, Vec<bool>)> = (0..3)
        .map(|_| {
            let values = values(&mut state, nrows)[..nrows].to_vec();
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
    for nulls in [int64::NullKeys::Reject, int64::NullKeys::Group] {
        let mut whole = vec![0x5a5a_5a5a; nrows];
        let mut whole_words = vec![0; nrows.div_ceil(64)];
        let mut by_rows = vec![0x5a5a_5a5a; nrows];
        let mut by_rows_words = vec![0; nrows.div_ceil(64)];
        for (index, (values, non_null)) in columns.iter().enumerate() {
            let non_null_words = words_for(non_null);
            let column =
                ColumnView::try_new(values, Some(RowMaskView::try_new(nrows, &non_null_words)?))?;
            let mut whole_valid = RowMask::try_new(nrows, &mut whole_words)?;
            let mut by_rows_valid = RowMask::try_new(nrows, &mut by_rows_words)?;
            if index == 0 {
                int64::hash(&column, &rows, nulls, &mut whole, &mut whole_valid)?;
                int64::hash(
                    &RowsOnly(&column),
                    &rows,
                    nulls,
                    &mut by_rows,
                    &mut by_rows_valid,
                )?;
            } else {
                int64::hash_next(&column, nulls, &mut whole, &mut whole_valid)?;
                int64::hash_next(&RowsOnly(&column), nulls, &mut by_rows, &mut by_rows_valid)?;
            }
            assert_eq!(whole_words, by_rows_words, "{nulls:?} key {index}");
            for row in 0..nrows {
                if whole_words[row / 64] >> (row % 64) & 1 == 1 {
                    assert_eq!(whole[row], by_rows[row], "{nulls:?} key {index} row {row}");
                }
            }
        }
    }
    Ok(())
}

#[test]
fn dimension_errors_come_before_any_change() -> Result<()> {
    let keys = [1_i64, 2, 3];
    let column = ColumnView::try_new(&keys, None)?;
    let rows = RowMaskView::try_new(3, &[0b111])?;
    let mut hashes = [7; 2];
    let mut words = [0b101];
    let mut valid = RowMask::try_new(3, &mut words)?;
    assert!(
        int64::hash(
            &column,
            &rows,
            int64::NullKeys::Reject,
            &mut hashes,
            &mut valid
        )
        .is_err()
    );
    assert!(int64::hash_next(&column, int64::NullKeys::Reject, &mut hashes, &mut valid).is_err());
    assert_eq!(hashes, [7; 2]);
    assert_eq!(words, [0b101]);
    Ok(())
}

#![forbid(unsafe_code)]

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView, WordValues};
use tessera_kernels::int32::{NullKeys, hash, hash_combine, hash_next, murmurhash32};

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

fn random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// The hashes and valid mask of a chain of keys, per the model: NULL under
/// Reject drops the row for good; under Group it hashes the group key.
fn model(
    keys: &[(&[i32], &[bool])],
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
                values[row] as u32
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
fn reject_narrows_valid_and_group_hashes_nulls_as_one_key() -> Result<()> {
    // Row 1 is NULL, row 3 is not selected.
    let keys = [10, 20, 30, 40];
    let column = ColumnView::try_new(&keys, Some(RowMaskView::try_new(4, &[0b1101])?))?;
    let rows = RowMaskView::try_new(4, &[0b0111])?;
    let mut hashes = [SENTINEL; 4];
    // The mask's prior contents do not matter: every word is written.
    let mut words = [0b1111];
    let mut valid = RowMask::try_new(4, &mut words)?;
    hash(&column, &rows, NullKeys::Reject, &mut hashes, &mut valid)?;
    assert_eq!(mask_words(&valid), [0b0101]);
    assert_eq!(hashes[0], murmurhash32(10));
    assert_eq!(hashes[2], murmurhash32(30));
    assert_eq!(hashes[3], SENTINEL, "unselected rows are not written");
    let mut hashes = [SENTINEL; 4];
    let mut words = [0];
    let mut valid = RowMask::try_new(4, &mut words)?;
    hash(&column, &rows, NullKeys::Group, &mut hashes, &mut valid)?;
    assert_eq!(mask_words(&valid), [0b0111]);
    assert_eq!(hashes[1], NULL_HASH);
    assert_eq!(hashes[3], SENTINEL);
    // The next key folds into the valid rows and drops row 2, NULL here.
    let second = [1, 2, 3, 4];
    let column = ColumnView::try_new(&second, Some(RowMaskView::try_new(4, &[0b1011])?))?;
    hash_next(&column, NullKeys::Reject, &mut hashes, &mut valid)?;
    assert_eq!(mask_words(&valid), [0b0011]);
    assert_eq!(hashes[0], hash_combine(murmurhash32(10), murmurhash32(1)));
    assert_eq!(hashes[1], hash_combine(NULL_HASH, murmurhash32(2)));
    assert_eq!(hashes[3], SENTINEL);
    Ok(())
}

/// A reader that refuses unprepared rows, to show which rows a call reads.
struct Prepared<'a> {
    values: &'a [i32],
    non_null: &'a [bool],
    prepared: &'a [u64],
}

impl ColumnReader for Prepared<'_> {
    type Value = i32;

    fn nrows(&self) -> usize {
        self.values.len()
    }

    fn get(&self, row: usize) -> Result<Option<i32>> {
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
    ) -> Result<impl Iterator<Item = (usize, Option<i32>)> + '_> {
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
fn a_rejected_row_is_not_read_by_later_keys() -> Result<()> {
    // Row 5 is NULL in the second key and unprepared in the third: reading
    // it there would fail, so the third key must skip it.
    let nrows = 70;
    let first: Vec<i32> = (0..nrows as i32).collect();
    let second: Vec<i32> = (0..nrows as i32).map(|v| v * 3).collect();
    let third: Vec<i32> = (0..nrows as i32).map(|v| v - 100).collect();
    let all = vec![true; nrows];
    let mut second_non_null = all.clone();
    second_non_null[5] = false;
    let selected: Vec<bool> = (0..nrows).map(|row| row % 7 != 3).collect();
    let words = words_for(&selected);
    let rows = RowMaskView::try_new(nrows, &words)?;
    let mut hashes = vec![SENTINEL; nrows];
    let mut valid_words = vec![0; 2];
    let mut valid = RowMask::try_new(nrows, &mut valid_words)?;
    hash(
        &ColumnView::try_new(&first, None)?,
        &rows,
        NullKeys::Reject,
        &mut hashes,
        &mut valid,
    )?;
    let second_words = words_for(&second_non_null);
    hash_next(
        &ColumnView::try_new(&second, Some(RowMaskView::try_new(nrows, &second_words)?))?,
        NullKeys::Reject,
        &mut hashes,
        &mut valid,
    )?;
    hash_next(
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
        hash(
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
fn random_keys_match_the_model_in_both_policies() -> Result<()> {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    for nrows in [0, 1, 63, 64, 65, 200] {
        let columns: Vec<(Vec<i32>, Vec<bool>)> = (0..3)
            .map(|_| {
                let values = (0..nrows)
                    .map(|_| (random(&mut state) >> 32) as i32)
                    .collect();
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
                    hash(&column, &rows, nulls, &mut hashes, &mut valid)?;
                } else {
                    hash_next(&column, nulls, &mut hashes, &mut valid)?;
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

/// The same values without bulk storage: every call takes the row path.
struct RowsOnly<'a>(&'a ColumnView<'a, i32>);

impl ColumnReader for RowsOnly<'_> {
    type Value = i32;
    fn nrows(&self) -> usize {
        self.0.nrows()
    }
    fn get(&self, row: usize) -> Result<Option<i32>> {
        ColumnReader::get(self.0, row)
    }
    fn word_values(
        &self,
        word_index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<i32>)> + '_> {
        self.0.word_values(word_index, selected)
    }
}

#[test]
fn whole_words_agree_with_the_row_path() -> Result<()> {
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let nrows = 4 * 64 + 11;
    let columns: Vec<(Vec<i32>, Vec<bool>)> = (0..3)
        .map(|_| {
            let values = (0..nrows)
                .map(|_| (random(&mut state) >> 32) as i32)
                .collect();
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
                hash(&column, &rows, nulls, &mut whole, &mut whole_valid)?;
                hash(
                    &RowsOnly(&column),
                    &rows,
                    nulls,
                    &mut by_rows,
                    &mut by_rows_valid,
                )?;
            } else {
                hash_next(&column, nulls, &mut whole, &mut whole_valid)?;
                hash_next(&RowsOnly(&column), nulls, &mut by_rows, &mut by_rows_valid)?;
            }
            assert_eq!(
                mask_words(&whole_valid),
                mask_words(&by_rows_valid),
                "{nulls:?}"
            );
            for row in whole_valid.as_view().selected_indices() {
                assert_eq!(whole[row], by_rows[row], "{nulls:?} key {index} row {row}");
            }
        }
    }
    Ok(())
}

#[test]
fn dimension_errors_come_before_any_mutation() -> Result<()> {
    let keys = [1, 2, 3];
    let column = ColumnView::try_new(&keys, None)?;
    let rows = RowMaskView::try_new(3, &[0b111])?;
    let short_rows = RowMaskView::try_new(2, &[0b11])?;
    let mut hashes = [SENTINEL; 3];
    let mut words = [0b111];
    let mut valid = RowMask::try_new(3, &mut words)?;
    assert!(
        hash(
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
        hash(
            &column,
            &rows,
            NullKeys::Reject,
            &mut short_hashes,
            &mut valid
        )
        .is_err()
    );
    let short_keys = [1, 2];
    let short_column = ColumnView::try_new(&short_keys, None)?;
    assert!(
        hash(
            &short_column,
            &rows,
            NullKeys::Group,
            &mut hashes,
            &mut valid
        )
        .is_err()
    );
    assert!(hash_next(&short_column, NullKeys::Group, &mut hashes, &mut valid).is_err());
    let mut short_words = [0b11];
    let mut short_valid = RowMask::try_new(2, &mut short_words)?;
    assert!(
        hash(
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
    Ok(())
}

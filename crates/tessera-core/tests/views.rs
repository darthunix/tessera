#![forbid(unsafe_code)]

use std::cell::Cell;

use anyhow::Context;
use tessera_core::{ColumnView, RowMask, RowMaskView};

fn sizes() -> impl Iterator<Item = usize> {
    (0..=129).chain([255, 256, 257, 1025])
}

fn patterns(nrows: usize) -> Vec<Vec<bool>> {
    (0..6)
        .map(|kind| {
            (0..nrows)
                .map(|row| match kind {
                    0 => false,
                    1 => true,
                    2 => row.is_multiple_of(2),
                    3 => row % 5 == 1,
                    4 => row.is_multiple_of(64) || row % 64 == 63,
                    _ => row == 0 || row + 1 == nrows,
                })
                .collect()
        })
        .collect()
}

fn words_for(flags: &[bool]) -> Vec<u64> {
    let mut words = vec![0; flags.len().div_ceil(64)];
    for (row, &flag) in flags.iter().enumerate() {
        if flag {
            words[row / 64] |= 1_u64 << (row % 64);
        }
    }
    words
}

fn assert_mask(mask: RowMaskView<'_>, expected: &[bool]) {
    assert_eq!(mask.nrows(), expected.len());
    let indices: Vec<_> = expected
        .iter()
        .enumerate()
        .filter_map(|(row, &selected)| selected.then_some(row))
        .collect();
    assert_eq!(mask.selected_count(), indices.len());
    assert_eq!(mask.selected_indices().collect::<Vec<_>>(), indices);
    for (row, &selected) in expected.iter().enumerate() {
        assert_eq!(mask.contains(row).unwrap(), selected);
    }
    let mut iter = mask.selected_indices();
    for row in indices {
        assert_eq!(iter.next(), Some(row));
    }
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next(), None);
    for row in [expected.len(), usize::MAX] {
        assert_eq!(
            mask.contains(row).unwrap_err().to_string(),
            format!(
                "physical row {row} is out of bounds for {} rows",
                expected.len()
            )
        );
    }
}

#[test]
fn masks_match_boolean_model() {
    for nrows in sizes() {
        for expected in patterns(nrows) {
            let words = words_for(&expected);
            assert_mask(RowMaskView::try_new(nrows, &words).unwrap(), &expected);
        }
    }
}

#[test]
fn byte_windows_match_word_masks() {
    for nrows in [0, 1, 7, 8, 9, 63, 64, 65, 127, 128, 129, 257] {
        for offset in 0..80 {
            for expected in patterns(nrows) {
                // Set all bits outside the window to catch accidental exposure.
                let mut bytes = vec![u8::MAX; (offset + nrows).div_ceil(8) + 1];
                for (row, &selected) in expected.iter().enumerate() {
                    if !selected {
                        bytes[(offset + row) / 8] &= !(1 << ((offset + row) % 8));
                    }
                }
                let byte_mask = RowMaskView::try_from_bytes(nrows, &bytes, offset).unwrap();
                assert_mask(byte_mask, &expected);
                let mut words = words_for(&vec![true; nrows]);
                let expected_words = words_for(&expected);
                for (index, &word) in expected_words.iter().enumerate() {
                    assert_eq!(byte_mask.word(index), Some(word));
                }
                assert_eq!(byte_mask.word(expected_words.len()), None);
                assert_eq!(byte_mask.word(usize::MAX), None);
                RowMask::try_new(nrows, &mut words)
                    .unwrap()
                    .intersect(byte_mask)
                    .unwrap();
                assert_eq!(words, expected_words);
            }
        }
    }
}

#[test]
fn byte_windows_validate_ranges_and_exact_buffers() {
    for (nrows, offset) in [(0, 0), (0, 8), (1, 7), (8, 0), (7, 1)] {
        let mask = RowMaskView::try_from_bytes(nrows, &[u8::MAX], offset).unwrap();
        assert_eq!(mask.selected_count(), nrows);
    }
    assert!(RowMaskView::try_from_bytes(0, &[], 0).is_ok());
    for (nrows, offset) in [
        (0, 9),
        (1, 8),
        (9, 0),
        (8, 1),
        (usize::MAX, 1),
        (1, usize::MAX),
    ] {
        assert!(RowMaskView::try_from_bytes(nrows, &[0], offset).is_err());
    }
    // A shifted full word needs nine bytes; a shorter final word must not
    // access a ninth byte unless it is part of the validated window.
    for offset in 0_usize..8 {
        for nrows in 1..=129 {
            let bytes = vec![u8::MAX; (offset + nrows).div_ceil(8)];
            let mask = RowMaskView::try_from_bytes(nrows, &bytes, offset).unwrap();
            assert_mask(mask, &vec![true; nrows]);
        }
    }
}

#[test]
fn clear_matches_boolean_model_and_is_idempotent() {
    for nrows in sizes() {
        for mut expected in patterns(nrows) {
            let mut words = words_for(&expected);
            {
                let mut mask = RowMask::try_new(nrows, &mut words).unwrap();
                for row in (0..nrows).rev() {
                    mask.clear(row);
                    mask.clear(row);
                    expected[row] = false;
                    assert_mask(mask.as_view(), &expected);
                }
            }
            assert!(words.iter().all(|&word| word == 0));
        }
    }
}

#[test]
fn intersection_matches_boolean_model_and_never_restores_rows() {
    for nrows in sizes() {
        for selected in patterns(nrows) {
            for keep in patterns(nrows) {
                let expected: Vec<_> = selected
                    .iter()
                    .zip(&keep)
                    .map(|(&left, &right)| left && right)
                    .collect();
                let mut words = words_for(&selected);
                let keep_words = words_for(&keep);
                let keep_mask = RowMaskView::try_new(nrows, &keep_words).unwrap();
                let mut mask = RowMask::try_new(nrows, &mut words).unwrap();
                mask.intersect(keep_mask).unwrap();
                assert_mask(mask.as_view(), &expected);
                mask.intersect(keep_mask).unwrap();
                assert_mask(mask.as_view(), &expected);
                let all_words = words_for(&vec![true; nrows]);
                mask.intersect(RowMaskView::try_new(nrows, &all_words).unwrap())
                    .unwrap();
                assert_mask(mask.as_view(), &expected);
            }
        }
    }
}

#[test]
fn masks_match_existing_c_test_cases() {
    let zero = RowMaskView::try_new(0, &[]).unwrap();
    assert_eq!(zero.selected_count(), 0);
    assert_eq!(zero.selected_indices().next(), None);
    let empty = RowMaskView::try_new(7, &[0]).unwrap();
    assert_eq!(empty.selected_count(), 0);
    assert_eq!(empty.selected_indices().next(), None);

    let mut small_words = [(1 << 0) | (1 << 2) | (1 << 5)];
    let mut small = RowMask::try_new(6, &mut small_words).unwrap();
    assert_eq!(
        small.as_view().selected_indices().collect::<Vec<_>>(),
        [0, 2, 5]
    );
    small.clear(2);
    assert_eq!(
        small.as_view().selected_indices().collect::<Vec<_>>(),
        [0, 5]
    );

    let mut wide_words = [(1 << 0) | (1 << 63), (1 << 0) | (1 << 5)];
    let mut wide = RowMask::try_new(70, &mut wide_words).unwrap();
    assert_eq!(
        wide.as_view().selected_indices().collect::<Vec<_>>(),
        [0, 63, 64, 69]
    );
    wide.clear(64);
    let keep_words = [(1 << 1) | (1 << 63), (1 << 0) | (1 << 5)];
    wide.intersect(RowMaskView::try_new(70, &keep_words).unwrap())
        .unwrap();
    assert_eq!(
        wide.as_view().selected_indices().collect::<Vec<_>>(),
        [63, 69]
    );
    assert_eq!(wide.as_view().selected_count(), 2);
}

#[test]
fn bitmap_constructors_reject_wrong_word_counts_without_mutation() {
    for nrows in sizes() {
        let expected = nrows.div_ceil(64);
        for actual in [0, expected.saturating_sub(1), expected + 1] {
            if actual == expected {
                continue;
            }
            let mut words = vec![u64::MAX; actual];
            let original = words.clone();
            let message = format!("expected {expected} bitmap words, got {actual}");
            assert_eq!(
                RowMaskView::try_new(nrows, &words).unwrap_err().to_string(),
                message
            );
            assert_eq!(
                RowMask::try_new(nrows, &mut words).unwrap_err().to_string(),
                message
            );
            assert_eq!(words, original);
        }
    }
}

#[test]
fn bitmap_constructors_reject_nonzero_tail_bits_without_mutation() {
    for nrows in sizes().filter(|nrows| !nrows.is_multiple_of(64)) {
        for bit in nrows % 64..64 {
            let mut words = words_for(&vec![false; nrows]);
            *words.last_mut().unwrap() |= 1_u64 << bit;
            let original = words.clone();
            assert_eq!(
                RowMaskView::try_new(nrows, &words).unwrap_err().to_string(),
                "bitmap has set bits beyond its physical row count"
            );
            assert_eq!(
                RowMask::try_new(nrows, &mut words).unwrap_err().to_string(),
                "bitmap has set bits beyond its physical row count"
            );
            assert_eq!(words, original);
        }
    }
}

#[test]
fn extreme_dimensions_fail_without_large_allocations_or_overflow() {
    for nrows in [usize::MAX - 63, usize::MAX - 62, usize::MAX] {
        let expected = nrows / 64 + usize::from(nrows % 64 != 0);
        let message = format!("expected {expected} bitmap words, got 0");
        assert_eq!(
            RowMaskView::try_new(nrows, &[]).unwrap_err().to_string(),
            message
        );
        assert_eq!(
            RowMask::try_new(nrows, &mut []).unwrap_err().to_string(),
            message
        );
    }
}

#[test]
fn clear_ignores_out_of_bounds_rows_without_changing_any_word() {
    for nrows in sizes() {
        for expected in patterns(nrows) {
            let original = words_for(&expected);
            let mut words = original.clone();
            let first_absent_word_row = words.len() * 64;
            // Include every padding bit, the first absent word, and MAX.
            for row in (nrows..=first_absent_word_row).chain([usize::MAX]) {
                {
                    let mut mask = RowMask::try_new(nrows, &mut words).unwrap();
                    mask.clear(row);
                    mask.clear(row);
                    assert_mask(mask.as_view(), &expected);
                }
                assert_eq!(words, original);
            }
        }
    }
}

#[test]
fn failed_intersections_preserve_every_word() {
    for nrows in sizes() {
        let original = words_for(&vec![true; nrows]);
        let mut words = original.clone();
        {
            let mut mask = RowMask::try_new(nrows, &mut words).unwrap();
            for other_nrows in [nrows.saturating_sub(1), nrows + 1, nrows + 64] {
                if other_nrows == nrows {
                    continue;
                }
                let other_words = words_for(&vec![false; other_nrows]);
                let other = RowMaskView::try_new(other_nrows, &other_words).unwrap();
                assert_eq!(
                    mask.intersect(other).unwrap_err().to_string(),
                    format!("expected {nrows} physical rows, got {other_nrows}")
                );
                assert_mask(mask.as_view(), &vec![true; nrows]);
            }
        }
        assert_eq!(words, original);
    }
}

#[test]
fn mutable_mask_can_be_reborrowed_after_reading() {
    let mut words = [0b111];
    {
        let mut mask = RowMask::try_new(3, &mut words).unwrap();
        {
            let view = mask.as_view();
            assert_eq!(view.selected_indices().collect::<Vec<_>>(), [0, 1, 2]);
        }
        mask.clear(1);
        assert_eq!(
            mask.as_view().selected_indices().collect::<Vec<_>>(),
            [0, 2]
        );
    }
    assert_eq!(words, [0b101]);
}

#[test]
fn all_non_null_matches_explicit_bitmap_including_bounds() {
    for nrows in sizes() {
        let values: Vec<_> = (0..nrows).collect();
        let words = words_for(&vec![true; nrows]);
        let non_nulls = RowMaskView::try_new(nrows, &words).unwrap();
        let bitmap = ColumnView::try_new(&values, Some(non_nulls)).unwrap();
        let all_non_null = ColumnView::try_new(&values, None).unwrap();
        assert_eq!(all_non_null.nrows(), nrows);
        for row in 0..nrows {
            assert_eq!(all_non_null.get(row).unwrap(), bitmap.get(row).unwrap());
        }
        for row in [nrows, usize::MAX] {
            assert_eq!(
                all_non_null.get(row).unwrap_err().to_string(),
                bitmap.get(row).unwrap_err().to_string()
            );
        }
    }
}

#[test]
fn columns_borrow_original_non_copy_values_and_distinguish_null_from_bounds() {
    for nrows in sizes() {
        let values: Vec<_> = (0..nrows).map(|row| format!("row-{row}")).collect();
        for expected in patterns(nrows) {
            let words = words_for(&expected);
            let non_nulls = RowMaskView::try_new(nrows, &words).unwrap();
            let column = ColumnView::try_new(&values, Some(non_nulls)).unwrap();
            assert_eq!(column.nrows(), nrows);
            for (row, &non_null) in expected.iter().enumerate() {
                let actual = column.get(row).unwrap();
                if non_null {
                    assert!(std::ptr::eq(actual.unwrap(), &values[row]));
                } else {
                    assert!(actual.is_none());
                }
            }
            for row in [nrows, usize::MAX] {
                assert_eq!(
                    column.get(row).unwrap_err().to_string(),
                    format!("physical row {row} is out of bounds for {nrows} rows")
                );
            }
        }
    }
}

#[test]
fn columns_accept_all_non_null_without_bitmap_including_empty_values() {
    for nrows in sizes() {
        let values: Vec<_> = (0..nrows).collect();
        let column = ColumnView::try_new(&values, None).unwrap();
        assert_eq!(column.nrows(), nrows);
        for (row, value) in values.iter().enumerate() {
            assert!(std::ptr::eq(column.get(row).unwrap().unwrap(), value));
        }
        for row in [nrows, usize::MAX] {
            assert_eq!(
                column.get(row).unwrap_err().to_string(),
                format!("physical row {row} is out of bounds for {nrows} rows")
            );
        }
    }
}

#[test]
fn column_construction_rejects_mismatched_non_nulls() {
    for (expected, actual) in [(0, 1), (1, 0), (1, 2), (64, 65), (65, 64)] {
        let values = vec![0; expected];
        let words = words_for(&vec![true; actual]);
        let non_nulls = RowMaskView::try_new(actual, &words).unwrap();
        assert_eq!(
            ColumnView::try_new(&values, Some(non_nulls))
                .unwrap_err()
                .to_string(),
            format!("expected {expected} physical rows, got {actual}")
        );
    }
}

#[test]
fn selection_does_not_change_non_nulls_or_physical_indices() {
    let values = [10, 20, 30, 40];
    let non_null_words = [0b1010];
    let non_nulls = RowMaskView::try_new(4, &non_null_words).unwrap();
    let column = ColumnView::try_new(&values, Some(non_nulls)).unwrap();
    let mut row_words = [0b1111];
    let mut rows = RowMask::try_new(4, &mut row_words).unwrap();
    rows.clear(0);
    rows.clear(3);
    let selected: Vec<_> = rows
        .as_view()
        .selected_indices()
        .map(|row| (row, column.get(row).unwrap()))
        .collect();
    assert_eq!(selected, [(1, Some(&20)), (2, None)]);
    assert_eq!(column.get(3).unwrap(), Some(&40));
    assert_eq!(column.nrows(), 4);
}

#[test]
fn dropping_a_column_does_not_drop_any_values_even_at_null_positions() {
    struct Value<'a>(&'a Cell<usize>);
    impl Drop for Value<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    let drops = Cell::new(0);
    let values = [Value(&drops), Value(&drops)];
    {
        let words = [0b01];
        let non_nulls = RowMaskView::try_new(2, &words).unwrap();
        let column = ColumnView::try_new(&values, Some(non_nulls)).unwrap();
        assert!(std::ptr::eq(column.get(0).unwrap().unwrap(), &values[0]));
        assert!(column.get(1).unwrap().is_none());
    }
    assert_eq!(drops.get(), 0);
    drop(values);
    assert_eq!(drops.get(), 2);
}

#[test]
fn errors_support_anyhow_results_and_context() {
    let result: anyhow::Result<RowMaskView<'_>> = RowMaskView::try_new(65, &[0]);
    let error = result.context("cannot borrow row mask").unwrap_err();
    assert_eq!(error.to_string(), "cannot borrow row mask");
    assert_eq!(
        format!("{error:#}"),
        "cannot borrow row mask: expected 2 bitmap words, got 1"
    );
    assert_eq!(
        error.root_cause().to_string(),
        "expected 2 bitmap words, got 1"
    );
}

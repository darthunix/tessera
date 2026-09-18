#![forbid(unsafe_code)]

use anyhow::Result;
use tessera_core::{ColumnView, RowMaskView};
use tessera_kernels::int32::{count, max, min, sum};

const VALUES: [i32; 7] = [i32::MIN, -42, -1, 0, 1, 42, i32::MAX];

fn words_for(flags: &[bool]) -> Vec<u64> {
    let mut words = vec![0; flags.len().div_ceil(64)];
    for (row, &flag) in flags.iter().enumerate() {
        if flag {
            words[row / 64] |= 1 << (row % 64);
        }
    }
    words
}

/// xorshift64*, fixed seed: the same data on every run.
fn random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

struct Model {
    count: usize,
    sum: Option<i64>,
    min: Option<i32>,
    max: Option<i32>,
}

fn model(values: &[i32], selected: &[bool], non_null: &[bool]) -> Model {
    let present: Vec<i32> = (0..values.len())
        .filter(|&row| selected[row] && non_null[row])
        .map(|row| values[row])
        .collect();
    Model {
        count: present.len(),
        sum: (!present.is_empty()).then(|| present.iter().map(|&v| i64::from(v)).sum()),
        min: present.iter().copied().min(),
        max: present.iter().copied().max(),
    }
}

fn assert_aggregates(
    column: &ColumnView<'_, i32>,
    rows: &RowMaskView<'_>,
    expected: &Model,
    what: &str,
) -> Result<()> {
    assert_eq!(count(column, rows)?, expected.count, "count {what}");
    assert_eq!(sum(column, rows)?, expected.sum, "sum {what}");
    assert_eq!(min(column, rows)?, expected.min, "min {what}");
    assert_eq!(max(column, rows)?, expected.max, "max {what}");
    Ok(())
}

#[test]
fn aggregates_match_the_model_on_random_data() -> Result<()> {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    for nrows in [0, 1, 63, 64, 65, 130, 1024] {
        let values: Vec<i32> = (0..nrows)
            .map(|_| {
                let draw = random(&mut state);
                if draw.is_multiple_of(4) {
                    VALUES[(draw >> 8) as usize % VALUES.len()]
                } else {
                    (draw >> 8) as i32 % 1000
                }
            })
            .collect();
        let non_null: Vec<bool> = (0..nrows)
            .map(|_| !random(&mut state).is_multiple_of(3))
            .collect();
        let non_null_words = words_for(&non_null);
        for masked in [false, true] {
            let non_nulls = masked.then(|| RowMaskView::try_new(nrows, &non_null_words).unwrap());
            let column = ColumnView::try_new(&values, non_nulls)?;
            let all_present = vec![true; nrows];
            let flags = if masked { &non_null } else { &all_present };
            for density in [1, 2, 8, 64] {
                let selected: Vec<bool> = (0..nrows)
                    .map(|_| random(&mut state).is_multiple_of(density))
                    .collect();
                let words = words_for(&selected);
                let rows = RowMaskView::try_new(nrows, &words)?;
                let expected = model(&values, &selected, flags);
                assert_aggregates(
                    &column,
                    &rows,
                    &expected,
                    &format!("{nrows} rows, 1/{density}, masked {masked}"),
                )?;
            }
        }
    }
    Ok(())
}

#[test]
fn empty_and_all_null_selections_give_nothing_and_extremes_do_not_overflow() -> Result<()> {
    let values = [i32::MIN; 130];
    let column = ColumnView::try_new(&values, None)?;
    let none = RowMaskView::try_new(130, &[0, 0, 0])?;
    assert_eq!(count(&column, &none)?, 0);
    assert_eq!(sum(&column, &none)?, None);
    assert_eq!(min(&column, &none)?, None);
    assert_eq!(max(&column, &none)?, None);
    let all = RowMaskView::try_new(130, &[u64::MAX, u64::MAX, 3])?;
    assert_eq!(count(&column, &all)?, 130);
    assert_eq!(sum(&column, &all)?, Some(130 * i64::from(i32::MIN)));
    assert_eq!(min(&column, &all)?, Some(i32::MIN));
    assert_eq!(max(&column, &all)?, Some(i32::MIN));
    // Every selected row NULL: nothing, and the values are never read.
    let nulls = RowMaskView::try_new(130, &[0, 0, 0])?;
    let column = ColumnView::try_new(&values, Some(nulls))?;
    assert_eq!(count(&column, &all)?, 0);
    assert_eq!(sum(&column, &all)?, None);
    assert_eq!(min(&column, &all)?, None);
    assert_eq!(max(&column, &all)?, None);
    Ok(())
}

#[test]
fn row_count_mismatches_are_errors() -> Result<()> {
    let values = [1; 64];
    let column = ColumnView::try_new(&values, None)?;
    let rows = RowMaskView::try_new(65, &[u64::MAX, 1])?;
    assert!(count(&column, &rows).is_err());
    assert!(sum(&column, &rows).is_err());
    assert!(min(&column, &rows).is_err());
    assert!(max(&column, &rows).is_err());
    Ok(())
}

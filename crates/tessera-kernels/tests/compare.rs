#![forbid(unsafe_code)]

use anyhow::Result;
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView};
use tessera_kernels::{int32, int64};

const OPS: [int32::CompareOp; 6] = [
    int32::CompareOp::Eq,
    int32::CompareOp::Ne,
    int32::CompareOp::Lt,
    int32::CompareOp::Le,
    int32::CompareOp::Gt,
    int32::CompareOp::Ge,
];

fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn words_for(flags: &[bool]) -> Vec<u64> {
    let mut words = vec![0; flags.len().div_ceil(64)];
    for (row, _) in flags.iter().enumerate().filter(|(_, flag)| **flag) {
        words[row / 64] |= 1 << (row % 64);
    }
    words
}

fn model<T: Ord + Copy>(op: int32::CompareOp, a: T, b: T) -> bool {
    match op {
        int32::CompareOp::Eq => a == b,
        int32::CompareOp::Ne => a != b,
        int32::CompareOp::Lt => a < b,
        int32::CompareOp::Le => a <= b,
        int32::CompareOp::Gt => a > b,
        int32::CompareOp::Ge => a >= b,
    }
}

/// The same values without bulk storage: every word takes the row path.
struct RowsOnly<'a, T>(&'a ColumnView<'a, T>);

impl<T: Copy + std::fmt::Debug> ColumnReader for RowsOnly<'_, T>
where
    for<'b> ColumnView<'b, T>: ColumnReader<Value = T>,
{
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

/// Two columns, their non-NULL flags and a selection, row by row.
struct Fixture {
    left: Vec<i64>,
    right: Vec<i64>,
    left_non_null: Vec<bool>,
    right_non_null: Vec<bool>,
    selected: Vec<bool>,
}

/// Two columns of `nrows` values from a small range, so that every
/// operator both keeps and drops rows, NULLs in each, and a selection whose
/// first word is full, then words from dense to sparse, single and empty.
fn fixture(state: &mut u64, nrows: usize) -> Fixture {
    let left = (0..nrows).map(|_| (random(state) % 7) as i64 - 3).collect();
    let right = (0..nrows).map(|_| (random(state) % 7) as i64 - 3).collect();
    let left_non_null = (0..nrows)
        .map(|_| !random(state).is_multiple_of(5))
        .collect();
    let right_non_null = (0..nrows)
        .map(|_| !random(state).is_multiple_of(6))
        .collect();
    let selected = (0..nrows)
        .map(|row| match row / 64 {
            0 => true,
            1 => random(state).is_multiple_of(2),
            2 => row % 64 == 5,
            3 => false,
            _ => !row.is_multiple_of(3),
        })
        .collect();
    Fixture {
        left,
        right,
        left_non_null,
        right_non_null,
        selected,
    }
}

#[test]
fn comparisons_match_the_model_and_the_row_path() -> Result<()> {
    let mut state = 0x9e37_79b9_7f4a_7c15;
    for nrows in [0, 1, 63, 64, 65, 300] {
        let Fixture {
            left,
            right,
            left_non_null: left_nn,
            right_non_null: right_nn,
            selected,
        } = fixture(&mut state, nrows);
        let (left_words, right_words) = (words_for(&left_nn), words_for(&right_nn));
        let left_view = Some(RowMaskView::try_new(nrows, &left_words)?);
        let right_view = Some(RowMaskView::try_new(nrows, &right_words)?);
        let left32: Vec<i32> = left.iter().map(|&v| v as i32).collect();
        let right32: Vec<i32> = right.iter().map(|&v| v as i32).collect();
        // int8 values past the int4 range, in the same order.
        let left64: Vec<i64> = left.iter().map(|&v| v << 40).collect();
        let right64: Vec<i64> = right.iter().map(|&v| v << 40).collect();
        let columns32 = (
            ColumnView::try_new(&left32, left_view)?,
            ColumnView::try_new(&right32, right_view)?,
        );
        let columns64 = (
            ColumnView::try_new(&left64, left_view)?,
            ColumnView::try_new(&right64, right_view)?,
        );
        for op in OPS {
            let expected: Vec<bool> = (0..nrows)
                .map(|row| {
                    selected[row]
                        && left_nn[row]
                        && right_nn[row]
                        && model(op, left[row], right[row])
                })
                .collect();
            let expected = words_for(&expected);
            let mut words = words_for(&selected);
            int32::compare_columns(
                &columns32.0,
                &columns32.1,
                &mut RowMask::try_new(nrows, &mut words)?,
                op,
            )?;
            assert_eq!(words, expected, "int4 {op:?} {nrows}");
            let mut words = words_for(&selected);
            int32::compare_columns(
                &RowsOnly(&columns32.0),
                &RowsOnly(&columns32.1),
                &mut RowMask::try_new(nrows, &mut words)?,
                op,
            )?;
            assert_eq!(words, expected, "int4 rows {op:?} {nrows}");
            let mut words = words_for(&selected);
            int64::compare_columns(
                &columns64.0,
                &columns64.1,
                &mut RowMask::try_new(nrows, &mut words)?,
                op,
            )?;
            assert_eq!(words, expected, "int8 {op:?} {nrows}");
            let mut words = words_for(&selected);
            int64::compare_columns(
                &RowsOnly(&columns64.0),
                &RowsOnly(&columns64.1),
                &mut RowMask::try_new(nrows, &mut words)?,
                op,
            )?;
            assert_eq!(words, expected, "int8 rows {op:?} {nrows}");
        }
    }
    Ok(())
}

#[test]
fn row_counts_must_agree_before_any_change() -> Result<()> {
    let (a, b) = ([1, 2, 3], [1, 2]);
    let left = ColumnView::try_new(&a, None)?;
    let right = ColumnView::try_new(&b, None)?;
    let mut words = [0b111];
    let error = int32::compare_columns(
        &left,
        &right,
        &mut RowMask::try_new(3, &mut words)?,
        int32::CompareOp::Eq,
    )
    .unwrap_err();
    assert!(error.to_string().contains("row counts"), "{error}");
    assert_eq!(words, [0b111]);
    Ok(())
}

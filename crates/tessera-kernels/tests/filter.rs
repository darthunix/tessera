#![forbid(unsafe_code)]

use std::cell::Cell;

use anyhow::Result;
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView, WordValues};
use tessera_kernels::int32::{CompareOp, filter};

const OPS: [CompareOp; 6] = [
    CompareOp::Eq,
    CompareOp::Ne,
    CompareOp::Lt,
    CompareOp::Le,
    CompareOp::Gt,
    CompareOp::Ge,
];
const VALUES: [i32; 7] = [i32::MIN, -42, -1, 0, 1, 42, i32::MAX];

fn compare(value: i32, op: CompareOp, scalar: i32) -> bool {
    match op {
        CompareOp::Eq => value == scalar,
        CompareOp::Ne => value != scalar,
        CompareOp::Lt => value < scalar,
        CompareOp::Le => value <= scalar,
        CompareOp::Gt => value > scalar,
        CompareOp::Ge => value >= scalar,
    }
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

#[test]
fn comparisons_match_scalar_model() {
    for nrows in [0, 1, 63, 64, 65, 1024] {
        let values: Vec<_> = (0..nrows).map(|row| VALUES[row % VALUES.len()]).collect();
        for null_kind in 0..3 {
            let non_null: Vec<_> = (0..nrows)
                .map(|row| null_kind == 0 || (null_kind == 1 && row % 5 != 0))
                .collect();
            let non_null_words = words_for(&non_null);
            let non_nulls =
                (null_kind != 0).then(|| RowMaskView::try_new(nrows, &non_null_words).unwrap());
            let column = ColumnView::try_new(&values, non_nulls).unwrap();
            for selection_kind in 0..4 {
                let selected: Vec<_> = (0..nrows)
                    .map(|row| match selection_kind {
                        0 => false,
                        1 => true,
                        2 => row % 5 == 2,
                        _ => row % 64 == 0 || row % 64 == 63,
                    })
                    .collect();
                for op in OPS {
                    for scalar in VALUES {
                        let expected: Vec<_> = (0..nrows)
                            .map(|row| {
                                selected[row] && non_null[row] && compare(values[row], op, scalar)
                            })
                            .collect();
                        let mut words = words_for(&selected);
                        let mut rows = RowMask::try_new(nrows, &mut words).unwrap();
                        filter(&column, &mut rows, op, scalar).unwrap();
                        // Repeating a filter must not restore rows or change padding.
                        filter(&column, &mut rows, op, scalar).unwrap();
                        assert_eq!(words, words_for(&expected), "{nrows} {op:?} {scalar}");
                        RowMaskView::try_new(nrows, &words).unwrap();
                    }
                }
            }
        }
    }
}

#[test]
fn successive_filters_keep_only_the_intersection() {
    let values: Vec<_> = (0..65).collect();
    let column = ColumnView::try_new(&values, None).unwrap();
    let mut words = [!(1 << 15), 1];
    let mut rows = RowMask::try_new(65, &mut words).unwrap();
    filter(&column, &mut rows, CompareOp::Ge, 10).unwrap();
    filter(&column, &mut rows, CompareOp::Lt, 20).unwrap();
    assert_eq!(
        rows.as_view().selected_indices().collect::<Vec<_>>(),
        [10, 11, 12, 13, 14, 16, 17, 18, 19]
    );
    filter(&column, &mut rows, CompareOp::Gt, 40).unwrap();
    filter(&column, &mut rows, CompareOp::Ge, 0).unwrap();
    assert_eq!(words, [0, 0]);
}

struct Tracked<'a> {
    nrows: usize,
    prepared: &'a [u64],
    words: Cell<usize>,
    reads: Cell<usize>,
}

impl ColumnReader for Tracked<'_> {
    type Value = i32;

    fn nrows(&self) -> usize {
        self.nrows
    }

    fn get(&self, _: usize) -> Result<Option<i32>> {
        panic!("filter must use word_values, not get")
    }

    fn word_values(
        &self,
        index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<i32>)> + '_> {
        self.words.set(self.words.get() + 1);
        WordValues::try_new(self.nrows, index, selected, self.prepared[index], |row| {
            self.reads.set(self.reads.get() + 1);
            Some(row as i32)
        })
    }
}

#[test]
fn reader_error_keeps_current_and_later_words_without_rollback() {
    for invalid_word in 0..3 {
        let mut prepared = [u64::MAX; 3];
        prepared[invalid_word] &= !(1 << 10);
        let column = Tracked {
            nrows: 192,
            prepared: &prepared,
            words: Cell::new(0),
            reads: Cell::new(0),
        };
        let mut words = [u64::MAX; 3];
        let mut rows = RowMask::try_new(192, &mut words).unwrap();
        assert!(filter(&column, &mut rows, CompareOp::Lt, 10).is_err());
        let mut expected = [u64::MAX; 3];
        for (index, word) in expected.iter_mut().enumerate().take(invalid_word) {
            *word = if index == 0 { (1 << 10) - 1 } else { 0 };
        }
        assert_eq!(words, expected);
        assert_eq!(column.words.get(), invalid_word + 1);
        assert_eq!(column.reads.get(), invalid_word * 64);
    }
}

#[test]
fn empty_words_and_unselected_unprepared_rows_are_not_read() {
    let column = Tracked {
        nrows: 192,
        prepared: &[0, 1, 0],
        words: Cell::new(0),
        reads: Cell::new(0),
    };
    let mut words = [0, 1, 0];
    let mut rows = RowMask::try_new(192, &mut words).unwrap();
    filter(&column, &mut rows, CompareOp::Eq, 64).unwrap();
    assert_eq!(column.words.get(), 1);
    assert_eq!(column.reads.get(), 1);
    rows.clear(64);
    filter(&column, &mut rows, CompareOp::Eq, 64).unwrap();
    assert_eq!(column.words.get(), 1);
    assert_eq!(column.reads.get(), 1);
    assert_eq!(words, [0; 3]);
}

#[test]
fn dimension_errors_never_read_or_mutate() {
    let column = Tracked {
        nrows: 65,
        prepared: &[],
        words: Cell::new(0),
        reads: Cell::new(0),
    };
    for nrows in [0, 1, 64, 66, 128] {
        for selected in [false, true] {
            for op in OPS {
                let original = words_for(&vec![selected; nrows]);
                let mut words = original.clone();
                let mut rows = RowMask::try_new(nrows, &mut words).unwrap();
                assert!(filter(&column, &mut rows, op, 0).is_err());
                assert_eq!(words, original);
            }
        }
    }
    assert_eq!(column.words.get(), 0);
    assert_eq!(column.reads.get(), 0);
}

#![forbid(unsafe_code)]

use std::cell::Cell;

use anyhow::{Result, ensure};
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

/// The same values without bulk storage: every word takes the row path.
struct RowsOnly<'a>(ColumnView<'a, i32>);

impl ColumnReader for RowsOnly<'_> {
    type Value = i32;
    fn nrows(&self) -> usize {
        self.0.nrows()
    }
    fn get(&self, row: usize) -> Result<Option<i32>> {
        ColumnReader::get(&self.0, row)
    }
    fn word_values(
        &self,
        word_index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<i32>)> + '_> {
        self.0.word_values(word_index, selected)
    }
}

/// xorshift64*, fixed seed: the same data on every run.
fn random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

#[test]
fn bulk_words_agree_with_the_row_path_on_random_data() {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    let nrows = 4 * 64 + 17;
    let values: Vec<i32> = (0..nrows)
        .map(|_| {
            let draw = random(&mut state);
            if draw.is_multiple_of(4) {
                VALUES[(draw >> 8) as usize % VALUES.len()]
            } else {
                (draw >> 8) as i32 % 100
            }
        })
        .collect();
    let non_null: Vec<bool> = (0..nrows)
        .map(|_| !random(&mut state).is_multiple_of(4))
        .collect();
    let non_null_words = words_for(&non_null);
    for masked in [false, true] {
        let non_nulls = masked.then(|| RowMaskView::try_new(nrows, &non_null_words).unwrap());
        let column = ColumnView::try_new(&values, non_nulls).unwrap();
        let rows_only = RowsOnly(ColumnView::try_new(&values, non_nulls).unwrap());
        for op in OPS {
            for (scalar, density) in [
                (i32::MIN, 2),
                (-100, 8),
                (-99, 2),
                (-1, 16),
                (0, 2),
                (1, 3),
                (42, 2),
                (98, 8),
                (99, 2),
                (i32::MAX, 5),
            ] {
                // Dense selections take the whole-word loop, sparse ones the rows.
                let selected: Vec<bool> = (0..nrows)
                    .map(|_| random(&mut state).is_multiple_of(density))
                    .collect();
                let mut bulk = words_for(&selected);
                let mut rows = words_for(&selected);
                filter(
                    &column,
                    &mut RowMask::try_new(nrows, &mut bulk).unwrap(),
                    op,
                    scalar,
                )
                .unwrap();
                filter(
                    &rows_only,
                    &mut RowMask::try_new(nrows, &mut rows).unwrap(),
                    op,
                    scalar,
                )
                .unwrap();
                assert_eq!(bulk, rows, "{op:?} {scalar} masked={masked}");
                let expected: Vec<_> = (0..nrows)
                    .map(|row| {
                        selected[row]
                            && (!masked || non_null[row])
                            && compare(values[row], op, scalar)
                    })
                    .collect();
                assert_eq!(
                    bulk,
                    words_for(&expected),
                    "{op:?} {scalar} masked={masked}"
                );
            }
        }
    }
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
            for selection_kind in 0..6 {
                let selected: Vec<_> = (0..nrows)
                    .map(|row| match selection_kind {
                        0 => false,
                        1 => true,
                        2 => row % 5 == 2,
                        3 => row % 64 == 0 || row % 64 == 63,
                        4 => row % 64 == 0,
                        _ => row % 64 == 63,
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
    gets: Cell<usize>,
    reads: Cell<usize>,
}

impl ColumnReader for Tracked<'_> {
    type Value = i32;

    fn nrows(&self) -> usize {
        self.nrows
    }

    fn get(&self, row: usize) -> Result<Option<i32>> {
        self.gets.set(self.gets.get() + 1);
        ensure!(row < self.nrows, "row is out of bounds");
        ensure!(
            self.prepared[row / 64] & (1 << (row % 64)) != 0,
            "row is not prepared"
        );
        self.reads.set(self.reads.get() + 1);
        Ok(Some(row as i32))
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
    for (selected, reads_per_word) in [(u64::MAX, 64), (1 << 10, 1)] {
        for invalid_word in 0..3 {
            let mut prepared = [u64::MAX; 3];
            prepared[invalid_word] &= !(1 << 10);
            let column = Tracked {
                nrows: 192,
                prepared: &prepared,
                words: Cell::new(0),
                gets: Cell::new(0),
                reads: Cell::new(0),
            };
            let mut words = [selected; 3];
            let mut rows = RowMask::try_new(192, &mut words).unwrap();
            assert!(filter(&column, &mut rows, CompareOp::Lt, 10).is_err());
            let mut expected = [selected; 3];
            for (index, word) in expected.iter_mut().enumerate().take(invalid_word) {
                *word &= if index == 0 { (1 << 10) - 1 } else { 0 };
            }
            assert_eq!(words, expected);
            let singleton = selected.is_power_of_two();
            assert_eq!(
                column.words.get(),
                if singleton { 0 } else { invalid_word + 1 }
            );
            assert_eq!(
                column.gets.get(),
                if singleton { invalid_word + 1 } else { 0 }
            );
            assert_eq!(column.reads.get(), invalid_word * reads_per_word);
        }
    }
}

#[test]
fn empty_words_and_unselected_unprepared_rows_are_not_read() {
    let column = Tracked {
        nrows: 192,
        prepared: &[0, 1, 0],
        words: Cell::new(0),
        gets: Cell::new(0),
        reads: Cell::new(0),
    };
    let mut words = [0, 1, 0];
    let mut rows = RowMask::try_new(192, &mut words).unwrap();
    filter(&column, &mut rows, CompareOp::Eq, 64).unwrap();
    assert_eq!(column.words.get(), 0);
    assert_eq!(column.gets.get(), 1);
    assert_eq!(column.reads.get(), 1);
    rows.clear(64);
    filter(&column, &mut rows, CompareOp::Eq, 64).unwrap();
    assert_eq!(column.words.get(), 0);
    assert_eq!(column.gets.get(), 1);
    assert_eq!(column.reads.get(), 1);
    assert_eq!(words, [0; 3]);
}

#[test]
fn dimension_errors_never_read_or_mutate() {
    let column = Tracked {
        nrows: 65,
        prepared: &[],
        words: Cell::new(0),
        gets: Cell::new(0),
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
    assert_eq!(column.gets.get(), 0);
    assert_eq!(column.reads.get(), 0);
}

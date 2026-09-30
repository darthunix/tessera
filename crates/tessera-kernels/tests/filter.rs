#![forbid(unsafe_code)]

use std::cell::Cell;
use std::fmt::{Debug, Display};
use std::marker::PhantomData;

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView, WordValues};
use tessera_kernels::{int32, int64, ops::CompareOp};

const OPS: [CompareOp; 6] = [
    CompareOp::Eq,
    CompareOp::Ne,
    CompareOp::Lt,
    CompareOp::Le,
    CompareOp::Gt,
    CompareOp::Ge,
];

/// One integer width under test: its values and its filter kernel. Every
/// check below runs once for int4 (`i32`) and once for int8 (`i64`).
trait Width: Copy + Ord + Debug + Display + From<i32> + 'static {
    /// Column values and scalars of the model sweep.
    const VALUES: &'static [Self];
    /// Scalars of the random test, each with its selection density.
    const SCALARS: [(Self, u64); 10];

    /// A random column value from a draw that does not pick from `VALUES`.
    fn from_draw(draw: u64) -> Self;

    /// `small` for int4; `small << 33` for int8, past the int4 range in the
    /// same order.
    fn spread(small: i32) -> Self;

    fn filter<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &mut RowMask<'_>,
        op: CompareOp,
        scalar: Self,
    ) -> Result<()>;
}

impl Width for i32 {
    const VALUES: &'static [i32] = &[i32::MIN, -42, -1, 0, 1, 42, i32::MAX];
    const SCALARS: [(i32, u64); 10] = [
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
    ];

    fn from_draw(draw: u64) -> i32 {
        (draw >> 8) as i32 % 100
    }

    fn spread(small: i32) -> i32 {
        small
    }

    fn filter<C: ColumnReader<Value = i32>>(
        column: &C,
        rows: &mut RowMask<'_>,
        op: CompareOp,
        scalar: i32,
    ) -> Result<()> {
        int32::filter(column, rows, op, scalar)
    }
}

impl Width for i64 {
    /// Values on both sides of the int4 range, so that a 32-bit read would
    /// compare wrongly.
    const VALUES: &'static [i64] = &[i64::MIN, -(1 << 40), -42, -1, 0, 1, 42, 1 << 40, i64::MAX];
    const SCALARS: [(i64, u64); 10] = [
        (i64::MIN, 2),
        (-100 << 32, 8),
        (-99, 2),
        (-1, 16),
        (0, 2),
        (1, 3),
        (42, 2),
        (98 << 32, 8),
        (99, 2),
        (i64::MAX, 5),
    ];

    fn from_draw(draw: u64) -> i64 {
        // Around zero, and around the int4 boundary.
        let small = (draw >> 8) as i64 % 100;
        if draw & 8 == 0 {
            small
        } else {
            small * (1 << 32)
        }
    }

    fn spread(small: i32) -> i64 {
        i64::from(small) << 33
    }

    fn filter<C: ColumnReader<Value = i64>>(
        column: &C,
        rows: &mut RowMask<'_>,
        op: CompareOp,
        scalar: i64,
    ) -> Result<()> {
        int64::filter(column, rows, op, scalar)
    }
}

fn compare<W: Width>(value: W, op: CompareOp, scalar: W) -> bool {
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
struct RowsOnly<'a, W>(ColumnView<'a, W>);

impl<W: Copy> ColumnReader for RowsOnly<'_, W> {
    type Value = W;
    fn nrows(&self) -> usize {
        self.0.nrows()
    }
    fn get(&self, row: usize) -> Result<Option<W>> {
        ColumnReader::get(&self.0, row)
    }
    fn word_values(
        &self,
        word_index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<W>)> + '_> {
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

fn bulk_words_agree_with_the_row_path_on_random_data<W: Width>() {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    let nrows = 4 * 64 + 17;
    let values: Vec<W> = (0..nrows)
        .map(|_| {
            let draw = random(&mut state);
            if draw.is_multiple_of(4) {
                W::VALUES[(draw >> 8) as usize % W::VALUES.len()]
            } else {
                W::from_draw(draw)
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
            for (scalar, density) in W::SCALARS {
                // Dense selections take the whole-word loop, sparse ones the rows.
                let selected: Vec<bool> = (0..nrows)
                    .map(|_| random(&mut state).is_multiple_of(density))
                    .collect();
                let mut bulk = words_for(&selected);
                let mut rows = words_for(&selected);
                W::filter(
                    &column,
                    &mut RowMask::try_new(nrows, &mut bulk).unwrap(),
                    op,
                    scalar,
                )
                .unwrap();
                W::filter(
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
fn bulk_words_agree_with_the_row_path_on_random_data_int4() {
    bulk_words_agree_with_the_row_path_on_random_data::<i32>();
}

#[test]
fn bulk_words_agree_with_the_row_path_on_random_data_int8() {
    bulk_words_agree_with_the_row_path_on_random_data::<i64>();
}

fn comparisons_match_scalar_model<W: Width>() {
    for nrows in [0, 1, 63, 64, 65, 1024] {
        let values: Vec<W> = (0..nrows)
            .map(|row| W::VALUES[row % W::VALUES.len()])
            .collect();
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
                    for &scalar in W::VALUES {
                        let expected: Vec<_> = (0..nrows)
                            .map(|row| {
                                selected[row] && non_null[row] && compare(values[row], op, scalar)
                            })
                            .collect();
                        let mut words = words_for(&selected);
                        let mut rows = RowMask::try_new(nrows, &mut words).unwrap();
                        W::filter(&column, &mut rows, op, scalar).unwrap();
                        // Repeating a filter must not restore rows or change padding.
                        W::filter(&column, &mut rows, op, scalar).unwrap();
                        assert_eq!(words, words_for(&expected), "{nrows} {op:?} {scalar}");
                        RowMaskView::try_new(nrows, &words).unwrap();
                    }
                }
            }
        }
    }
}

#[test]
fn comparisons_match_scalar_model_int4() {
    comparisons_match_scalar_model::<i32>();
}

#[test]
fn comparisons_match_scalar_model_int8() {
    comparisons_match_scalar_model::<i64>();
}

fn successive_filters_keep_only_the_intersection<W: Width>() {
    let values: Vec<W> = (0..65).map(W::spread).collect();
    let column = ColumnView::try_new(&values, None).unwrap();
    let mut words = [!(1 << 15), 1];
    let mut rows = RowMask::try_new(65, &mut words).unwrap();
    W::filter(&column, &mut rows, CompareOp::Ge, W::spread(10)).unwrap();
    W::filter(&column, &mut rows, CompareOp::Lt, W::spread(20)).unwrap();
    assert_eq!(
        rows.as_view().selected_indices().collect::<Vec<_>>(),
        [10, 11, 12, 13, 14, 16, 17, 18, 19]
    );
    W::filter(&column, &mut rows, CompareOp::Gt, W::spread(40)).unwrap();
    W::filter(&column, &mut rows, CompareOp::Ge, W::from(0)).unwrap();
    assert_eq!(words, [0, 0]);
}

#[test]
fn successive_filters_keep_only_the_intersection_int4() {
    successive_filters_keep_only_the_intersection::<i32>();
}

#[test]
fn successive_filters_keep_only_the_intersection_int8() {
    successive_filters_keep_only_the_intersection::<i64>();
}

struct Tracked<'a, W> {
    nrows: usize,
    prepared: &'a [u64],
    words: Cell<usize>,
    gets: Cell<usize>,
    reads: Cell<usize>,
    width: PhantomData<W>,
}

impl<'a, W> Tracked<'a, W> {
    fn new(nrows: usize, prepared: &'a [u64]) -> Self {
        Self {
            nrows,
            prepared,
            words: Cell::new(0),
            gets: Cell::new(0),
            reads: Cell::new(0),
            width: PhantomData,
        }
    }
}

impl<W: Width> ColumnReader for Tracked<'_, W> {
    type Value = W;

    fn nrows(&self) -> usize {
        self.nrows
    }

    fn get(&self, row: usize) -> Result<Option<W>> {
        self.gets.set(self.gets.get() + 1);
        ensure!(row < self.nrows, "row is out of bounds");
        ensure!(
            self.prepared[row / 64] & (1 << (row % 64)) != 0,
            "row is not prepared"
        );
        self.reads.set(self.reads.get() + 1);
        Ok(Some(W::from(row as i32)))
    }

    fn word_values(
        &self,
        index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<W>)> + '_> {
        self.words.set(self.words.get() + 1);
        WordValues::try_new(self.nrows, index, selected, self.prepared[index], |row| {
            self.reads.set(self.reads.get() + 1);
            Some(W::from(row as i32))
        })
    }
}

fn reader_error_keeps_current_and_later_words_without_rollback<W: Width>() {
    for (selected, reads_per_word) in [(u64::MAX, 64), (1 << 10, 1)] {
        for invalid_word in 0..3 {
            let mut prepared = [u64::MAX; 3];
            prepared[invalid_word] &= !(1 << 10);
            let column = Tracked::<W>::new(192, &prepared);
            let mut words = [selected; 3];
            let mut rows = RowMask::try_new(192, &mut words).unwrap();
            assert!(W::filter(&column, &mut rows, CompareOp::Lt, W::from(10)).is_err());
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
fn reader_error_keeps_current_and_later_words_without_rollback_int4() {
    reader_error_keeps_current_and_later_words_without_rollback::<i32>();
}

#[test]
fn reader_error_keeps_current_and_later_words_without_rollback_int8() {
    reader_error_keeps_current_and_later_words_without_rollback::<i64>();
}

fn empty_words_and_unselected_unprepared_rows_are_not_read<W: Width>() {
    let column = Tracked::<W>::new(192, &[0, 1, 0]);
    let mut words = [0, 1, 0];
    let mut rows = RowMask::try_new(192, &mut words).unwrap();
    W::filter(&column, &mut rows, CompareOp::Eq, W::from(64)).unwrap();
    assert_eq!(column.words.get(), 0);
    assert_eq!(column.gets.get(), 1);
    assert_eq!(column.reads.get(), 1);
    rows.clear(64);
    W::filter(&column, &mut rows, CompareOp::Eq, W::from(64)).unwrap();
    assert_eq!(column.words.get(), 0);
    assert_eq!(column.gets.get(), 1);
    assert_eq!(column.reads.get(), 1);
    assert_eq!(words, [0; 3]);
}

#[test]
fn empty_words_and_unselected_unprepared_rows_are_not_read_int4() {
    empty_words_and_unselected_unprepared_rows_are_not_read::<i32>();
}

#[test]
fn empty_words_and_unselected_unprepared_rows_are_not_read_int8() {
    empty_words_and_unselected_unprepared_rows_are_not_read::<i64>();
}

fn dimension_errors_never_read_or_mutate<W: Width>() {
    let column = Tracked::<W>::new(65, &[]);
    for nrows in [0, 1, 64, 66, 128] {
        for selected in [false, true] {
            for op in OPS {
                let original = words_for(&vec![selected; nrows]);
                let mut words = original.clone();
                let mut rows = RowMask::try_new(nrows, &mut words).unwrap();
                assert!(W::filter(&column, &mut rows, op, W::from(0)).is_err());
                assert_eq!(words, original);
            }
        }
    }
    assert_eq!(column.words.get(), 0);
    assert_eq!(column.gets.get(), 0);
    assert_eq!(column.reads.get(), 0);
}

#[test]
fn dimension_errors_never_read_or_mutate_int4() {
    dimension_errors_never_read_or_mutate::<i32>();
}

#[test]
fn dimension_errors_never_read_or_mutate_int8() {
    dimension_errors_never_read_or_mutate::<i64>();
}

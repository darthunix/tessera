#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]
#![forbid(unsafe_code)]

use std::cell::Cell;
use std::fmt::Display;
use std::marker::PhantomData;

use anyhow::{Result, ensure};
use proptest::prelude::*;
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView, WordValues};
use tessera_kernels::{int32, int64, ops::CompareOp};
use tessera_testing::{Int, edge, flags, integer, nrows, property, values, words};

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
trait Width: Int + Ord + Display + From<i32> {
    /// Column values and scalars of the model sweep.
    const VALUES: &'static [Self];

    /// Values close to each other, so that every operator both keeps and
    /// drops rows: -3 to 3, and for int8 also the same with a high half, so
    /// that a 32-bit comparison would call them equal.
    fn near() -> BoxedStrategy<Self>;

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

    fn near() -> BoxedStrategy<Self> {
        (-3..=3).boxed()
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

    fn near() -> BoxedStrategy<Self> {
        prop_oneof![-3_i64..=3, (-3_i64..=3).prop_map(|value| value << 32)].boxed()
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

/// A column, its non-NULL flags (none when `masked` is false: the unmasked
/// kernels), a selection and a scalar; the rows a filter reads and the
/// scalar hold close values, edges and any values, the other rows edges.
#[derive(Clone, Debug)]
struct Batch<W> {
    values: Vec<W>,
    non_null: Vec<bool>,
    masked: bool,
    selected: Vec<bool>,
    scalar: W,
}

fn comparable<W: Width>() -> BoxedStrategy<W> {
    prop_oneof![1 => edge::<W>(), 2 => W::near(), 1 => integer::<W>()].boxed()
}

fn batches<W: Width>() -> impl Strategy<Value = Batch<W>> {
    nrows()
        .prop_flat_map(|nrows| (flags(nrows), flags(nrows), any::<bool>()))
        .prop_flat_map(|(selected, non_null, masked)| {
            let non_null = if masked {
                non_null
            } else {
                vec![true; selected.len()]
            };
            let read: Vec<bool> = selected
                .iter()
                .zip(&non_null)
                .map(|(&s, &n)| s && n)
                .collect();
            (
                values(&read, &comparable::<W>()),
                comparable::<W>(),
                Just((non_null, masked, selected.clone())),
            )
        })
        .prop_map(|(values, scalar, (non_null, masked, selected))| Batch {
            values,
            non_null,
            masked,
            selected,
            scalar,
        })
}

/// Every operator keeps exactly the selected non-NULL rows where the model
/// holds, on the whole-word and the row path.
fn filters_match_the_model_and_the_row_path<W: Width>() {
    property(batches::<W>(), |batch| -> Result<()> {
        let nrows = batch.selected.len();
        let non_null_words = words(&batch.non_null);
        let non_nulls = batch
            .masked
            .then(|| RowMaskView::try_new(nrows, &non_null_words))
            .transpose()?;
        let column = ColumnView::try_new(&batch.values, non_nulls)?;
        let rows_only = RowsOnly(ColumnView::try_new(&batch.values, non_nulls)?);
        for op in OPS {
            let mut whole = words(&batch.selected);
            W::filter(
                &column,
                &mut RowMask::try_new(nrows, &mut whole)?,
                op,
                batch.scalar,
            )?;
            let mut by_rows = words(&batch.selected);
            W::filter(
                &rows_only,
                &mut RowMask::try_new(nrows, &mut by_rows)?,
                op,
                batch.scalar,
            )?;
            let expected: Vec<bool> = (0..nrows)
                .map(|row| {
                    batch.selected[row]
                        && batch.non_null[row]
                        && compare(batch.values[row], op, batch.scalar)
                })
                .collect();
            assert_eq!(whole, words(&expected), "{op:?}");
            assert_eq!(by_rows, whole, "rows {op:?}");
        }
        Ok(())
    });
}

#[test]
fn filters_match_the_model_and_the_row_path_int4() {
    filters_match_the_model_and_the_row_path::<i32>();
}

#[test]
fn filters_match_the_model_and_the_row_path_int8() {
    filters_match_the_model_and_the_row_path::<i64>();
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
            let non_null_words = words(&non_null);
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
                        let mut kept = words(&selected);
                        let mut rows = RowMask::try_new(nrows, &mut kept).unwrap();
                        W::filter(&column, &mut rows, op, scalar).unwrap();
                        // Repeating a filter must not restore rows or change padding.
                        W::filter(&column, &mut rows, op, scalar).unwrap();
                        assert_eq!(kept, words(&expected), "{nrows} {op:?} {scalar}");
                        RowMaskView::try_new(nrows, &kept).unwrap();
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
                let original = words(&vec![selected; nrows]);
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

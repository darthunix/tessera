#![forbid(unsafe_code)]

use anyhow::Result;
use proptest::prelude::*;
use tessera_core::{ColumnReader, ColumnView, RowMaskView};
use tessera_kernels::count::count;
use tessera_kernels::{int32, int64};
use tessera_testing::{Int, flags, integer, nrows, property, values, words};

/// The same values without bulk storage: every call takes the row path.
struct RowsOnly<'a, T>(ColumnView<'a, T>);

impl<T: Copy> ColumnReader for RowsOnly<'_, T> {
    type Value = T;
    fn nrows(&self) -> usize {
        self.0.nrows()
    }
    fn get(&self, row: usize) -> Result<Option<T>> {
        ColumnReader::get(&self.0, row)
    }
    fn word_values(
        &self,
        word_index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<T>)> + '_> {
        self.0.word_values(word_index, selected)
    }
}

/// One width under test, int4 or int8, and its kernels.
trait Lane: Int + Ord + From<i32> {
    /// The type in failure messages.
    const NAME: &'static str;
    const MIN: Self;
    /// Whether the width has a sum kernel: int4 sums into int8, int8 sums
    /// into numeric elsewhere.
    const SUMS: bool;
    /// A value as the sum adds it.
    fn widen(self) -> i64;
    /// The width's `min` kernel.
    fn least<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Self>>;
    /// The width's `max` kernel.
    fn greatest<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Self>>;
    /// The sum kernel's result, `None` without a sum kernel.
    fn sum<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Option<i64>>>;
}

impl Lane for i32 {
    const NAME: &'static str = "int4";
    const MIN: Self = i32::MIN;
    const SUMS: bool = true;
    fn widen(self) -> i64 {
        i64::from(self)
    }
    fn least<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Self>> {
        int32::min(column, rows)
    }
    fn greatest<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Self>> {
        int32::max(column, rows)
    }
    fn sum<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Option<i64>>> {
        int32::sum(column, rows).map(Some)
    }
}

impl Lane for i64 {
    const NAME: &'static str = "int8";
    const MIN: Self = i64::MIN;
    const SUMS: bool = false;
    fn widen(self) -> i64 {
        self
    }
    fn least<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Self>> {
        int64::min(column, rows)
    }
    fn greatest<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Self>> {
        int64::max(column, rows)
    }
    fn sum<C: ColumnReader<Value = Self>>(
        _: &C,
        _: &RowMaskView<'_>,
    ) -> Result<Option<Option<i64>>> {
        Ok(None)
    }
}

struct Model<T> {
    count: usize,
    /// `None` without a sum kernel.
    sum: Option<Option<i64>>,
    min: Option<T>,
    max: Option<T>,
}

fn model<W: Lane>(values: &[W], selected: &[bool], non_null: &[bool]) -> Model<W> {
    let present: Vec<W> = (0..values.len())
        .filter(|&row| selected[row] && non_null[row])
        .map(|row| values[row])
        .collect();
    Model {
        count: present.len(),
        sum: W::SUMS
            .then(|| (!present.is_empty()).then(|| present.iter().map(|&v| v.widen()).sum())),
        min: present.iter().copied().min(),
        max: present.iter().copied().max(),
    }
}

fn assert_aggregates<W: Lane>(
    column: &ColumnView<'_, W>,
    rows: &RowMaskView<'_>,
    expected: &Model<W>,
    what: &str,
) -> Result<()> {
    assert_eq!(count(column, rows)?, expected.count, "count {what}");
    assert_eq!(W::sum(column, rows)?, expected.sum, "sum {what}");
    assert_eq!(W::least(column, rows)?, expected.min, "min {what}");
    assert_eq!(W::greatest(column, rows)?, expected.max, "max {what}");
    Ok(())
}

/// A column, its non-NULL flags (none when `masked` is false: the unmasked
/// kernels) and a selection; the rows an aggregate reads hold values leaning
/// to the edges, the others edges.
#[derive(Clone, Debug)]
struct Batch<W> {
    values: Vec<W>,
    non_null: Vec<bool>,
    masked: bool,
    selected: Vec<bool>,
}

fn batches<W: Lane>() -> impl Strategy<Value = Batch<W>> {
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
                values(&read, &integer::<W>()),
                Just((non_null, masked, selected.clone())),
            )
        })
        .prop_map(|(values, (non_null, masked, selected))| Batch {
            values,
            non_null,
            masked,
            selected,
        })
}

/// count, sum, min and max match the model, on the whole-word and the row
/// path.
fn aggregates_match_the_model_and_the_row_path<W: Lane>() {
    property(batches::<W>(), |batch| -> Result<()> {
        let nrows = batch.selected.len();
        let non_null_words = words(&batch.non_null);
        let non_nulls = batch
            .masked
            .then(|| RowMaskView::try_new(nrows, &non_null_words))
            .transpose()?;
        let column = ColumnView::try_new(&batch.values, non_nulls)?;
        let rows_only = RowsOnly(ColumnView::try_new(&batch.values, non_nulls)?);
        let selection = words(&batch.selected);
        let rows = RowMaskView::try_new(nrows, &selection)?;
        let expected = model(&batch.values, &batch.selected, &batch.non_null);
        assert_aggregates(&column, &rows, &expected, W::NAME)?;
        let name = W::NAME;
        assert_eq!(count(&rows_only, &rows)?, expected.count, "{name} rows");
        assert_eq!(W::sum(&rows_only, &rows)?, expected.sum, "{name} rows");
        assert_eq!(W::least(&rows_only, &rows)?, expected.min, "{name} rows");
        assert_eq!(W::greatest(&rows_only, &rows)?, expected.max, "{name} rows");
        Ok(())
    });
}

#[test]
fn aggregates_match_the_model_and_the_row_path_int4() {
    aggregates_match_the_model_and_the_row_path::<i32>();
}

#[test]
fn aggregates_match_the_model_and_the_row_path_int8() {
    aggregates_match_the_model_and_the_row_path::<i64>();
}

fn empty_and_all_null<W: Lane>() -> Result<()> {
    let name = W::NAME;
    let values = [W::MIN; 130];
    let column = ColumnView::try_new(&values, None)?;
    let none = RowMaskView::try_new(130, &[0, 0, 0])?;
    assert_eq!(count(&column, &none)?, 0, "{name}");
    assert_eq!(W::sum(&column, &none)?, W::SUMS.then_some(None), "{name}");
    assert_eq!(W::least(&column, &none)?, None, "{name}");
    assert_eq!(W::greatest(&column, &none)?, None, "{name}");
    let all = RowMaskView::try_new(130, &[u64::MAX, u64::MAX, 3])?;
    assert_eq!(count(&column, &all)?, 130, "{name}");
    assert_eq!(
        W::sum(&column, &all)?,
        W::SUMS.then(|| Some(130 * W::MIN.widen())),
        "{name}"
    );
    assert_eq!(W::least(&column, &all)?, Some(W::MIN), "{name}");
    assert_eq!(W::greatest(&column, &all)?, Some(W::MIN), "{name}");
    // Every selected row NULL: nothing, and the values are never read.
    let nulls = RowMaskView::try_new(130, &[0, 0, 0])?;
    let column = ColumnView::try_new(&values, Some(nulls))?;
    assert_eq!(count(&column, &all)?, 0, "{name}");
    assert_eq!(W::sum(&column, &all)?, W::SUMS.then_some(None), "{name}");
    assert_eq!(W::least(&column, &all)?, None, "{name}");
    assert_eq!(W::greatest(&column, &all)?, None, "{name}");
    Ok(())
}

#[test]
fn empty_and_all_null_selections_give_nothing_and_extremes_do_not_overflow() -> Result<()> {
    empty_and_all_null::<i32>()?;
    empty_and_all_null::<i64>()
}

/// The count needs no values: a column of units counts like any other.
#[test]
fn count_reads_no_values() -> Result<()> {
    let units = [(); 200];
    let non_null: Vec<bool> = (0..200).map(|row| row % 3 != 0).collect();
    let non_null_words = words(&non_null);
    let column = ColumnView::try_new(&units, Some(RowMaskView::try_new(200, &non_null_words)?))?;
    for selected in [u64::MAX, 0b1010_1010, 1 << 7] {
        let words = [selected, selected, selected, selected & ((1 << 8) - 1)];
        let rows = RowMaskView::try_new(200, &words)?;
        let expected = (0..200)
            .filter(|&row| words[row / 64] & (1 << (row % 64)) != 0 && non_null[row])
            .count();
        assert_eq!(count(&column, &rows)?, expected, "{selected:#b}");
    }
    Ok(())
}

fn row_count_mismatches<W: Lane>() -> Result<()> {
    let name = W::NAME;
    let values = [W::from(1); 64];
    let column = ColumnView::try_new(&values, None)?;
    let rows = RowMaskView::try_new(65, &[u64::MAX, 1])?;
    assert!(count(&column, &rows).is_err(), "{name}");
    // Without a sum kernel there is nothing to fail.
    assert_eq!(W::sum(&column, &rows).is_err(), W::SUMS, "{name}");
    assert!(W::least(&column, &rows).is_err(), "{name}");
    assert!(W::greatest(&column, &rows).is_err(), "{name}");
    Ok(())
}

#[test]
fn row_count_mismatches_are_errors() -> Result<()> {
    row_count_mismatches::<i32>()?;
    row_count_mismatches::<i64>()
}

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]
#![forbid(unsafe_code)]

use anyhow::Result;
use proptest::prelude::*;
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView};
use tessera_kernels::{int32, int64};
use tessera_testing::{Int, edge, flags, integer, nrows, property, values, words};

const OPS: [int32::CompareOp; 6] = [
    int32::CompareOp::Eq,
    int32::CompareOp::Ne,
    int32::CompareOp::Lt,
    int32::CompareOp::Le,
    int32::CompareOp::Gt,
    int32::CompareOp::Ge,
];

/// One integer width under test: its comparison of two columns and the
/// values its columns draw.
trait Width: Int + Ord {
    /// Values close to each other, so that every operator both keeps and
    /// drops rows: -3 to 3, and for int8 also the same with a high half, so
    /// that a 32-bit comparison would call them equal.
    fn near() -> BoxedStrategy<Self>;

    fn compare_columns<L: ColumnReader<Value = Self>, R: ColumnReader<Value = Self>>(
        left: &L,
        right: &R,
        rows: &mut RowMask<'_>,
        op: int32::CompareOp,
    ) -> Result<()>;
}

impl Width for i32 {
    fn near() -> BoxedStrategy<Self> {
        (-3..=3).boxed()
    }

    fn compare_columns<L: ColumnReader<Value = Self>, R: ColumnReader<Value = Self>>(
        left: &L,
        right: &R,
        rows: &mut RowMask<'_>,
        op: int32::CompareOp,
    ) -> Result<()> {
        int32::compare_columns(left, right, rows, op)
    }
}

impl Width for i64 {
    fn near() -> BoxedStrategy<Self> {
        prop_oneof![-3_i64..=3, (-3_i64..=3).prop_map(|value| value << 32)].boxed()
    }

    fn compare_columns<L: ColumnReader<Value = Self>, R: ColumnReader<Value = Self>>(
        left: &L,
        right: &R,
        rows: &mut RowMask<'_>,
        op: int32::CompareOp,
    ) -> Result<()> {
        int64::compare_columns(left, right, rows, op)
    }
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

/// Two columns, their non-NULL flags and a selection, row by row; the rows
/// a comparison reads hold close values, edges and any values, the others
/// edges.
#[derive(Clone, Debug)]
struct Batch<T> {
    left: Vec<T>,
    right: Vec<T>,
    left_non_null: Vec<bool>,
    right_non_null: Vec<bool>,
    selected: Vec<bool>,
}

fn batches<T: Width>() -> impl Strategy<Value = Batch<T>> {
    nrows()
        .prop_flat_map(|nrows| (flags(nrows), flags(nrows), flags(nrows)))
        .prop_flat_map(|(selected, left_non_null, right_non_null)| {
            let live = prop_oneof![1 => edge::<T>(), 2 => T::near(), 1 => integer::<T>()].boxed();
            let read = |non_null: &[bool]| -> Vec<bool> {
                selected
                    .iter()
                    .zip(non_null)
                    .map(|(&s, &n)| s && n)
                    .collect()
            };
            (
                values(&read(&left_non_null), &live),
                values(&read(&right_non_null), &live),
                Just((selected.clone(), left_non_null, right_non_null)),
            )
        })
        .prop_map(
            |(left, right, (selected, left_non_null, right_non_null))| Batch {
                left,
                right,
                left_non_null,
                right_non_null,
                selected,
            },
        )
}

/// Every operator keeps exactly the selected rows where both sides are
/// present and the model holds, on the whole-word and the row path.
fn comparisons_match_the_model_and_the_row_path<T: Width>() {
    property(batches::<T>(), |batch| -> Result<()> {
        let nrows = batch.selected.len();
        let (left_words, right_words) = (words(&batch.left_non_null), words(&batch.right_non_null));
        let left =
            ColumnView::try_new(&batch.left, Some(RowMaskView::try_new(nrows, &left_words)?))?;
        let right = ColumnView::try_new(
            &batch.right,
            Some(RowMaskView::try_new(nrows, &right_words)?),
        )?;
        for op in OPS {
            let expected: Vec<bool> = (0..nrows)
                .map(|row| {
                    batch.selected[row]
                        && batch.left_non_null[row]
                        && batch.right_non_null[row]
                        && model(op, batch.left[row], batch.right[row])
                })
                .collect();
            let expected = words(&expected);
            let mut whole = words(&batch.selected);
            T::compare_columns(&left, &right, &mut RowMask::try_new(nrows, &mut whole)?, op)?;
            assert_eq!(whole, expected, "{op:?}");
            let mut by_rows = words(&batch.selected);
            T::compare_columns(
                &RowsOnly(&left),
                &RowsOnly(&right),
                &mut RowMask::try_new(nrows, &mut by_rows)?,
                op,
            )?;
            assert_eq!(by_rows, expected, "rows {op:?}");
        }
        Ok(())
    });
}

#[test]
fn comparisons_match_the_model_and_the_row_path_int4() {
    comparisons_match_the_model_and_the_row_path::<i32>();
}

#[test]
fn comparisons_match_the_model_and_the_row_path_int8() {
    comparisons_match_the_model_and_the_row_path::<i64>();
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

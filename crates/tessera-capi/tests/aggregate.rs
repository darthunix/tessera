//! The int4 and int8 aggregates and the type-free count over the capi
//! representations: whole words where the batch is fully prepared, rows
//! where it is not, and edges in NULL and unselected cells that must never
//! reach a result. Every check runs for both widths.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

mod support;

use std::mem::MaybeUninit;

use anyhow::Result;
use tessera_capi::{DatumIntColumn, DenseIntColumn};
use tessera_core::{ColumnReader, RowMaskView};
use tessera_kernels::count::count;
use tessera_kernels::{int32, int64};
use tessera_testing::{integer, property};

use support::{Storage, Width, batches};

/// One width under test and its kernels.
trait Lane: Width + From<i32> {
    /// The sum kernel's result, `None` without one: int4 sums into int8,
    /// int8 into numeric elsewhere.
    fn sum<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Option<i64>>>;
    /// A sum's model, `None` without a sum kernel.
    fn total(present: &[Self]) -> Option<Option<i64>>;
    fn least<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Self>>;
    fn greatest<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Self>>;
}

impl Lane for i32 {
    fn sum<C: ColumnReader<Value = Self>>(
        column: &C,
        rows: &RowMaskView<'_>,
    ) -> Result<Option<Option<i64>>> {
        int32::sum(column, rows).map(Some)
    }
    fn total(present: &[Self]) -> Option<Option<i64>> {
        Some((!present.is_empty()).then(|| present.iter().map(|&v| i64::from(v)).sum()))
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
}

impl Lane for i64 {
    fn sum<C: ColumnReader<Value = Self>>(
        _: &C,
        _: &RowMaskView<'_>,
    ) -> Result<Option<Option<i64>>> {
        Ok(None)
    }
    fn total(_: &[Self]) -> Option<Option<i64>> {
        None
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
}

fn check<T: Lane, C: ColumnReader<Value = T>>(
    column: &C,
    rows: &RowMaskView<'_>,
    present: &[T],
    what: &str,
) -> Result<()> {
    assert_eq!(count(column, rows)?, present.len(), "count {what}");
    assert_eq!(T::sum(column, rows)?, T::total(present), "sum {what}");
    assert_eq!(
        T::least(column, rows)?,
        present.iter().copied().min(),
        "min {what}"
    );
    assert_eq!(
        T::greatest(column, rows)?,
        present.iter().copied().max(),
        "max {what}"
    );
    Ok(())
}

fn representations_match_the_model<T: Lane>() {
    property(batches(integer::<T>()), |batch| -> Result<()> {
        let storage = Storage::new(&batch);
        let rows = storage.rows();
        let present: Vec<T> = (0..batch.values.len())
            .filter(|&row| batch.read(row))
            .map(|row| batch.values[row])
            .collect();
        assert_eq!(count(&storage.nulls(), &rows)?, present.len(), "nulls");
        check(&storage.dense(), &rows, &present, "dense")?;
        check(&storage.datum(), &rows, &present, "datum")
    });
}

#[test]
fn representations_match_the_model_int4() {
    representations_match_the_model::<i32>();
}

#[test]
fn representations_match_the_model_int8() {
    representations_match_the_model::<i64>();
}

fn unprepared_selected_rows_are_errors<T: Lane>() -> Result<()> {
    let values = [MaybeUninit::new(T::from(1)); 64];
    let datums = [MaybeUninit::new(1_u64); 64];
    let flags = [MaybeUninit::new(false); 64];
    let prepared = RowMaskView::try_new(64, &[u64::MAX >> 1])?;
    // SAFETY: the one unprepared row is never selected below except to fail.
    let dense = unsafe { DenseIntColumn::try_new(&values, None, Some(prepared)) }?;
    // SAFETY: as above.
    let datum = unsafe { DatumIntColumn::<T>::try_new(&datums, &flags, Some(prepared)) }?;
    let rows = RowMaskView::try_new(64, &[u64::MAX])?;
    // Without a sum kernel there is nothing to fail.
    assert_eq!(T::sum(&dense, &rows).is_err(), T::total(&[]).is_some());
    assert!(T::least(&dense, &rows).is_err());
    assert!(count(&datum, &rows).is_err());
    let rows = RowMaskView::try_new(64, &[u64::MAX >> 1])?;
    assert_eq!(T::sum(&dense, &rows)?, T::total(&[T::from(1); 63]));
    assert_eq!(T::greatest(&dense, &rows)?, Some(T::from(1)));
    assert_eq!(count(&datum, &rows)?, 63);
    Ok(())
}

#[test]
fn unprepared_selected_rows_are_errors_int4() -> Result<()> {
    unprepared_selected_rows_are_errors::<i32>()
}

#[test]
fn unprepared_selected_rows_are_errors_int8() -> Result<()> {
    unprepared_selected_rows_are_errors::<i64>()
}

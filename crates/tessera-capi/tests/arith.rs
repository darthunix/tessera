//! The int4 and int8 arithmetic over the capi representations: whole words
//! where the batch is fully prepared, rows where it is not, and edges in
//! NULL and unselected cells that would overflow or divide by zero if they
//! were used. Every check runs for both widths as `<check>_int4` and
//! `<check>_int8`.

mod support;

use std::mem::MaybeUninit;

use anyhow::Result;
use proptest::prelude::*;
use tessera_capi::{DatumIntColumn, DenseIntColumn};
use tessera_core::{ColumnReader, RowMask, RowMaskView};
use tessera_kernels::ops::{ArithOp, ArithmeticError};
use tessera_kernels::{int32, int64};
use tessera_testing::{integer, property, small, words};

use support::{Batch, Storage, Width, batches};

const OPS: [ArithOp; 5] = [
    ArithOp::Add,
    ArithOp::Sub,
    ArithOp::Mul,
    ArithOp::Div,
    ArithOp::Mod,
];

/// One width under test: its error and its three kernels.
trait Lane: Width + Into<i128> + TryFrom<i128> + From<i32> {
    /// The error of a result that does not fit the width.
    const OUT_OF_RANGE: ArithmeticError;

    fn arith_scalar<C: ColumnReader<Value = Self>>(
        op: ArithOp,
        column: &C,
        scalar: Self,
        rows: &RowMaskView<'_>,
        values: &mut [MaybeUninit<Self>],
        non_nulls: &mut RowMask<'_>,
    ) -> Result<()>;
    fn arith_scalar_left<C: ColumnReader<Value = Self>>(
        op: ArithOp,
        scalar: Self,
        column: &C,
        rows: &RowMaskView<'_>,
        values: &mut [MaybeUninit<Self>],
        non_nulls: &mut RowMask<'_>,
    ) -> Result<()>;
    fn arith_columns<L: ColumnReader<Value = Self>, R: ColumnReader<Value = Self>>(
        op: ArithOp,
        left: &L,
        right: &R,
        rows: &RowMaskView<'_>,
        values: &mut [MaybeUninit<Self>],
        non_nulls: &mut RowMask<'_>,
    ) -> Result<()>;
}

/// The three kernels of one width's module.
macro_rules! kernels {
    ($module:ident) => {
        fn arith_scalar<C: ColumnReader<Value = Self>>(
            op: ArithOp,
            column: &C,
            scalar: Self,
            rows: &RowMaskView<'_>,
            values: &mut [MaybeUninit<Self>],
            non_nulls: &mut RowMask<'_>,
        ) -> Result<()> {
            $module::arith_scalar(op, column, scalar, rows, values, non_nulls)
        }
        fn arith_scalar_left<C: ColumnReader<Value = Self>>(
            op: ArithOp,
            scalar: Self,
            column: &C,
            rows: &RowMaskView<'_>,
            values: &mut [MaybeUninit<Self>],
            non_nulls: &mut RowMask<'_>,
        ) -> Result<()> {
            $module::arith_scalar_left(op, scalar, column, rows, values, non_nulls)
        }
        fn arith_columns<L: ColumnReader<Value = Self>, R: ColumnReader<Value = Self>>(
            op: ArithOp,
            left: &L,
            right: &R,
            rows: &RowMaskView<'_>,
            values: &mut [MaybeUninit<Self>],
            non_nulls: &mut RowMask<'_>,
        ) -> Result<()> {
            $module::arith_columns(op, left, right, rows, values, non_nulls)
        }
    };
}

impl Lane for i32 {
    const OUT_OF_RANGE: ArithmeticError = ArithmeticError::IntegerOutOfRange;
    kernels!(int32);
}

impl Lane for i64 {
    const OUT_OF_RANGE: ArithmeticError = ArithmeticError::BigintOutOfRange;
    kernels!(int64);
}

/// PostgreSQL's int4 and int8 arithmetic on i128, with its errors.
fn model<T: Lane>(op: ArithOp, a: T, b: T) -> Result<T, ArithmeticError> {
    let (a, b): (i128, i128) = (a.into(), b.into());
    let wide = match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div | ArithOp::Mod if b == 0 => return Err(ArithmeticError::DivisionByZero),
        ArithOp::Div => a / b,
        ArithOp::Mod if b == -1 => 0,
        ArithOp::Mod => a % b,
    };
    T::try_from(wide).map_err(|_| T::OUT_OF_RANGE)
}

/// Every kernel shape against the model: `column op scalar`, `scalar op
/// column` and `column op column` with the column as both sides. The kernel
/// fails exactly when the model fails in a read row, with one of its errors.
fn check<T: Lane, C: ColumnReader<Value = T>>(
    column: &C,
    batch: &Batch<T>,
    rows: &RowMaskView<'_>,
    scalar: T,
    what: &str,
) -> Result<()> {
    let nrows = batch.values.len();
    let present: Vec<bool> = (0..nrows).map(|row| batch.read(row)).collect();
    for op in OPS {
        for shape in 0..3 {
            let mut out = vec![MaybeUninit::uninit(); nrows];
            let mut kept = vec![0; nrows.div_ceil(64)];
            let mut mask = RowMask::try_new(nrows, &mut kept)?;
            let outcome = match shape {
                0 => T::arith_scalar(op, column, scalar, rows, &mut out, &mut mask),
                1 => T::arith_scalar_left(op, scalar, column, rows, &mut out, &mut mask),
                _ => T::arith_columns(op, column, column, rows, &mut out, &mut mask),
            };
            let expected: Vec<Option<Result<T, ArithmeticError>>> = (0..nrows)
                .map(|row| {
                    let value = batch.values[row];
                    present[row].then(|| match shape {
                        0 => model(op, value, scalar),
                        1 => model(op, scalar, value),
                        _ => model(op, value, value),
                    })
                })
                .collect();
            let errors: Vec<ArithmeticError> = expected
                .iter()
                .flatten()
                .filter_map(|result| result.err())
                .collect();
            match (errors.first(), outcome) {
                (Some(_), Err(error)) => {
                    let error = error.downcast_ref::<ArithmeticError>().copied();
                    assert!(
                        error.is_some_and(|error| errors.contains(&error)),
                        "{what} {op:?} {shape}: kernel {error:?}, model {errors:?}"
                    );
                }
                (None, Ok(())) => {
                    assert_eq!(kept, words(&present), "{what} {op:?} {shape}");
                    for (row, expected) in expected.iter().enumerate() {
                        if let Some(Ok(value)) = expected {
                            // SAFETY: the mask marks the row, so the kernel wrote it.
                            let written = unsafe { out[row].assume_init() };
                            assert_eq!(written, *value, "{what} {op:?} {shape} row {row}");
                        }
                    }
                }
                (failure, outcome) => {
                    panic!("{what} {op:?} {shape}: model {failure:?}, kernel {outcome:?}")
                }
            }
        }
    }
    Ok(())
}

/// Batches whose read rows hold small values, so that most calls succeed,
/// or values leaning to the edges, so that most fail, and a scalar leaning
/// to the edges.
fn cases<T: Lane>() -> impl Strategy<Value = (Batch<T>, T)> {
    any::<bool>().prop_flat_map(|edgy| {
        let live = if edgy { integer::<T>() } else { small::<T>() };
        (batches(live), integer::<T>())
    })
}

fn representations_match_the_model<T: Lane>() {
    property(cases::<T>(), |(batch, scalar)| -> Result<()> {
        let storage = Storage::new(&batch);
        let rows = storage.rows();
        check(&storage.dense(), &batch, &rows, scalar, "dense")?;
        check(&storage.datum(), &batch, &rows, scalar, "datum")
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
    let one = T::from(1);
    let values = [MaybeUninit::new(one); 64];
    let datums = [MaybeUninit::new(1_u64); 64];
    let flags = [MaybeUninit::new(false); 64];
    let prepared = RowMaskView::try_new(64, &[u64::MAX >> 1])?;
    // SAFETY: the one unprepared row is selected only to fail.
    let dense = unsafe { DenseIntColumn::try_new(&values, None, Some(prepared)) }?;
    // SAFETY: as above.
    let datum = unsafe { DatumIntColumn::<T>::try_new(&datums, &flags, Some(prepared)) }?;
    let mut out = [MaybeUninit::uninit(); 64];
    let mut kept = [0];
    let rows = RowMaskView::try_new(64, &[u64::MAX])?;
    let mut mask = RowMask::try_new(64, &mut kept)?;
    assert!(T::arith_scalar(ArithOp::Add, &dense, one, &rows, &mut out, &mut mask).is_err());
    assert!(T::arith_scalar(ArithOp::Add, &datum, one, &rows, &mut out, &mut mask).is_err());
    let rows = RowMaskView::try_new(64, &[u64::MAX >> 1])?;
    T::arith_scalar(ArithOp::Add, &dense, one, &rows, &mut out, &mut mask)?;
    assert_eq!(mask.as_view().selected_count(), 63);
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

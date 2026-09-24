//! The int8 entry points.

use std::ffi::c_uint;

use anyhow::{Context, Result};
use tessera_core::RowMaskView;
use tessera_kernels::int64;

use super::args::{inputs, outputs, reader};
use super::column::DatumColumn;
use super::int32::{arith_op, compare_op, hash_outputs, null_keys};
use super::mask::Mask;
use super::status::{Code, Status, guard};
use crate::DatumInt64Column;

/// `tess_int8_filter`: keep in `rows` the selected rows whose non-NULL
/// value satisfies `value op scalar`.
///
/// # Safety
///
/// `column` must point to a valid `TessDatumColumn` satisfying
/// [`DatumColumn::int64`]'s contract with `prepared` (null or a valid mask)
/// as its readiness; `rows` must point to a valid mask that nothing else
/// accesses during the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int8_filter(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *mut Mask,
    op: c_uint,
    scalar: i64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's status contract.
    unsafe {
        guard(status, || {
            let op = compare_op(op)?;
            let column = reader::<i64>(column, prepared)?;
            let rows = rows.as_mut().context("a null row mask")?;
            let mut rows = rows.mask()?;
            int64::filter(&column, &mut rows, op, scalar)
        })
    }
}

/// `tess_int8_arith_scalar`: `column op scalar` into a dense int8 result.
///
/// # Safety
///
/// As for [`inputs`] and [`outputs`]; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int8_arith_scalar(
    op: c_uint,
    column: *const DatumColumn,
    scalar: i64,
    prepared: *const Mask,
    rows: *const Mask,
    values: *mut i64,
    non_nulls: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let op = arith_op(op)?;
            let (column, rows) = inputs::<i64>(column, prepared, rows)?;
            let (values, mut non_nulls) = outputs(values, non_nulls)?;
            int64::arith_scalar(op, &column, scalar, &rows, values, &mut non_nulls)
        })
    }
}

/// `tess_int8_arith_scalar_left`: `scalar op column` into a dense int8
/// result.
///
/// # Safety
///
/// As for [`tess_int8_arith_scalar`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int8_arith_scalar_left(
    op: c_uint,
    scalar: i64,
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
    values: *mut i64,
    non_nulls: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let op = arith_op(op)?;
            let (column, rows) = inputs::<i64>(column, prepared, rows)?;
            let (values, mut non_nulls) = outputs(values, non_nulls)?;
            int64::arith_scalar_left(op, scalar, &column, &rows, values, &mut non_nulls)
        })
    }
}

/// `tess_int8_arith_columns`: `left op right` row by row into a dense int8
/// result.
///
/// # Safety
///
/// As for [`tess_int8_arith_scalar`], for both columns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int8_arith_columns(
    op: c_uint,
    left: *const DatumColumn,
    left_prepared: *const Mask,
    right: *const DatumColumn,
    right_prepared: *const Mask,
    rows: *const Mask,
    values: *mut i64,
    non_nulls: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let op = arith_op(op)?;
            let (left, rows) = inputs::<i64>(left, left_prepared, rows)?;
            let right = reader::<i64>(right, right_prepared)?;
            let (values, mut non_nulls) = outputs(values, non_nulls)?;
            int64::arith_columns(op, &left, &right, &rows, values, &mut non_nulls)
        })
    }
}

/// `tess_int8_hash`: hash the first int8 key of the selected rows into
/// `hashes` and set `valid`, as `tess_int4_hash` with PostgreSQL's
/// `hashint8` fold.
///
/// # Safety
///
/// As for [`inputs`] and [`hash_outputs`]; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int8_hash(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
    nulls: c_uint,
    hashes: *mut u32,
    valid: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let nulls = null_keys(nulls)?;
            let (column, rows) = inputs::<i64>(column, prepared, rows)?;
            let (hashes, mut valid) = hash_outputs(hashes, valid)?;
            int64::hash(&column, &rows, nulls, hashes, &mut valid)
        })
    }
}

/// `tess_int8_hash_next`: fold the next int8 key into the hashes of the
/// rows in `valid`, narrowing it.
///
/// # Safety
///
/// As for [`reader`] and [`hash_outputs`]; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int8_hash_next(
    column: *const DatumColumn,
    prepared: *const Mask,
    nulls: c_uint,
    hashes: *mut u32,
    valid: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let nulls = null_keys(nulls)?;
            let column = reader::<i64>(column, prepared)?;
            let (hashes, mut valid) = hash_outputs(hashes, valid)?;
            int64::hash_next(&column, nulls, hashes, &mut valid)
        })
    }
}

/// `tess_int8_min`: the least selected non-NULL value, NULL without any.
///
/// # Safety
///
/// As for [`inputs`]; `isnull` and `value` must be writable; `status` as
/// for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int8_min(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
    isnull: *mut bool,
    value: *mut i64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        extreme(
            column,
            prepared,
            rows,
            isnull,
            value,
            status,
            |column, rows| int64::min(column, rows),
        )
    }
}

/// `tess_int8_max`: the greatest selected non-NULL value, NULL without any.
///
/// # Safety
///
/// As for [`tess_int8_min`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int8_max(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
    isnull: *mut bool,
    value: *mut i64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        extreme(
            column,
            prepared,
            rows,
            isnull,
            value,
            status,
            |column, rows| int64::max(column, rows),
        )
    }
}

/// The shape of `tess_int8_min` and `tess_int8_max`.
///
/// # Safety
///
/// As for [`tess_int8_min`].
unsafe fn extreme(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
    isnull: *mut bool,
    value: *mut i64,
    status: *mut Status,
    kernel: fn(&DatumInt64Column<'_>, &RowMaskView<'_>) -> Result<Option<i64>>,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let null = isnull.as_mut().context("a null flag")?;
            let out = value.as_mut().context("a null result")?;
            let (column, rows) = inputs::<i64>(column, prepared, rows)?;
            let found = kernel(&column, &rows)?;
            *null = found.is_none();
            *out = found.unwrap_or(0);
            Ok(())
        })
    }
}

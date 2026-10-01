//! The int4 entry points.

use std::ffi::c_uint;

use anyhow::{Context, Result};
use tessera_core::RowMaskView;
use tessera_kernels::int32;

use super::args::{arith_op, compare_op, hash_outputs, inputs, null_keys, outputs, reader};
use super::column::DatumColumn;
use super::mask::Mask;
use super::status::{Code, Status, guard};
use crate::DatumInt32Column;

/// The ABI version of `tessera/kernels.h` this library implements.
const ABI_VERSION: u32 = 0;

/// `tess_kernels_abi_version`.
#[unsafe(no_mangle)]
pub extern "C" fn tess_kernels_abi_version() -> u32 {
    ABI_VERSION
}

/// `TessKernelsProbe`: a `TessRowMask`'s size.
pub const PROBE_ROW_MASK_SIZE: c_uint = 0;
/// A `TessDatumColumn`'s size.
pub const PROBE_DATUM_COLUMN_SIZE: c_uint = 1;
/// The offset of a `TessDatumColumn`'s `nrows`.
pub const PROBE_DATUM_COLUMN_NROWS_OFFSET: c_uint = 2;
/// A `TessStatus`'s size.
pub const PROBE_STATUS_SIZE: c_uint = 3;
/// The offset of a `TessStatus`'s `message`.
pub const PROBE_STATUS_MESSAGE_OFFSET: c_uint = 4;

/// `tess_kernels_probe`: the size or offset for a `TessKernelsProbe`, or 0.
#[unsafe(no_mangle)]
pub extern "C" fn tess_kernels_probe(kind: c_uint) -> usize {
    match kind {
        PROBE_ROW_MASK_SIZE => size_of::<Mask>(),
        PROBE_DATUM_COLUMN_SIZE => size_of::<DatumColumn>(),
        PROBE_DATUM_COLUMN_NROWS_OFFSET => std::mem::offset_of!(DatumColumn, nrows),
        PROBE_STATUS_SIZE => size_of::<Status>(),
        PROBE_STATUS_MESSAGE_OFFSET => std::mem::offset_of!(Status, message),
        _ => 0,
    }
}

/// `tess_kernels_test_panic`: raise and catch a panic.
///
/// # Safety
///
/// `status` must be null or valid as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_kernels_test_panic(status: *mut Status) -> Code {
    // SAFETY: the caller's status contract.
    unsafe { guard(status, || panic!("injected panic")) }
}

/// `tess_int4_filter`: keep in `rows` the selected rows whose non-NULL
/// value satisfies `value op scalar`.
///
/// # Safety
///
/// `column` must point to a valid `TessDatumColumn` satisfying
/// [`DatumColumn::int32`]'s contract with `prepared` (null or a valid mask)
/// as its readiness; `rows` must point to a valid mask that nothing else
/// accesses during the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int4_filter(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *mut Mask,
    op: c_uint,
    scalar: i32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's status contract.
    unsafe {
        guard(status, || {
            let op = compare_op(op)?;
            let column = column.as_ref().context("a null column")?;
            let prepared = Mask::view_optional(prepared)?;
            let column = column.int32(prepared)?;
            let rows = rows.as_mut().context("a null row mask")?;
            let mut rows = rows.mask()?;
            int32::filter(&column, &mut rows, op, scalar)
        })
    }
}

/// `tess_int4_compare_columns`: keep in `rows` the selected rows where both
/// columns are non-NULL and `left op right`.
///
/// # Safety
///
/// `left` and `right` must point to valid `TessDatumColumn`s satisfying
/// [`DatumColumn::int32`]'s contract with their readiness masks (null or
/// valid); `rows` must point to a valid mask that nothing else accesses
/// during the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int4_compare_columns(
    left: *const DatumColumn,
    left_prepared: *const Mask,
    right: *const DatumColumn,
    right_prepared: *const Mask,
    rows: *mut Mask,
    op: c_uint,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's status contract.
    unsafe {
        guard(status, || {
            let op = compare_op(op)?;
            let left = reader(left, left_prepared)?;
            let right = reader(right, right_prepared)?;
            let rows = rows.as_mut().context("a null row mask")?;
            let mut rows = rows.mask()?;
            int32::compare_columns(&left, &right, &mut rows, op)
        })
    }
}

/// `tess_int4_arith_scalar`: `column op scalar` into a dense result.
///
/// # Safety
///
/// As for [`inputs`] and [`outputs`]; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int4_arith_scalar(
    op: c_uint,
    column: *const DatumColumn,
    scalar: i32,
    prepared: *const Mask,
    rows: *const Mask,
    values: *mut i32,
    non_nulls: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let op = arith_op(op)?;
            let (column, rows) = inputs(column, prepared, rows)?;
            let (values, mut non_nulls) = outputs(values, non_nulls)?;
            int32::arith_scalar(op, &column, scalar, &rows, values, &mut non_nulls)
        })
    }
}

/// `tess_int4_arith_scalar_left`: `scalar op column` into a dense result.
///
/// # Safety
///
/// As for [`tess_int4_arith_scalar`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int4_arith_scalar_left(
    op: c_uint,
    scalar: i32,
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
    values: *mut i32,
    non_nulls: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let op = arith_op(op)?;
            let (column, rows) = inputs(column, prepared, rows)?;
            let (values, mut non_nulls) = outputs(values, non_nulls)?;
            int32::arith_scalar_left(op, scalar, &column, &rows, values, &mut non_nulls)
        })
    }
}

/// `tess_int4_arith_columns`: `left op right` row by row into a dense
/// result.
///
/// # Safety
///
/// As for [`tess_int4_arith_scalar`], for both columns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int4_arith_columns(
    op: c_uint,
    left: *const DatumColumn,
    left_prepared: *const Mask,
    right: *const DatumColumn,
    right_prepared: *const Mask,
    rows: *const Mask,
    values: *mut i32,
    non_nulls: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let op = arith_op(op)?;
            let (left, rows) = inputs(left, left_prepared, rows)?;
            let right = reader(right, right_prepared)?;
            let (values, mut non_nulls) = outputs(values, non_nulls)?;
            int32::arith_columns(op, &left, &right, &rows, values, &mut non_nulls)
        })
    }
}

/// `tess_int4_hash`: hash the first key of the selected rows into `hashes`
/// and set `valid`.
///
/// # Safety
///
/// As for [`inputs`] and [`hash_outputs`]; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int4_hash(
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
            let (column, rows) = inputs(column, prepared, rows)?;
            let (hashes, mut valid) = hash_outputs(hashes, valid)?;
            int32::hash(&column, &rows, nulls, hashes, &mut valid)
        })
    }
}

/// `tess_int4_hash_next`: fold the next key into the hashes of the rows in
/// `valid`, narrowing it.
///
/// # Safety
///
/// As for [`reader`] and [`hash_outputs`]; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int4_hash_next(
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
            let column = reader(column, prepared)?;
            let (hashes, mut valid) = hash_outputs(hashes, valid)?;
            int32::hash_next(&column, nulls, hashes, &mut valid)
        })
    }
}

/// `tess_int4_sum`: the int8 sum of the selected non-NULL values, NULL
/// without any.
///
/// # Safety
///
/// As for [`inputs`]; `isnull` and `sum` must be writable; `status` as
/// for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int4_sum(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
    isnull: *mut bool,
    sum: *mut i64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let null = isnull.as_mut().context("a null flag")?;
            let out = sum.as_mut().context("a null result")?;
            let (column, rows) = inputs(column, prepared, rows)?;
            let value = int32::sum(&column, &rows)?;
            *null = value.is_none();
            *out = value.unwrap_or(0);
            Ok(())
        })
    }
}

/// `tess_int4_min`: the least selected non-NULL value, NULL without any.
///
/// # Safety
///
/// As for [`tess_int4_sum`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int4_min(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
    isnull: *mut bool,
    value: *mut i32,
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
            |column, rows| int32::min(column, rows),
        )
    }
}

/// `tess_int4_max`: the greatest selected non-NULL value, NULL without any.
///
/// # Safety
///
/// As for [`tess_int4_sum`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int4_max(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
    isnull: *mut bool,
    value: *mut i32,
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
            |column, rows| int32::max(column, rows),
        )
    }
}

/// The shape of `tess_int4_min` and `tess_int4_max`.
///
/// # Safety
///
/// As for [`tess_int4_sum`].
unsafe fn extreme(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
    isnull: *mut bool,
    value: *mut i32,
    status: *mut Status,
    kernel: fn(&DatumInt32Column<'_>, &RowMaskView<'_>) -> Result<Option<i32>>,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let null = isnull.as_mut().context("a null flag")?;
            let out = value.as_mut().context("a null result")?;
            let (column, rows) = inputs(column, prepared, rows)?;
            let found = kernel(&column, &rows)?;
            *null = found.is_none();
            *out = found.unwrap_or(0);
            Ok(())
        })
    }
}

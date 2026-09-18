//! The int4 entry points.

use std::ffi::c_uint;

use anyhow::{Context, Result, bail};
use tessera_core::RowMaskView;
use tessera_kernels::int32::{self, CompareOp};

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

/// `tess_kernels_layout`: the size or offset for a `TessLayoutKind`, or 0.
#[unsafe(no_mangle)]
pub extern "C" fn tess_kernels_layout(kind: c_uint) -> usize {
    match kind {
        0 => size_of::<Mask>(),
        1 => size_of::<DatumColumn>(),
        2 => std::mem::offset_of!(DatumColumn, nrows),
        3 => size_of::<Status>(),
        4 => std::mem::offset_of!(Status, message),
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

/// The column reader and the selection of a read-only entry point.
///
/// # Safety
///
/// `column` must point to a valid `TessDatumColumn` satisfying
/// [`DatumColumn::int32`]'s contract with `prepared` (null or valid) as its
/// readiness, and `rows` to a valid mask; all valid and unchanged for `'a`.
unsafe fn inputs<'a>(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
) -> Result<(DatumInt32Column<'a>, RowMaskView<'a>)> {
    // SAFETY: the caller's contract, for `'a`.
    unsafe {
        let column = column.as_ref().context("a null column")?;
        let prepared = Mask::view_optional(prepared)?;
        let column = column.int32(prepared)?;
        let rows = rows.as_ref().context("a null row mask")?.view()?;
        Ok((column, rows))
    }
}

/// `tess_int4_count`: the number of selected non-NULL values.
///
/// # Safety
///
/// As for [`inputs`]; `count` must be writable; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int4_count(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
    count: *mut i64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let out = count.as_mut().context("a null result")?;
            let (column, rows) = inputs(column, prepared, rows)?;
            let value = int32::count(&column, &rows)?;
            *out = i64::try_from(value)?;
            Ok(())
        })
    }
}

/// `tess_int4_sum`: the int8 sum of the selected non-NULL values, NULL
/// without any.
///
/// # Safety
///
/// As for [`tess_int4_count`], with `isnull` and `sum` writable.
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

/// A `TessCompareOp` value.
fn compare_op(op: c_uint) -> Result<CompareOp> {
    Ok(match op {
        0 => CompareOp::Eq,
        1 => CompareOp::Ne,
        2 => CompareOp::Lt,
        3 => CompareOp::Le,
        4 => CompareOp::Gt,
        5 => CompareOp::Ge,
        _ => bail!("unknown comparison operation {op}"),
    })
}

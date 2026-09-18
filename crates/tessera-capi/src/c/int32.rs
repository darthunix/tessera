//! The int4 entry points.

use std::ffi::c_uint;
use std::mem::MaybeUninit;
use std::slice;

use anyhow::{Context, Result, bail, ensure};
use tessera_core::{RowMask, RowMaskView};
use tessera_kernels::int32::{self, ArithOp, CompareOp, NullKeys};

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

/// The reader of a column argument.
///
/// # Safety
///
/// `column` must point to a valid `TessDatumColumn` satisfying
/// [`DatumColumn::int32`]'s contract with `prepared` (null or valid) as its
/// readiness, both valid and unchanged for `'a`.
unsafe fn reader<'a>(
    column: *const DatumColumn,
    prepared: *const Mask,
) -> Result<DatumInt32Column<'a>> {
    // SAFETY: the caller's contract, for `'a`.
    unsafe {
        let column = column.as_ref().context("a null column")?;
        let prepared = Mask::view_optional(prepared)?;
        column.int32(prepared)
    }
}

/// The column reader and the selection of an entry point.
///
/// # Safety
///
/// As for [`reader`], and `rows` must point to a valid mask, unchanged
/// for `'a`.
unsafe fn inputs<'a>(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
) -> Result<(DatumInt32Column<'a>, RowMaskView<'a>)> {
    // SAFETY: the caller's contract, for `'a`.
    unsafe {
        let column = reader(column, prepared)?;
        let rows = rows.as_ref().context("a null row mask")?.view()?;
        Ok((column, rows))
    }
}

/// The result buffers of an arithmetic entry point: `values` has the row
/// count of `non_nulls`.
///
/// # Safety
///
/// `non_nulls` must point to a valid mask and `values` to as many writable
/// int4 slots as it has rows, possibly uninitialized; nothing else may
/// access either for `'a`.
unsafe fn outputs<'a>(
    values: *mut i32,
    non_nulls: *mut Mask,
) -> Result<(&'a mut [MaybeUninit<i32>], RowMask<'a>)> {
    // SAFETY: the caller's contract, for `'a`.
    unsafe {
        let non_nulls = non_nulls.as_mut().context("a null result mask")?.mask()?;
        let nrows = non_nulls.as_view().nrows();
        let values = if nrows == 0 {
            &mut [][..]
        } else {
            ensure!(!values.is_null(), "a null result buffer");
            slice::from_raw_parts_mut(values.cast::<MaybeUninit<i32>>(), nrows)
        };
        Ok((values, non_nulls))
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

/// The result buffers of a hash entry point: `hashes` has the row count of
/// `valid`.
///
/// # Safety
///
/// `valid` must point to a valid mask and `hashes` to as many initialized,
/// writable `u32` slots as it has rows; nothing else may access either for
/// `'a`.
unsafe fn hash_outputs<'a>(
    hashes: *mut u32,
    valid: *mut Mask,
) -> Result<(&'a mut [u32], RowMask<'a>)> {
    // SAFETY: the caller's contract, for `'a`.
    unsafe {
        let valid = valid.as_mut().context("a null valid mask")?.mask()?;
        let nrows = valid.as_view().nrows();
        let hashes = if nrows == 0 {
            &mut [][..]
        } else {
            ensure!(!hashes.is_null(), "a null hash buffer");
            slice::from_raw_parts_mut(hashes, nrows)
        };
        Ok((hashes, valid))
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

/// A `TessNullKeys` value.
fn null_keys(nulls: c_uint) -> Result<NullKeys> {
    Ok(match nulls {
        0 => NullKeys::Reject,
        1 => NullKeys::Group,
        _ => bail!("unknown NULL key policy {nulls}"),
    })
}

/// A `TessArithOp` value.
fn arith_op(op: c_uint) -> Result<ArithOp> {
    Ok(match op {
        0 => ArithOp::Add,
        1 => ArithOp::Sub,
        2 => ArithOp::Mul,
        3 => ArithOp::Div,
        4 => ArithOp::Mod,
        _ => bail!("unknown arithmetic operation {op}"),
    })
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

//! The int4 entry points.

use std::ffi::c_uint;

use anyhow::{Context, Result, bail};
use tessera_kernels::int32::{self, CompareOp};

use super::column::DatumColumn;
use super::mask::Mask;
use super::status::{Code, Status, guard};

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

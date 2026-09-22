//! The int8 entry points.

use std::ffi::c_uint;

use anyhow::Context;
use tessera_kernels::int64;

use super::args::reader;
use super::column::DatumColumn;
use super::int32::compare_op;
use super::mask::Mask;
use super::status::{Code, Status, guard};

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

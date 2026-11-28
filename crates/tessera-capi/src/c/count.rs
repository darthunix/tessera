//! The count entry point, for a column of any type.

use tessera_kernels::count;

use super::args::inputs;
use super::column::DatumColumn;
use super::mask::Mask;
use super::status::{Code, Status, guard};
use super::table::output;

/// `tess_count`: the number of selected non-NULL rows of a column of any
/// type, from its NULL flags alone.
///
/// # Safety
///
/// `column` must point to a valid `TessDatumColumn` satisfying
/// [`DatumColumn::nulls`]'s contract with `prepared` (null or a valid
/// mask) as its readiness; `rows` must point to a valid mask; `count` must
/// be writable; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_count(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
    count: *mut i64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let out = output(count, "result")?;
            let (column, rows) = inputs::<()>(column, prepared, rows)?;
            let value = count::count(&column, &rows)?;
            out.write(i64::try_from(value)?);
            Ok(())
        })
    }
}

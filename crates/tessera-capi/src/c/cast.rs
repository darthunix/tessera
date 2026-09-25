//! The cast entry points: int4 Datums widened into int8 Datums, int8
//! Datums narrowed into int4 values.

use tessera_kernels::cast;

use super::args::{inputs, outputs};
use super::column::DatumColumn;
use super::mask::Mask;
use super::status::{Code, Status, guard};

/// `tess_int4_to_int8`: the selected int4 values widened into a dense
/// column of int8 Datums, with `non_nulls` marking the rows that hold one.
///
/// # Safety
///
/// As for the int4 arithmetic entry points: `column` must satisfy
/// [`DatumColumn::int32`]'s contract with `prepared` as its readiness,
/// `rows` and `non_nulls` must point to valid masks, `values` to as many
/// writable Datum slots as `non_nulls` has rows; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int4_to_int8(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
    values: *mut u64,
    non_nulls: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let (column, rows) = inputs::<i32>(column, prepared, rows)?;
            // A Datum holding an int8 is its bits.
            let (values, mut non_nulls) = outputs(values.cast::<i64>(), non_nulls)?;
            cast::int4_to_int8(&column, &rows, values, &mut non_nulls)
        })
    }
}

/// `tess_int8_to_int4`: the selected int8 values narrowed into a dense
/// column of int32 values, with `non_nulls` marking the rows that hold one;
/// a selected value outside the int4 range fails with 22003.
///
/// # Safety
///
/// As for the int8 arithmetic entry points: `column` must satisfy
/// [`DatumColumn::int64`]'s contract with `prepared` as its readiness,
/// `rows` and `non_nulls` must point to valid masks, `values` to as many
/// writable int32 slots as `non_nulls` has rows; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int8_to_int4(
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
            let (column, rows) = inputs::<i64>(column, prepared, rows)?;
            let (values, mut non_nulls) = outputs(values, non_nulls)?;
            cast::int8_to_int4(&column, &rows, values, &mut non_nulls)
        })
    }
}

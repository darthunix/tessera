//! The set entry points, declared in `include/tessera/kernels.h`: `x IN
//! (…)` of integer constants over a column of int4 or int8 words
//! ([`tessera_kernels::set`]).

use std::slice;

use anyhow::{Context, Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};
use tessera_kernels::set::{self, KeySet, SetValue};

use super::column::DatumColumn;
use super::mask::Mask;
use super::status::{Code, Status, guard};

/// The keys, the selection and the two result masks of a call.
///
/// # Safety
///
/// `keys` must point to `nkeys` values unless `nkeys` is 0, `rows` to a
/// valid mask, `found` and `present` to valid masks that nothing else
/// accesses during the call.
unsafe fn in_set<'a, T, C>(
    column: impl FnOnce(Option<RowMaskView<'a>>) -> Result<C>,
    prepared: *const Mask,
    keys: *const T,
    nkeys: i32,
    rows: *const Mask,
    found: *mut Mask,
    present: *mut Mask,
) -> Result<()>
where
    T: SetValue + 'a,
    C: ColumnReader<Value = T>,
{
    // SAFETY: the caller's contract.
    unsafe {
        let column = column(Mask::view_optional(prepared)?)?;
        let rows = rows.as_ref().context("a null row mask")?.view()?;
        let nkeys = usize::try_from(nkeys).context("a negative key count")?;
        let keys = if nkeys == 0 {
            &[][..]
        } else {
            ensure!(!keys.is_null(), "null keys");
            slice::from_raw_parts(keys, nkeys)
        };
        let mut found: RowMask<'_> = found.as_mut().context("a null result mask")?.mask()?;
        let mut present = present.as_mut().context("a null result mask")?.mask()?;
        set::in_set(&KeySet::new(keys), &column, rows, &mut found, &mut present)
    }
}

/// `tess_int4_in_set`: of the selected rows, `found` the rows whose int4
/// value the `nkeys` sorted keys hold, `present` the rows whose value is
/// not NULL.
///
/// # Safety
///
/// `column` must point to a valid `TessDatumColumn` satisfying
/// [`DatumColumn::int32`]'s contract with `prepared` (null or a valid mask)
/// as its readiness; `keys` must point to `nkeys` values sorted without
/// repeats, `rows` to a valid mask, `found` and `present` to valid masks
/// that nothing else accesses during the call; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int4_in_set(
    column: *const DatumColumn,
    prepared: *const Mask,
    keys: *const i32,
    nkeys: i32,
    rows: *const Mask,
    found: *mut Mask,
    present: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let column = column.as_ref().context("a null column")?;
            in_set(
                |prepared| column.int32(prepared),
                prepared,
                keys,
                nkeys,
                rows,
                found,
                present,
            )
        })
    }
}

/// `tess_int8_in_set`: as [`tess_int4_in_set`] over int8 values.
///
/// # Safety
///
/// As for [`tess_int4_in_set`], with [`DatumColumn::int64`]'s contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_int8_in_set(
    column: *const DatumColumn,
    prepared: *const Mask,
    keys: *const i64,
    nkeys: i32,
    rows: *const Mask,
    found: *mut Mask,
    present: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let column = column.as_ref().context("a null column")?;
            in_set(
                |prepared| column.int64(prepared),
                prepared,
                keys,
                nkeys,
                rows,
                found,
                present,
            )
        })
    }
}

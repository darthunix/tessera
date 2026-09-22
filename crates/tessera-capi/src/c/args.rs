//! The arguments every integer entry point decodes: a column of one width
//! with its readiness, a selection, and the dense result buffers.

use std::mem::MaybeUninit;
use std::slice;

use anyhow::{Context, Result, ensure};
use tessera_core::{RowMask, RowMaskView};

use super::column::DatumColumn;
use super::mask::Mask;
use crate::{DatumIntColumn, FromDatum};

/// The reader of a column argument.
///
/// # Safety
///
/// `column` must point to a valid `TessDatumColumn` satisfying
/// [`DatumColumn::ints`]'s contract with `prepared` (null or valid) as its
/// readiness, both valid and unchanged for `'a`.
pub(super) unsafe fn reader<'a, T: FromDatum>(
    column: *const DatumColumn,
    prepared: *const Mask,
) -> Result<DatumIntColumn<'a, T>> {
    // SAFETY: the caller's contract, for `'a`.
    unsafe {
        let column = column.as_ref().context("a null column")?;
        let prepared = Mask::view_optional(prepared)?;
        column.ints(prepared)
    }
}

/// The column reader and the selection of an entry point.
///
/// # Safety
///
/// As for [`reader`], and `rows` must point to a valid mask, unchanged
/// for `'a`.
pub(super) unsafe fn inputs<'a, T: FromDatum>(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
) -> Result<(DatumIntColumn<'a, T>, RowMaskView<'a>)> {
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
/// slots of `T` as it has rows, possibly uninitialized; nothing else may
/// access either for `'a`.
pub(super) unsafe fn outputs<'a, T>(
    values: *mut T,
    non_nulls: *mut Mask,
) -> Result<(&'a mut [MaybeUninit<T>], RowMask<'a>)> {
    // SAFETY: the caller's contract, for `'a`.
    unsafe {
        let non_nulls = non_nulls.as_mut().context("a null result mask")?.mask()?;
        let nrows = non_nulls.as_view().nrows();
        let values = if nrows == 0 {
            &mut [][..]
        } else {
            ensure!(!values.is_null(), "a null result buffer");
            slice::from_raw_parts_mut(values.cast::<MaybeUninit<T>>(), nrows)
        };
        Ok((values, non_nulls))
    }
}

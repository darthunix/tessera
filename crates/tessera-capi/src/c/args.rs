//! The arguments every integer entry point decodes: a column of one width
//! with its readiness.

use anyhow::{Context, Result};

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

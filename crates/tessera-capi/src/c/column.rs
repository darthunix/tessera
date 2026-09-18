//! `TessDatumColumn` and its Rust reader.

use std::ffi::c_int;
use std::mem::{MaybeUninit, offset_of};
use std::slice;

use anyhow::{Context, Result, ensure};
use tessera_core::RowMaskView;

use crate::DatumInt32Column;

/// `TessDatumColumn` from `tessera/batch.h`: borrowed Datum values and NULL
/// flags indexed by physical row.
#[repr(C)]
#[derive(Debug)]
pub struct DatumColumn {
    /// The size the caller allocated, at least [`DatumColumn::MIN_SIZE`].
    pub struct_size: usize,
    /// One Datum per row.
    pub values: *const u64,
    /// One flag per row.
    pub isnull: *const bool,
    /// The number of rows.
    pub nrows: c_int,
}

impl DatumColumn {
    /// The size through `nrows` (`TESS_DATUM_COLUMN_MIN_SIZE`).
    pub const MIN_SIZE: usize = offset_of!(DatumColumn, nrows) + size_of::<c_int>();

    /// Read the column as int4 with `prepared` as its readiness.
    ///
    /// # Safety
    ///
    /// `values` and `isnull` must point to `nrows` elements that stay valid
    /// and unchanged for the borrow, initialized for every row of
    /// `prepared` (every row, without it), with valid `bool` flags there.
    pub unsafe fn int32<'a>(
        &'a self,
        prepared: Option<RowMaskView<'a>>,
    ) -> Result<DatumInt32Column<'a>> {
        ensure!(
            self.struct_size >= Self::MIN_SIZE,
            "a Datum column is smaller than its required fields"
        );
        let nrows = usize::try_from(self.nrows).context("a column has a negative row count")?;
        let (values, isnull) = if nrows == 0 {
            (&[][..], &[][..])
        } else {
            ensure!(
                !self.values.is_null() && !self.isnull.is_null(),
                "a column has null buffers"
            );
            // SAFETY: the caller guarantees `nrows` valid elements in both
            // buffers for the borrow; `MaybeUninit` admits unprepared rows.
            unsafe {
                (
                    slice::from_raw_parts(self.values.cast::<MaybeUninit<u64>>(), nrows),
                    slice::from_raw_parts(self.isnull.cast::<MaybeUninit<bool>>(), nrows),
                )
            }
        };
        // SAFETY: the caller guarantees initialized values and valid flags
        // for every prepared row, and immutable buffers for the borrow.
        unsafe { DatumInt32Column::try_new(values, isnull, prepared) }
    }
}

#[cfg(test)]
mod tests {
    use super::DatumColumn;

    #[test]
    fn layout_matches_the_header() {
        assert_eq!(size_of::<DatumColumn>(), 32);
        assert_eq!(DatumColumn::MIN_SIZE, 28);
    }
}

//! `TessDatumColumn` and its Rust reader.

use std::ffi::c_int;
use std::mem::{MaybeUninit, offset_of};
use std::slice;

use anyhow::{Context, Result, ensure};
use tessera_core::RowMaskView;

use crate::{DatumInt32Column, DatumInt64Column, DatumIntColumn, DatumNullsColumn, FromDatum};

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
    /// A numeric column's decimals: whether the consumer reads them. The
    /// kernels here read none, so an array of columns keeps C's layout.
    pub accept_decimals: bool,
    /// The rows whose Datum holds a decimal, or null.
    pub decimal_rows: *const u64,
    /// The decimals' scale.
    pub decimal_scale: c_int,
}

impl DatumColumn {
    /// The size through `nrows` (`TESS_DATUM_COLUMN_MIN_SIZE`).
    pub const MIN_SIZE: usize = offset_of!(DatumColumn, nrows) + size_of::<c_int>();

    /// A column of no rows and no decimals, to complete a literal with.
    pub const EMPTY: DatumColumn = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: std::ptr::null(),
        isnull: std::ptr::null(),
        nrows: 0,
        accept_decimals: false,
        decimal_rows: std::ptr::null(),
        decimal_scale: 0,
    };

    /// Read the column as integers of one width with `prepared` as its
    /// readiness.
    ///
    /// # Safety
    ///
    /// `values` and `isnull` must point to `nrows` elements that stay valid
    /// and unchanged for the borrow, initialized for every row of
    /// `prepared` (every row, without it), with valid `bool` flags there.
    pub unsafe fn ints<'a, T: FromDatum>(
        &'a self,
        prepared: Option<RowMaskView<'a>>,
    ) -> Result<DatumIntColumn<'a, T>> {
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
        unsafe { DatumIntColumn::try_new(values, isnull, prepared) }
    }

    /// Read the column as int4 with `prepared` as its readiness.
    ///
    /// # Safety
    ///
    /// As for [`DatumColumn::ints`].
    pub unsafe fn int32<'a>(
        &'a self,
        prepared: Option<RowMaskView<'a>>,
    ) -> Result<DatumInt32Column<'a>> {
        // SAFETY: the caller's contract.
        unsafe { self.ints(prepared) }
    }

    /// Read the column as int8 with `prepared` as its readiness.
    ///
    /// # Safety
    ///
    /// As for [`DatumColumn::ints`].
    pub unsafe fn int64<'a>(
        &'a self,
        prepared: Option<RowMaskView<'a>>,
    ) -> Result<DatumInt64Column<'a>> {
        // SAFETY: the caller's contract.
        unsafe { self.ints(prepared) }
    }

    /// Read the column's NULL flags alone, whatever its type, with
    /// `prepared` as its readiness.
    ///
    /// # Safety
    ///
    /// As for [`DatumColumn::ints`].
    pub unsafe fn nulls<'a>(
        &'a self,
        prepared: Option<RowMaskView<'a>>,
    ) -> Result<DatumNullsColumn<'a>> {
        // SAFETY: the caller's contract.
        unsafe { self.ints(prepared) }
    }
}

/// `TessBytesColumn` from `tessera/batch.h`: a string column in the bytes
/// form, borrowed from its batch. For a row asked that is not `NULL`,
/// `data` points to its bytes, never null, and `lengths` gives their
/// count; the bytes stay valid and unchanged until the batch is released.
/// Rows not asked hold entries of no meaning.
#[repr(C)]
#[derive(Debug)]
pub struct BytesColumn {
    /// The size the caller allocated, at least [`BytesColumn::MIN_SIZE`].
    pub struct_size: usize,
    /// The number of rows of the arrays.
    pub nrows: c_int,
    /// The dictionary's count of values, or 0 without one.
    pub nvalues: u32,
    /// One flag per row.
    pub isnull: *const bool,
    /// One pointer per row: the value's first byte.
    pub data: *const *const u8,
    /// One count of bytes per row.
    pub lengths: *const u32,
    /// One index into the dictionary per row, or null without one.
    pub indexes: *const i32,
    /// The dictionary's identity, 0 without one.
    pub dictionary: u64,
}

impl BytesColumn {
    /// The size through `dictionary` (`TESS_BYTES_COLUMN_MIN_SIZE`).
    pub const MIN_SIZE: usize = offset_of!(BytesColumn, dictionary) + size_of::<u64>();

    /// A column of no rows, to complete a literal with.
    pub const EMPTY: BytesColumn = BytesColumn {
        struct_size: size_of::<BytesColumn>(),
        nrows: 0,
        nvalues: 0,
        isnull: std::ptr::null(),
        data: std::ptr::null(),
        lengths: std::ptr::null(),
        indexes: std::ptr::null(),
        dictionary: 0,
    };
}

#[cfg(test)]
mod tests {
    use std::mem::offset_of;

    use super::{BytesColumn, DatumColumn};

    #[test]
    fn layout_matches_the_header() {
        assert_eq!(size_of::<DatumColumn>(), 48);
        assert_eq!(DatumColumn::MIN_SIZE, 28);
        assert_eq!(offset_of!(DatumColumn, decimal_rows), 32);
    }

    #[test]
    fn the_bytes_form_s_layout_matches_the_header() {
        assert_eq!(size_of::<BytesColumn>(), 56);
        assert_eq!(align_of::<BytesColumn>(), 8);
        assert_eq!(BytesColumn::MIN_SIZE, 56);
        assert_eq!(
            [
                offset_of!(BytesColumn, struct_size),
                offset_of!(BytesColumn, nrows),
                offset_of!(BytesColumn, nvalues),
                offset_of!(BytesColumn, isnull),
                offset_of!(BytesColumn, data),
                offset_of!(BytesColumn, lengths),
                offset_of!(BytesColumn, indexes),
                offset_of!(BytesColumn, dictionary),
            ],
            [0, 8, 12, 16, 24, 32, 40, 48]
        );
    }
}

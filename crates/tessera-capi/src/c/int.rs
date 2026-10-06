//! The int4 and int8 entry points, both sets written by one macro
//! (`int_entry_points!`) so that a change to an entry point is made
//! once for both widths; the sum, which only int4 has here, and the
//! entry points of the library itself (its ABI version, the probe of its
//! layouts and the test of its panic guard).

use std::ffi::c_uint;

use anyhow::{Context, Result};
use tessera_core::RowMaskView;
use tessera_kernels::{int32, int64};

use super::args::{arith_op, compare_op, hash_outputs, inputs, null_keys, outputs, reader};
use super::column::DatumColumn;
use super::mask::Mask;
use super::status::{Code, Status, guard};
use crate::{DatumIntColumn, FromDatum};

/// The ABI version of `tessera/kernels.h` this library implements.
const ABI_VERSION: u32 = 0;

/// `tess_kernels_abi_version`.
#[unsafe(no_mangle)]
pub extern "C" fn tess_kernels_abi_version() -> u32 {
    ABI_VERSION
}

/// `TessKernelsProbe`: a `TessRowMask`'s size.
pub const PROBE_ROW_MASK_SIZE: c_uint = 0;
/// A `TessDatumColumn`'s size.
pub const PROBE_DATUM_COLUMN_SIZE: c_uint = 1;
/// The offset of a `TessDatumColumn`'s `nrows`.
pub const PROBE_DATUM_COLUMN_NROWS_OFFSET: c_uint = 2;
/// A `TessStatus`'s size.
pub const PROBE_STATUS_SIZE: c_uint = 3;
/// The offset of a `TessStatus`'s `message`.
pub const PROBE_STATUS_MESSAGE_OFFSET: c_uint = 4;

/// `tess_kernels_probe`: the size or offset for a `TessKernelsProbe`, or 0.
#[unsafe(no_mangle)]
pub extern "C" fn tess_kernels_probe(kind: c_uint) -> usize {
    match kind {
        PROBE_ROW_MASK_SIZE => size_of::<Mask>(),
        PROBE_DATUM_COLUMN_SIZE => size_of::<DatumColumn>(),
        PROBE_DATUM_COLUMN_NROWS_OFFSET => std::mem::offset_of!(DatumColumn, nrows),
        PROBE_STATUS_SIZE => size_of::<Status>(),
        PROBE_STATUS_MESSAGE_OFFSET => std::mem::offset_of!(Status, message),
        _ => 0,
    }
}

/// `tess_kernels_test_panic`: raise and catch a panic.
///
/// # Safety
///
/// `status` must be null or valid as for every entry point.
#[unsafe(no_mangle)]
#[allow(
    clippy::panic,
    reason = "the C test of the guard needs a panic to catch"
)]
pub unsafe extern "C" fn tess_kernels_test_panic(status: *mut Status) -> Code {
    // SAFETY: the caller's status contract.
    unsafe { guard(status, || panic!("injected panic")) }
}

/// The entry points of one integer width: `$value` is its Rust type, `$sql`
/// the name of its PostgreSQL type, `$kernels` the module of its kernels,
/// and each other key names the C symbol of an entry point.
macro_rules! int_entry_points {
    (
        value: $value:ty,
        sql: $sql:literal,
        kernels: $kernels:ident,
        filter: $filter:ident,
        compare_columns: $compare_columns:ident,
        arith_scalar: $arith_scalar:ident,
        arith_scalar_left: $arith_scalar_left:ident,
        arith_columns: $arith_columns:ident,
        hash: $hash:ident,
        hash_next: $hash_next:ident,
        min: $min:ident,
        max: $max:ident $(,)?
    ) => {
        #[doc = concat!("`", stringify!($filter), "`: keep in `rows` the selected rows whose")]
        /// non-NULL value satisfies `value op scalar`.
        ///
        /// # Safety
        ///
        /// `column` must point to a valid `TessDatumColumn` satisfying
        /// [`DatumColumn::ints`]'s contract with `prepared` (null or a valid
        /// mask) as its readiness; `rows` must point to a valid mask that
        /// nothing else accesses during the call; `status` as for every entry
        /// point.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $filter(
            column: *const DatumColumn,
            prepared: *const Mask,
            rows: *mut Mask,
            op: c_uint,
            scalar: $value,
            status: *mut Status,
        ) -> Code {
            // SAFETY: the caller's status contract.
            unsafe {
                guard(status, || {
                    let op = compare_op(op)?;
                    let column = reader::<$value>(column, prepared)?;
                    let rows = rows.as_mut().context("a null row mask")?;
                    let mut rows = rows.mask()?;
                    $kernels::filter(&column, &mut rows, op, scalar)
                })
            }
        }

        #[doc = concat!("`", stringify!($compare_columns), "`: keep in `rows` the selected rows")]
        #[doc = concat!("where both ", $sql, " columns are non-NULL and `left op right`.")]
        ///
        /// # Safety
        ///
        /// `left` and `right` must point to valid `TessDatumColumn`s
        /// satisfying [`DatumColumn::ints`]'s contract with their readiness
        /// masks (null or valid); `rows` must point to a valid mask that
        /// nothing else accesses during the call; `status` as for every entry
        /// point.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $compare_columns(
            left: *const DatumColumn,
            left_prepared: *const Mask,
            right: *const DatumColumn,
            right_prepared: *const Mask,
            rows: *mut Mask,
            op: c_uint,
            status: *mut Status,
        ) -> Code {
            // SAFETY: the caller's status contract.
            unsafe {
                guard(status, || {
                    let op = compare_op(op)?;
                    let left = reader::<$value>(left, left_prepared)?;
                    let right = reader::<$value>(right, right_prepared)?;
                    let rows = rows.as_mut().context("a null row mask")?;
                    let mut rows = rows.mask()?;
                    $kernels::compare_columns(&left, &right, &mut rows, op)
                })
            }
        }

        #[doc = concat!("`", stringify!($arith_scalar), "`: `column op scalar` into a dense ")]
        #[doc = concat!($sql, " result.")]
        ///
        /// # Safety
        ///
        /// As for [`inputs`] and [`outputs`]; `status` as for every entry
        /// point.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $arith_scalar(
            op: c_uint,
            column: *const DatumColumn,
            scalar: $value,
            prepared: *const Mask,
            rows: *const Mask,
            values: *mut $value,
            non_nulls: *mut Mask,
            status: *mut Status,
        ) -> Code {
            // SAFETY: the caller's contract.
            unsafe {
                guard(status, || {
                    let op = arith_op(op)?;
                    let (column, rows) = inputs::<$value>(column, prepared, rows)?;
                    let (values, mut non_nulls) = outputs(values, non_nulls)?;
                    $kernels::arith_scalar(op, &column, scalar, &rows, values, &mut non_nulls)
                })
            }
        }

        #[doc = concat!("`", stringify!($arith_scalar_left), "`: `scalar op column` into a")]
        #[doc = concat!("dense ", $sql, " result.")]
        ///
        /// # Safety
        ///
        #[doc = concat!("As for [`", stringify!($arith_scalar), "`].")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $arith_scalar_left(
            op: c_uint,
            scalar: $value,
            column: *const DatumColumn,
            prepared: *const Mask,
            rows: *const Mask,
            values: *mut $value,
            non_nulls: *mut Mask,
            status: *mut Status,
        ) -> Code {
            // SAFETY: the caller's contract.
            unsafe {
                guard(status, || {
                    let op = arith_op(op)?;
                    let (column, rows) = inputs::<$value>(column, prepared, rows)?;
                    let (values, mut non_nulls) = outputs(values, non_nulls)?;
                    $kernels::arith_scalar_left(op, scalar, &column, &rows, values, &mut non_nulls)
                })
            }
        }

        #[doc = concat!("`", stringify!($arith_columns), "`: `left op right` row by row into a")]
        #[doc = concat!("dense ", $sql, " result.")]
        ///
        /// # Safety
        ///
        #[doc = concat!("As for [`", stringify!($arith_scalar), "`], for both columns.")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $arith_columns(
            op: c_uint,
            left: *const DatumColumn,
            left_prepared: *const Mask,
            right: *const DatumColumn,
            right_prepared: *const Mask,
            rows: *const Mask,
            values: *mut $value,
            non_nulls: *mut Mask,
            status: *mut Status,
        ) -> Code {
            // SAFETY: the caller's contract.
            unsafe {
                guard(status, || {
                    let op = arith_op(op)?;
                    let (left, rows) = inputs::<$value>(left, left_prepared, rows)?;
                    let right = reader::<$value>(right, right_prepared)?;
                    let (values, mut non_nulls) = outputs(values, non_nulls)?;
                    $kernels::arith_columns(op, &left, &right, &rows, values, &mut non_nulls)
                })
            }
        }

        #[doc = concat!("`", stringify!($hash), "`: hash the first ", $sql, " key of the selected")]
        #[doc = concat!("rows into `hashes` and set `valid`, as PostgreSQL's `hash", $sql, "`")]
        /// does.
        ///
        /// # Safety
        ///
        /// As for [`inputs`] and [`hash_outputs`]; `status` as for every
        /// entry point.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $hash(
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
                    let (column, rows) = inputs::<$value>(column, prepared, rows)?;
                    let (hashes, mut valid) = hash_outputs(hashes, valid)?;
                    $kernels::hash(&column, &rows, nulls, hashes, &mut valid)
                })
            }
        }

        #[doc = concat!("`", stringify!($hash_next), "`: fold the next ", $sql, " key into the")]
        /// hashes of the rows in `valid`, narrowing it.
        ///
        /// # Safety
        ///
        /// As for [`reader`] and [`hash_outputs`]; `status` as for every
        /// entry point.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $hash_next(
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
                    let column = reader::<$value>(column, prepared)?;
                    let (hashes, mut valid) = hash_outputs(hashes, valid)?;
                    $kernels::hash_next(&column, nulls, hashes, &mut valid)
                })
            }
        }

        #[doc = concat!("`", stringify!($min), "`: the least selected non-NULL value, NULL")]
        /// without any.
        ///
        /// # Safety
        ///
        /// As for [`inputs`]; `isnull` and `value` must be writable; `status`
        /// as for every entry point.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $min(
            column: *const DatumColumn,
            prepared: *const Mask,
            rows: *const Mask,
            isnull: *mut bool,
            value: *mut $value,
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
                    |column, rows| $kernels::min(column, rows),
                )
            }
        }

        #[doc = concat!("`", stringify!($max), "`: the greatest selected non-NULL value, NULL")]
        /// without any.
        ///
        /// # Safety
        ///
        #[doc = concat!("As for [`", stringify!($min), "`].")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $max(
            column: *const DatumColumn,
            prepared: *const Mask,
            rows: *const Mask,
            isnull: *mut bool,
            value: *mut $value,
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
                    |column, rows| $kernels::max(column, rows),
                )
            }
        }
    };
}

int_entry_points! {
    value: i32,
    sql: "int4",
    kernels: int32,
    filter: tess_int4_filter,
    compare_columns: tess_int4_compare_columns,
    arith_scalar: tess_int4_arith_scalar,
    arith_scalar_left: tess_int4_arith_scalar_left,
    arith_columns: tess_int4_arith_columns,
    hash: tess_int4_hash,
    hash_next: tess_int4_hash_next,
    min: tess_int4_min,
    max: tess_int4_max,
}

int_entry_points! {
    value: i64,
    sql: "int8",
    kernels: int64,
    filter: tess_int8_filter,
    compare_columns: tess_int8_compare_columns,
    arith_scalar: tess_int8_arith_scalar,
    arith_scalar_left: tess_int8_arith_scalar_left,
    arith_columns: tess_int8_arith_columns,
    hash: tess_int8_hash,
    hash_next: tess_int8_hash_next,
    min: tess_int8_min,
    max: tess_int8_max,
}

/// `tess_int4_sum`: the int8 sum of the selected non-NULL values, NULL
/// without any.
///
/// # Safety
///
/// As for [`inputs`]; `isnull` and `sum` must be writable; `status` as
/// for every entry point.
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

/// The shape of the least and the greatest value of either width.
///
/// # Safety
///
/// As for [`tess_int4_min`].
unsafe fn extreme<T: FromDatum + Default>(
    column: *const DatumColumn,
    prepared: *const Mask,
    rows: *const Mask,
    isnull: *mut bool,
    value: *mut T,
    status: *mut Status,
    kernel: fn(&DatumIntColumn<'_, T>, &RowMaskView<'_>) -> Result<Option<T>>,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let null = isnull.as_mut().context("a null flag")?;
            let out = value.as_mut().context("a null result")?;
            let (column, rows) = inputs(column, prepared, rows)?;
            let found = kernel(&column, &rows)?;
            *null = found.is_none();
            *out = found.unwrap_or_default();
            Ok(())
        })
    }
}

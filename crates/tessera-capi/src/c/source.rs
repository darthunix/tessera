//! What the calendar, decimal, text and float entry points share to read
//! their arguments and to write their results: a call's argument, a column
//! or a scalar with a loop for each ([`Input`], [`Constant`], `with_source!`
//! and `with_sources!`), the reader of a column or a scalar Datum
//! ([`DatumArg`], [`input`]), the selection, a result mask and result
//! slots.

use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::slice;

use anyhow::{Context, Result, ensure};
use tessera_core::{RowMask, RowMaskView};
use tessera_kernels::source::Source;

use super::column::DatumColumn;
use super::mask::Mask;
use crate::FromDatum;

/// A call's argument: a column or a scalar, a loop for each.
pub(super) enum Input<C, S> {
    Column(C),
    Scalar(S),
}

/// A scalar argument, the same for every row. Each family implements its
/// kernels' trait of a source for the scalar it reads.
pub(super) struct Constant<T>(pub(super) T);

/// Run `$body` with `$source` bound to the concrete source of an
/// [`Input`]: the column, or a [`Constant`] of the scalar. With a macro
/// `$columns!` that splits a column into its shapes, as the decimal one
/// does, each shape gets a loop of its own.
macro_rules! with_source {
    ($input:expr, |$source:ident| $body:expr) => {
        match $input {
            $crate::c::source::Input::Column(column) => {
                let $source = &column;
                $body
            }
            $crate::c::source::Input::Scalar(value) => {
                let $source = &$crate::c::source::Constant(value);
                $body
            }
        }
    };
    ($input:expr, $columns:ident!, |$source:ident| $body:expr) => {
        match $input {
            $crate::c::source::Input::Column(column) => $columns!(column, |$source| $body),
            $crate::c::source::Input::Scalar(value) => {
                let $source = &$crate::c::source::Constant(value);
                $body
            }
        }
    };
}

/// [`with_source!`] over two inputs: a loop for each pair of shapes.
macro_rules! with_sources {
    ($left:expr, $right:expr, |$l:ident, $r:ident| $body:expr) => {
        $crate::c::source::with_source!($left, |$l| $crate::c::source::with_source!(
            $right,
            |$r| $body
        ))
    };
    ($left:expr, $right:expr, $columns:ident!, |$l:ident, $r:ident| $body:expr) => {
        $crate::c::source::with_source!($left, $columns!, |$l| {
            $crate::c::source::with_source!($right, $columns!, |$r| $body)
        })
    };
}

pub(super) use {with_source, with_sources};

/// A selection to read.
///
/// # Safety
///
/// `rows` must point to a valid mask, unchanged for `'a`.
#[inline]
pub(super) unsafe fn selection<'a>(rows: *const Mask) -> Result<RowMaskView<'a>> {
    // SAFETY: the caller's contract.
    unsafe { rows.as_ref().context("a null row mask")?.view() }
}

/// A mask to write.
///
/// # Safety
///
/// `mask` must point to a valid mask that nothing else accesses for `'a`.
#[inline]
pub(super) unsafe fn output<'a>(mask: *mut Mask) -> Result<RowMask<'a>> {
    // SAFETY: the caller's contract.
    unsafe { mask.as_mut().context("a null result mask")?.mask() }
}

/// An array of `nrows` slots to write.
///
/// # Safety
///
/// `values` must point to `nrows` writable slots of `T`, possibly
/// uninitialized, that nothing else accesses for `'a`.
#[inline]
pub(super) unsafe fn slots<'a, T>(
    values: *mut T,
    nrows: usize,
) -> Result<&'a mut [MaybeUninit<T>]> {
    if nrows == 0 {
        return Ok(&mut []);
    }
    ensure!(!values.is_null(), "a null result buffer");
    // SAFETY: the caller's contract.
    Ok(unsafe { slice::from_raw_parts_mut(values.cast(), nrows) })
}

/// An argument of the calendar and float entry points (`TessCalendarArg`,
/// `TessFloatArg`): a column, or a scalar Datum when the column is null.
#[repr(C)]
#[derive(Debug)]
pub struct DatumArg {
    /// The column, or null.
    pub column: *const DatumColumn,
    /// The scalar when `column` is null.
    pub scalar: u64,
}

/// A value read from its Datum: an integer as [`FromDatum`] reads it, an
/// interval through the pointer the Datum holds. The pointer read is safe
/// only under the module's contract, so it stays in this trait, private,
/// rather than in the public [`FromDatum`].
pub(super) trait ReadDatum: Copy {
    /// The value a Datum holds.
    fn read_datum(datum: u64) -> Self;
}

impl<T: FromDatum> ReadDatum for T {
    #[inline(always)]
    fn read_datum(datum: u64) -> Self {
        T::from_datum(datum)
    }
}

/// A column's values and flags, read through raw pointers behind one check
/// of the row against the row count.
pub(super) struct Values<'a, T> {
    values: *const u64,
    isnull: *const u8,
    nrows: usize,
    borrow: PhantomData<(&'a (), T)>,
}

impl<T: ReadDatum> Source<T> for Values<'_, T> {
    #[inline(always)]
    fn get(&self, row: usize) -> Option<T> {
        assert!(row < self.nrows, "a row past its column");
        // SAFETY: the row is within the arrays, and the calls read selected
        // rows only, whose flags and non-NULL values the constructor's
        // caller guarantees; a flag is read as its byte.
        unsafe {
            if *self.isnull.add(row) != 0 {
                return None;
            }
            Some(T::read_datum(*self.values.add(row)))
        }
    }
}

impl<T: Copy> Source<T> for Constant<T> {
    #[inline(always)]
    fn get(&self, _row: usize) -> Option<T> {
        Some(self.0)
    }
}

/// A call's argument: a column or a scalar, a loop for each.
pub(super) type DatumInput<'a, T> = Input<Values<'a, T>, T>;

/// Borrow a column of `nrows` rows.
///
/// # Safety
///
/// As the module documentation says of a column.
#[inline]
pub(super) unsafe fn column<'a, T>(
    column: *const DatumColumn,
    nrows: usize,
) -> Result<Values<'a, T>> {
    ensure!(!column.is_null(), "a null column");
    // A caller built against older headers allocates only the fields
    // through `nrows`: no reference to the whole struct is made, and each
    // field of that prefix is read through its own pointer, the size first.
    // SAFETY: the caller's contract; the struct is at least as large as
    // its size field, and `MIN_SIZE` covers the fields read after it.
    let (values, isnull, rows) = unsafe {
        let struct_size = (&raw const (*column).struct_size).read();
        ensure!(
            struct_size >= DatumColumn::MIN_SIZE,
            "a Datum column is smaller than its required fields"
        );
        (
            (&raw const (*column).values).read(),
            (&raw const (*column).isnull).read(),
            (&raw const (*column).nrows).read(),
        )
    };
    ensure!(
        usize::try_from(rows).ok() == Some(nrows),
        "a column has another row count than its rows"
    );
    ensure!(
        nrows == 0 || (!values.is_null() && !isnull.is_null()),
        "a column has null buffers"
    );
    Ok(Values {
        values,
        isnull: isnull.cast(),
        nrows,
        borrow: PhantomData,
    })
}

/// The argument of a call with `nrows` rows.
///
/// # Safety
///
/// As the module documentation says of an argument.
#[inline]
pub(super) unsafe fn input<'a, T: ReadDatum>(
    arg: *const DatumArg,
    nrows: usize,
) -> Result<DatumInput<'a, T>> {
    // SAFETY: the caller's contract.
    unsafe {
        let arg = arg.as_ref().context("a null argument")?;
        Ok(if arg.column.is_null() {
            Input::Scalar(T::read_datum(arg.scalar))
        } else {
            Input::Column(column(arg.column, nrows)?)
        })
    }
}

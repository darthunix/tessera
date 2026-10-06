//! What the calendar, decimal and text entry points share to read their
//! arguments and to write their results: a call's argument, a column or a
//! scalar with a loop for each ([`Input`], [`Constant`], `with_source!`
//! and `with_sources!`), the selection, a result mask and result slots.

use std::mem::MaybeUninit;
use std::slice;

use anyhow::{Context, Result, ensure};
use tessera_core::{RowMask, RowMaskView};

use super::mask::Mask;

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

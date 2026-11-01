//! The float entry points, declared in `include/tessera/float.h`: the
//! comparisons, arithmetic and casts of float4 and float8 computed by
//! [`tessera_kernels::float`].
//!
//! # Arguments
//!
//! A `TessFloatArg` points to a column or holds a scalar Datum, as a
//! `TessCalendarArg` does ([`DatumArg`]). A column must be a valid
//! `TessDatumColumn` of the call's row count whose `values` and `isnull`
//! hold that many elements, unchanged for the call; a selected row's flag
//! must be initialized and, when not NULL, its value. A float8 is read as
//! its Datum's bits, a float4 as the low 32 bits, an int4 (or int2) as the
//! low 32 bits and an int8 as the Datum. A float is read through the
//! reader's own trait of a Datum's value, as an interval is, and the
//! public [`crate::FromDatum`] stays the integers' that the column
//! readers of the integer kernels take.

use std::ffi::{c_uint, c_void};

use anyhow::{Result, bail};
use tessera_kernels::float::{self, FloatOp, Integer};

use super::args::compare_op;
use super::mask::Mask;
use super::source::{
    DatumArg, ReadDatum, input, output, selection, slots, with_source, with_sources,
};
use super::status::{Code, Status, guard};

/// `TessFloatArg`: a column, or a scalar Datum when it is null.
pub type FloatArg = DatumArg;

impl ReadDatum for f64 {
    /// The float8 whose bits the Datum holds.
    #[inline(always)]
    fn read_datum(datum: u64) -> Self {
        f64::from_bits(datum)
    }
}

impl ReadDatum for f32 {
    /// The float4 whose bits are the Datum's low 32 bits.
    #[inline(always)]
    fn read_datum(datum: u64) -> Self {
        f32::from_bits(datum as u32)
    }
}

/// A `TessFloatType` value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Type {
    Float8,
    Float4,
    Int4,
    Int2,
    Int8,
}

fn type_of(code: c_uint) -> Result<Type> {
    Ok(match code {
        0 => Type::Float8,
        1 => Type::Float4,
        2 => Type::Int4,
        3 => Type::Int2,
        4 => Type::Int8,
        _ => bail!("an unknown float type {code}"),
    })
}

/// A `TessFloatOp` value.
fn op_of(code: c_uint) -> Result<FloatOp> {
    Ok(match code {
        0 => FloatOp::Add,
        1 => FloatOp::Sub,
        2 => FloatOp::Mul,
        3 => FloatOp::Div,
        4 => FloatOp::Neg,
        5 => FloatOp::Abs,
        _ => bail!("an unknown float operation {code}"),
    })
}

/// `tess_float_compare`: keep in `rows` the selected rows where `left op
/// right` holds, both not NULL, in the core's order of floats.
///
/// # Safety
///
/// `left` and `right` must be valid arguments (see the module
/// documentation) of `rows`' row count, and `rows` a valid mask that
/// nothing else accesses during the call; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_float_compare(
    op: c_uint,
    left_type: c_uint,
    left: *const FloatArg,
    right_type: c_uint,
    right: *const FloatArg,
    rows: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let op = compare_op(op)?;
            let mut rows = output(rows)?;
            let nrows = rows.as_view().nrows();
            match (type_of(left_type)?, type_of(right_type)?) {
                (Type::Float8, Type::Float8) => {
                    let (left, right) = (input::<f64>(left, nrows)?, input::<f64>(right, nrows)?);
                    with_sources!(left, right, |l, r| float::compare_rows(op, l, r, &mut rows))
                }
                (Type::Float4, Type::Float4) => {
                    let (left, right) = (input::<f32>(left, nrows)?, input::<f32>(right, nrows)?);
                    with_sources!(left, right, |l, r| float::compare_rows(op, l, r, &mut rows))
                }
                (Type::Float4, Type::Float8) => {
                    let (left, right) = (input::<f32>(left, nrows)?, input::<f64>(right, nrows)?);
                    with_sources!(left, right, |l, r| float::compare_rows(op, l, r, &mut rows))
                }
                (Type::Float8, Type::Float4) => {
                    let (left, right) = (input::<f64>(left, nrows)?, input::<f32>(right, nrows)?);
                    with_sources!(left, right, |l, r| float::compare_rows(op, l, r, &mut rows))
                }
                (left, right) => bail!("a float comparison of {left:?} and {right:?}"),
            }
        })
    }
}

/// `tess_float_arith`: `left op right`, or `op left` for a unary
/// operation, over the selected rows, a float4 result when both are
/// float4 and a float8 result otherwise, written as Datums.
///
/// # Safety
///
/// `left` and, for an operation of two, `right` must be valid arguments
/// (see the module documentation) of `rows`' row count; `rows` a valid
/// mask; `values` must point to as many writable Datums and `non_nulls`
/// to a valid mask, neither accessed by anything else during the call;
/// `status` as for every entry point.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments, reason = "the C signature")]
pub unsafe extern "C" fn tess_float_arith(
    op: c_uint,
    left_type: c_uint,
    left: *const FloatArg,
    right_type: c_uint,
    right: *const FloatArg,
    rows: *const Mask,
    values: *mut u64,
    non_nulls: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let op = op_of(op)?;
            let rows = selection(rows)?;
            let nrows = rows.nrows();
            let values = slots(values, nrows)?;
            let mut non_nulls = output(non_nulls)?;
            let non_nulls = &mut non_nulls;
            let left_type = type_of(left_type)?;
            if op.is_unary() {
                return match left_type {
                    Type::Float8 => with_source!(input::<f64>(left, nrows)?, |x| {
                        float::unary_float8(op, x, rows, values, non_nulls)
                    }),
                    Type::Float4 => with_source!(input::<f32>(left, nrows)?, |x| {
                        float::unary_float4(op, x, rows, values, non_nulls)
                    }),
                    other => bail!("{op:?} of {other:?}"),
                };
            }
            match (left_type, type_of(right_type)?) {
                (Type::Float8, Type::Float8) => {
                    let (left, right) = (input::<f64>(left, nrows)?, input::<f64>(right, nrows)?);
                    with_sources!(left, right, |l, r| float::binary_float8(
                        op, l, r, rows, values, non_nulls
                    ))
                }
                (Type::Float4, Type::Float4) => {
                    let (left, right) = (input::<f32>(left, nrows)?, input::<f32>(right, nrows)?);
                    with_sources!(left, right, |l, r| float::binary_float4(
                        op, l, r, rows, values, non_nulls
                    ))
                }
                (Type::Float4, Type::Float8) => {
                    let (left, right) = (input::<f32>(left, nrows)?, input::<f64>(right, nrows)?);
                    with_sources!(left, right, |l, r| float::binary_float8(
                        op, l, r, rows, values, non_nulls
                    ))
                }
                (Type::Float8, Type::Float4) => {
                    let (left, right) = (input::<f64>(left, nrows)?, input::<f32>(right, nrows)?);
                    with_sources!(left, right, |l, r| float::binary_float8(
                        op, l, r, rows, values, non_nulls
                    ))
                }
                (left, right) => bail!("{op:?} of {left:?} and {right:?}"),
            }
        })
    }
}

/// `tess_float_cast`: `arg`, of type `from`, cast to `to` over the selected
/// rows: an integer to a float, a float to the other width, or a float to
/// an integer.
///
/// # Safety
///
/// `arg` must be a valid argument (see the module documentation) of
/// `rows`' row count; `rows` a valid mask; `values` must point to as many
/// writable int32 for an int4 or int2 result, Datums otherwise, and
/// `non_nulls` to a valid mask, neither accessed by anything else during
/// the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_float_cast(
    from: c_uint,
    to: c_uint,
    arg: *const FloatArg,
    rows: *const Mask,
    values: *mut c_void,
    non_nulls: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let rows = selection(rows)?;
            let nrows = rows.nrows();
            let mut non_nulls = output(non_nulls)?;
            let non_nulls = &mut non_nulls;
            // An int2 argument's Datum holds its value as an int4's does.
            let from = match type_of(from)? {
                Type::Int2 => Type::Int4,
                other => other,
            };
            match (from, type_of(to)?) {
                (Type::Int4, Type::Float8) => with_source!(input::<i32>(arg, nrows)?, |x| {
                    float::to_float8(x, rows, slots(values.cast(), nrows)?, non_nulls)
                }),
                (Type::Int8, Type::Float8) => with_source!(input::<i64>(arg, nrows)?, |x| {
                    float::to_float8(x, rows, slots(values.cast(), nrows)?, non_nulls)
                }),
                (Type::Float4, Type::Float8) => with_source!(input::<f32>(arg, nrows)?, |x| {
                    float::to_float8(x, rows, slots(values.cast(), nrows)?, non_nulls)
                }),
                (Type::Int4, Type::Float4) => with_source!(input::<i32>(arg, nrows)?, |x| {
                    float::to_float4(x, rows, slots(values.cast(), nrows)?, non_nulls)
                }),
                (Type::Int8, Type::Float4) => with_source!(input::<i64>(arg, nrows)?, |x| {
                    float::to_float4(x, rows, slots(values.cast(), nrows)?, non_nulls)
                }),
                (Type::Float8, Type::Float4) => with_source!(input::<f64>(arg, nrows)?, |x| {
                    float::to_float4(x, rows, slots(values.cast(), nrows)?, non_nulls)
                }),
                (Type::Float8, to @ (Type::Int4 | Type::Int2)) => {
                    let to = integer(to);
                    with_source!(input::<f64>(arg, nrows)?, |x| {
                        float::to_int32(to, x, rows, slots(values.cast(), nrows)?, non_nulls)
                    })
                }
                (Type::Float4, to @ (Type::Int4 | Type::Int2)) => {
                    let to = integer(to);
                    with_source!(input::<f32>(arg, nrows)?, |x| {
                        float::to_int32(to, x, rows, slots(values.cast(), nrows)?, non_nulls)
                    })
                }
                (Type::Float8, Type::Int8) => with_source!(input::<f64>(arg, nrows)?, |x| {
                    float::to_int64(x, rows, slots(values.cast(), nrows)?, non_nulls)
                }),
                (Type::Float4, Type::Int8) => with_source!(input::<f32>(arg, nrows)?, |x| {
                    float::to_int64(x, rows, slots(values.cast(), nrows)?, non_nulls)
                }),
                (from, to) => bail!("a float cast of {from:?} to {to:?}"),
            }
        })
    }
}

/// The integer type of a cast's result.
fn integer(to: Type) -> Integer {
    match to {
        Type::Int2 => Integer::Int2,
        Type::Int4 => Integer::Int4,
        _ => Integer::Int8,
    }
}

//! The float entry points called with raw pointers, as C calls them: every
//! operation, every pair of types and every shape of the arguments, NULLs,
//! the errors as statuses, and the calls a caller gets wrong.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use std::ffi::c_void;
use std::ptr;

use tessera_capi::c::{
    Code, DatumColumn, FloatArg, Mask, Status, tess_float_arith, tess_float_cast,
    tess_float_compare,
};
use tessera_kernels::float::{FloatError, FloatOp, Integer, Number, Rounded, arith, compare};
use tessera_kernels::ops::CompareOp;

const FLOAT8: u32 = 0;
const FLOAT4: u32 = 1;
const INT4: u32 = 2;
const INT2: u32 = 3;
const INT8: u32 = 4;

const COMPARES: [CompareOp; 6] = [
    CompareOp::Eq,
    CompareOp::Ne,
    CompareOp::Lt,
    CompareOp::Le,
    CompareOp::Gt,
    CompareOp::Ge,
];

/// The values of a test column of float8, one a row; row 1 is NULL.
const F8: [f64; 8] = [1.5, 0.0, f64::NAN, -0.0, -2.5, f64::INFINITY, 1e10, 3.0];
/// The float4 column, row 2 NULL.
const F4: [f32; 8] = [1.5, -1.0, 0.0, f32::NAN, 2.5, -0.0, f32::NEG_INFINITY, 3.0];

fn column(values: &[u64], isnull: &[bool]) -> DatumColumn {
    DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: values.len() as i32,
        ..DatumColumn::EMPTY
    }
}

fn mask(nrows: usize, words: &mut [u64]) -> Mask {
    Mask {
        nrows: nrows as i32,
        bits: words.as_mut_ptr(),
    }
}

/// A float4 Datum with garbage above its low 32 bits, which a reader must
/// ignore.
fn float4_datum(value: f32) -> u64 {
    0xdead_beef_0000_0000 | u64::from(value.to_bits())
}

/// A float4 Datum by another derivation than the kernels': the bits, with
/// the high half filled when the sign bit is set.
fn datum4(value: f32) -> u64 {
    let bits = u64::from(value.to_bits());
    if bits & 0x8000_0000 == 0 {
        bits
    } else {
        bits | 0xffff_ffff_0000_0000
    }
}

/// The two test columns as Datums, with their NULLs.
struct Columns {
    f8: Vec<u64>,
    f8_nulls: [bool; 8],
    f4: Vec<u64>,
    f4_nulls: [bool; 8],
}

impl Columns {
    fn new() -> Self {
        let mut f8_nulls = [false; 8];
        f8_nulls[1] = true;
        let mut f4_nulls = [false; 8];
        f4_nulls[2] = true;
        Self {
            f8: F8.iter().map(|x| x.to_bits()).collect(),
            f8_nulls,
            f4: F4.iter().map(|&x| float4_datum(x)).collect(),
            f4_nulls,
        }
    }
}

/// An argument of a test: the type code, the column or a scalar, and the
/// value of a row (`None` for NULL) as a float8.
#[derive(Clone, Copy)]
enum Shape {
    Column,
    Scalar,
}

struct Arg {
    code: u32,
    arg: FloatArg,
    value: Box<dyn Fn(usize) -> Option<f64>>,
}

fn arg(columns: &Columns, code: u32, shape: Shape, f8: &DatumColumn, f4: &DatumColumn) -> Arg {
    match (code, shape) {
        (FLOAT8, Shape::Column) => {
            let nulls = columns.f8_nulls;
            Arg {
                code,
                arg: FloatArg {
                    column: f8,
                    scalar: 0,
                },
                value: Box::new(move |row| (!nulls[row]).then_some(F8[row])),
            }
        }
        (FLOAT4, Shape::Column) => {
            let nulls = columns.f4_nulls;
            Arg {
                code,
                arg: FloatArg {
                    column: f4,
                    scalar: 0,
                },
                value: Box::new(move |row| (!nulls[row]).then_some(f64::from(F4[row]))),
            }
        }
        (FLOAT8, Shape::Scalar) => Arg {
            code,
            arg: FloatArg {
                column: ptr::null(),
                scalar: 2.5_f64.to_bits(),
            },
            value: Box::new(|_| Some(2.5)),
        },
        _ => Arg {
            code,
            arg: FloatArg {
                column: ptr::null(),
                scalar: float4_datum(-0.0),
            },
            value: Box::new(|_| Some(-0.0)),
        },
    }
}

const PAIRS: [(u32, u32); 4] = [
    (FLOAT8, FLOAT8),
    (FLOAT4, FLOAT4),
    (FLOAT4, FLOAT8),
    (FLOAT8, FLOAT4),
];
const SHAPES: [(Shape, Shape); 3] = [
    (Shape::Column, Shape::Column),
    (Shape::Column, Shape::Scalar),
    (Shape::Scalar, Shape::Column),
];

#[test]
fn comparisons_of_every_pair_and_shape_keep_the_rows_that_hold() {
    let columns = Columns::new();
    let f8 = column(&columns.f8, &columns.f8_nulls);
    let f4 = column(&columns.f4, &columns.f4_nulls);
    for (left_code, right_code) in PAIRS {
        for (left_shape, right_shape) in SHAPES {
            let left = arg(&columns, left_code, left_shape, &f8, &f4);
            let right = arg(&columns, right_code, right_shape, &f8, &f4);
            for (op_code, op) in COMPARES.into_iter().enumerate() {
                // Every row but row 7.
                let mut rows = [0x7f_u64];
                let mut status = Status::new();
                // SAFETY: local columns, mask and status.
                let code = unsafe {
                    tess_float_compare(
                        op_code as u32,
                        left.code,
                        &left.arg,
                        right.code,
                        &right.arg,
                        &mut mask(8, &mut rows),
                        &raw mut status,
                    )
                };
                assert_eq!(code, Code::Ok, "{}", status.message());
                for row in 0..8 {
                    let want = row < 7
                        && matches!(((left.value)(row), (right.value)(row)),
                            (Some(l), Some(r)) if compare(op, l, r));
                    assert_eq!(rows[0] >> row & 1 == 1, want, "{op:?} row {row}");
                }
            }
        }
    }
}

/// The bits a float result of `op` over a row has: a float4's when both
/// operands are float4, a float8's otherwise; or its error.
fn arith_bits(op: FloatOp, both_float4: bool, left: f64, right: f64) -> Result<u64, FloatError> {
    if both_float4 {
        arith(op, left as f32, right as f32).map(datum4)
    } else {
        arith(op, left, right).map(f64::to_bits)
    }
}

#[test]
fn arithmetic_of_every_pair_and_shape_writes_the_core_results() {
    let columns = Columns::new();
    let f8 = column(&columns.f8, &columns.f8_nulls);
    let f4 = column(&columns.f4, &columns.f4_nulls);
    for (left_code, right_code) in PAIRS {
        for (left_shape, right_shape) in SHAPES {
            let left = arg(&columns, left_code, left_shape, &f8, &f4);
            let right = arg(&columns, right_code, right_shape, &f8, &f4);
            let both_float4 = left_code == FLOAT4 && right_code == FLOAT4;
            for (op_code, op) in [FloatOp::Add, FloatOp::Sub, FloatOp::Mul, FloatOp::Div]
                .into_iter()
                .enumerate()
            {
                let rows = [0xff_u64];
                let mut values = [0_u64; 8];
                let mut present = [0_u64];
                let mut status = Status::new();
                // SAFETY: local columns, masks, buffers and status.
                let code = unsafe {
                    tess_float_arith(
                        op_code as u32,
                        left.code,
                        &left.arg,
                        right.code,
                        &right.arg,
                        &mask(8, &mut rows.clone()),
                        values.as_mut_ptr(),
                        &mut mask(8, &mut present),
                        &raw mut status,
                    )
                };
                let results: Vec<_> = (0..8)
                    .map(|row| match ((left.value)(row), (right.value)(row)) {
                        (Some(l), Some(r)) => Some(arith_bits(op, both_float4, l, r)),
                        _ => None,
                    })
                    .collect();
                match results.iter().flatten().find_map(|result| result.err()) {
                    Some(error) => {
                        assert_eq!(code, Code::DataException, "{op:?}");
                        assert_eq!(
                            (status.sqlstate(), status.message()),
                            (error.sqlstate().to_owned(), error.to_string())
                        );
                    }
                    None => {
                        assert_eq!(code, Code::Ok, "{}", status.message());
                        for (row, result) in results.iter().enumerate() {
                            assert_eq!(present[0] >> row & 1 == 1, result.is_some(), "row {row}");
                            if let Some(Ok(bits)) = result {
                                let nan = |bits: u64| {
                                    if both_float4 {
                                        f32::from_bits(bits as u32).is_nan()
                                    } else {
                                        f64::from_bits(bits).is_nan()
                                    }
                                };
                                assert!(
                                    values[row] == *bits || (nan(values[row]) && nan(*bits)),
                                    "{op:?} row {row}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn negation_and_abs_take_one_argument() {
    let columns = Columns::new();
    let f8 = column(&columns.f8, &columns.f8_nulls);
    let f4 = column(&columns.f4, &columns.f4_nulls);
    for (op_code, op) in [(4, FloatOp::Neg), (5, FloatOp::Abs)] {
        for (code, float4) in [(FLOAT8, false), (FLOAT4, true)] {
            let left = arg(&columns, code, Shape::Column, &f8, &f4);
            let rows = [0xff_u64];
            let mut values = [0_u64; 8];
            let mut present = [0_u64];
            let mut status = Status::new();
            // SAFETY: local columns, masks, buffers and status; no right
            // argument for an operation of one.
            let outcome = unsafe {
                tess_float_arith(
                    op_code,
                    code,
                    &left.arg,
                    99,
                    ptr::null(),
                    &mask(8, &mut rows.clone()),
                    values.as_mut_ptr(),
                    &mut mask(8, &mut present),
                    &raw mut status,
                )
            };
            assert_eq!(outcome, Code::Ok, "{}", status.message());
            for (row, &got) in values.iter().enumerate() {
                let value = (left.value)(row);
                assert_eq!(present[0] >> row & 1 == 1, value.is_some());
                if let Some(x) = value {
                    let want = arith_bits(op, float4, x, 0.0).unwrap();
                    let nan = |bits: u64| {
                        if float4 {
                            f32::from_bits(bits as u32).is_nan()
                        } else {
                            f64::from_bits(bits).is_nan()
                        }
                    };
                    let same = got == want || (nan(got) && nan(want));
                    assert!(same, "{op:?} {x}: {:#x} {want:#x}", got);
                }
            }
        }
    }
}

/// Run a cast of `arg` from `from` to `to` over its 4 rows, all selected.
fn cast(
    from: u32,
    to: u32,
    arg: &FloatArg,
    values: *mut c_void,
    present: &mut [u64; 1],
) -> (Code, Status) {
    let rows = [0xf_u64];
    let mut status = Status::new();
    // SAFETY: local columns, masks and status; `values` holds 4 elements of
    // the result's width.
    let code = unsafe {
        tess_float_cast(
            from,
            to,
            arg,
            &mask(4, &mut rows.clone()),
            values,
            &mut mask(4, &mut *present),
            &raw mut status,
        )
    };
    (code, status)
}

#[test]
fn casts_between_floats_and_integers() {
    let isnull = [false, false, true, false];
    // Integers to floats, int2 read as int4.
    let ints: Vec<u64> = [-7_i32, 16_777_217, 0, i32::MIN]
        .iter()
        .map(|&x| x as u32 as u64)
        .collect();
    let longs: Vec<u64> = [-7_i64, i64::MAX, 0, i64::MIN]
        .iter()
        .map(|&x| x as u64)
        .collect();
    let int_column = column(&ints, &isnull);
    let long_column = column(&longs, &isnull);
    for (from, column, as_f64) in [
        (
            INT4,
            &int_column,
            (|d: u64| (d as u32 as i32).float8()) as fn(u64) -> f64,
        ),
        (INT2, &int_column, |d: u64| (d as u32 as i32).float8()),
        (INT8, &long_column, |d: u64| (d as i64).float8()),
    ] {
        let source = FloatArg { column, scalar: 0 };
        let mut values = [0_u64; 4];
        let mut present = [0];
        let (code, status) = cast(
            from,
            FLOAT8,
            &source,
            values.as_mut_ptr().cast(),
            &mut present,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(present, [0b1011]);
        let column_values = if from == INT8 { &longs } else { &ints };
        for row in [0, 1, 3] {
            assert_eq!(
                values[row],
                as_f64(column_values[row]).to_bits(),
                "{from} row {row}"
            );
        }
        let (code, status) = cast(
            from,
            FLOAT4,
            &source,
            values.as_mut_ptr().cast(),
            &mut present,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        for row in [0, 1, 3] {
            let want = if from == INT8 {
                (column_values[row] as i64).float4()
            } else {
                (column_values[row] as u32 as i32).float4()
            };
            assert_eq!(values[row], datum4(want.unwrap()), "{from} row {row}");
        }
    }
    // Floats to floats and to integers.
    let doubles: Vec<u64> = [2.5_f64, -32768.5, 0.0, 1e6]
        .iter()
        .map(|x| x.to_bits())
        .collect();
    let singles: Vec<u64> = [2.5_f32, -32768.5, 0.0, 1e6]
        .iter()
        .map(|&x| float4_datum(x))
        .collect();
    let double_column = column(&doubles, &isnull);
    let single_column = column(&singles, &isnull);
    let mut values = [0_u64; 4];
    let mut present = [0];
    let (code, _) = cast(
        FLOAT4,
        FLOAT8,
        &FloatArg {
            column: &single_column,
            scalar: 0,
        },
        values.as_mut_ptr().cast(),
        &mut present,
    );
    assert_eq!(code, Code::Ok);
    assert_eq!(values[3], f64::from(1e6_f32).to_bits());
    let (code, _) = cast(
        FLOAT8,
        FLOAT4,
        &FloatArg {
            column: &double_column,
            scalar: 0,
        },
        values.as_mut_ptr().cast(),
        &mut present,
    );
    assert_eq!(code, Code::Ok);
    assert_eq!(values[1], 0xffff_ffff_c700_0080);
    assert_eq!(values[1], datum4(-32768.5));
    for (from, column) in [(FLOAT8, &double_column), (FLOAT4, &single_column)] {
        let source = FloatArg { column, scalar: 0 };
        let mut ints = [0_i32; 4];
        let (code, status) = cast(from, INT4, &source, ints.as_mut_ptr().cast(), &mut present);
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(
            (ints[0], ints[1], ints[3], present[0]),
            (2, -32768, 1_000_000, 0b1011)
        );
        let mut longs = [0_i64; 4];
        let (code, _) = cast(from, INT8, &source, longs.as_mut_ptr().cast(), &mut present);
        assert_eq!(code, Code::Ok);
        assert_eq!((longs[0], longs[1], longs[3]), (2, -32768, 1_000_000));
        // 1e6 is past int2's range: the first row that fails.
        let (code, status) = cast(from, INT2, &source, ints.as_mut_ptr().cast(), &mut present);
        assert_eq!(code, Code::DataException);
        assert_eq!(
            (status.sqlstate(), status.message()),
            ("22003".to_owned(), "smallint out of range".to_owned())
        );
    }
}

#[test]
fn errors_come_back_with_the_core_status() {
    let big = [
        1e300_f64.to_bits(),
        1e-300_f64.to_bits(),
        0_f64.to_bits(),
        1e-300_f64.to_bits(),
    ];
    let isnull = [false; 4];
    let doubles = column(&big, &isnull);
    let source = FloatArg {
        column: &doubles,
        scalar: 0,
    };
    let mut values = [0_u64; 4];
    let mut present = [0];
    let (code, status) = cast(
        FLOAT8,
        FLOAT4,
        &source,
        values.as_mut_ptr().cast(),
        &mut present,
    );
    assert_eq!(
        (code, status.message()),
        (
            Code::DataException,
            "value out of range: overflow".to_owned()
        )
    );
    let mut ints = [0_i32; 4];
    let (code, status) = cast(
        FLOAT8,
        INT4,
        &source,
        ints.as_mut_ptr().cast(),
        &mut present,
    );
    assert_eq!(
        (code, status.message()),
        (Code::DataException, "integer out of range".to_owned())
    );
    let mut longs = [0_i64; 4];
    let (code, status) = cast(
        FLOAT8,
        INT8,
        &source,
        longs.as_mut_ptr().cast(),
        &mut present,
    );
    assert_eq!(
        (code, status.message()),
        (Code::DataException, "bigint out of range".to_owned())
    );
    let zero = FloatArg {
        column: ptr::null(),
        scalar: 0.0_f64.to_bits(),
    };
    let rows = [0xf_u64];
    let mut status = Status::new();
    // SAFETY: local columns, masks, buffers and status.
    let code = unsafe {
        tess_float_arith(
            3,
            FLOAT8,
            &source,
            FLOAT8,
            &zero,
            &mask(4, &mut rows.clone()),
            values.as_mut_ptr(),
            &mut mask(4, &mut present),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::DataException);
    assert_eq!(
        (status.sqlstate(), status.message()),
        ("22012".to_owned(), "division by zero".to_owned())
    );
    assert_eq!(
        1e300_f64.integer(Integer::Int4),
        Err(FloatError::IntegerOutOfRange)
    );
}

#[test]
fn a_call_a_caller_gets_wrong_is_an_internal_error() {
    let one = [1.0_f64.to_bits(); 4];
    let isnull = [false; 4];
    let doubles = column(&one, &isnull);
    let source = FloatArg {
        column: &doubles,
        scalar: 0,
    };
    let ints = [1_u64; 4];
    let int_column = column(&ints, &isnull);
    let int_source = FloatArg {
        column: &int_column,
        scalar: 0,
    };
    let mut values = [0_u64; 4];
    let mut present = [0];
    let wrong = |code: Code, status: &Status| {
        assert_eq!(
            (code, status.sqlstate()),
            (Code::InvalidArgument, "XX000".to_owned()),
            "{}",
            status.message()
        );
    };
    // Casts with no float, or of a type that is no type.
    for (from, to) in [
        (INT4, INT8),
        (INT8, INT4),
        (FLOAT8, FLOAT8),
        (FLOAT4, FLOAT4),
        (9, FLOAT8),
        (FLOAT8, 9),
    ] {
        let (code, status) = cast(
            from,
            to,
            if from == INT4 || from == INT8 {
                &int_source
            } else {
                &source
            },
            values.as_mut_ptr().cast(),
            &mut present,
        );
        wrong(code, &status);
    }
    let rows = [0xf_u64];
    for (op, left_type, right_type) in [
        (7, FLOAT8, FLOAT8),
        (0, INT4, FLOAT8),
        (0, FLOAT8, INT8),
        (4, INT4, FLOAT8),
    ] {
        let mut status = Status::new();
        // SAFETY: local columns, masks, buffers and status.
        let code = unsafe {
            tess_float_arith(
                op,
                left_type,
                &source,
                right_type,
                &source,
                &mask(4, &mut rows.clone()),
                values.as_mut_ptr(),
                &mut mask(4, &mut present),
                &raw mut status,
            )
        };
        wrong(code, &status);
    }
    for (op, left_type, right_type) in [(6, FLOAT8, FLOAT8), (0, INT4, FLOAT8), (0, FLOAT8, INT2)] {
        let mut status = Status::new();
        let mut kept = rows;
        // SAFETY: local columns, mask and status.
        let code = unsafe {
            tess_float_compare(
                op,
                left_type,
                &source,
                right_type,
                &source,
                &mut mask(4, &mut kept),
                &raw mut status,
            )
        };
        wrong(code, &status);
    }
    // A column of another row count than the selection.
    let short = column(&one[..3], &isnull[..3]);
    let (code, status) = cast(
        FLOAT8,
        FLOAT4,
        &FloatArg {
            column: &short,
            scalar: 0,
        },
        values.as_mut_ptr().cast(),
        &mut present,
    );
    wrong(code, &status);
}

/// A column as a caller built against the first headers allocates it:
/// the fields through `nrows` and no more.
#[repr(C)]
struct ShortColumn {
    struct_size: usize,
    values: *const u64,
    isnull: *const bool,
    nrows: i32,
}

#[test]
fn a_column_of_only_the_required_fields_is_read() {
    let values = [1.0_f64.to_bits()];
    let isnull = [false];
    let short = Box::new(ShortColumn {
        struct_size: size_of::<ShortColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 1,
    });
    let left = FloatArg {
        column: (&raw const *short).cast::<DatumColumn>(),
        scalar: 0,
    };
    let right = FloatArg {
        column: ptr::null(),
        scalar: 2.0_f64.to_bits(),
    };
    let rows = [1_u64];
    let mut out = [0_u64];
    let mut present = [0_u64];
    let mut status = Status::new();
    // SAFETY: a column of the required fields, which the ABI accepts; local
    // masks, buffers and status.
    let code = unsafe {
        tess_float_arith(
            0,
            FLOAT8,
            &left,
            FLOAT8,
            &right,
            &mask(1, &mut rows.clone()),
            out.as_mut_ptr(),
            &mut mask(1, &mut present),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!((out[0], present[0]), (3.0_f64.to_bits(), 1));
}

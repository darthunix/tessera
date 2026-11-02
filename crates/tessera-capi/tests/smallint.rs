//! The smallint entry points called with raw pointers, as C calls them:
//! every operation and shape over Datum columns of smallints, NULLs and
//! unselected rows, a result past the range, and the error of the first
//! row that fails, a range error or a division by zero, whichever comes
//! first. Miri runs it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use std::ptr;

use tessera_capi::c::{
    Code, DatumColumn, Mask, Status, tess_int2_arith_columns, tess_int2_arith_scalar,
    tess_int2_arith_scalar_left,
};

/// `TessArithOp`: + - * / %.
const OPS: [u32; 5] = [0, 1, 2, 3, 4];

/// PostgreSQL's smallint operators, `None` past the range, or the error.
fn model(op: u32, a: i16, b: i16) -> Result<i32, Code> {
    let result = match op {
        0 => a.checked_add(b),
        1 => a.checked_sub(b),
        2 => a.checked_mul(b),
        3 | 4 if b == 0 => return Err(Code::DivisionByZero),
        3 => a.checked_div(b),
        _ => Some(a.wrapping_rem(b)),
    };
    result.map(i32::from).ok_or(Code::IntegerOutOfRange)
}

/// A smallint Datum, sign-extended as Int16GetDatum makes it.
fn datum(value: i16) -> u64 {
    i64::from(value) as u64
}

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

/// A call's shape: a scalar on the right, on the left, or two columns.
#[derive(Clone, Copy, Debug)]
enum Shape {
    Right(i16),
    Left(i16),
    Columns,
}

/// Call one entry point over the rows `selected`; the outcome, the values
/// and the non-NULL words.
fn call(
    op: u32,
    shape: Shape,
    left: (&[i16], &[bool]),
    right: (&[i16], &[bool]),
    selected: &[u64],
) -> (Code, String, Vec<i32>, Vec<u64>) {
    let nrows = left.0.len();
    let left_datums: Vec<u64> = left.0.iter().map(|&v| datum(v)).collect();
    let right_datums: Vec<u64> = right.0.iter().map(|&v| datum(v)).collect();
    let left_column = column(&left_datums, left.1);
    let right_column = column(&right_datums, right.1);
    let mut rows_words = selected.to_vec();
    let rows = mask(nrows, &mut rows_words);
    let mut values = vec![0x5eed_i32; nrows];
    let mut non_null_words = vec![0_u64; nrows.div_ceil(64)];
    let mut non_nulls = mask(nrows, &mut non_null_words);
    let mut status = Status::new();
    // SAFETY: local columns, masks, buffers and status, alive for the call.
    let code = unsafe {
        match shape {
            Shape::Right(scalar) => tess_int2_arith_scalar(
                op,
                &left_column,
                i32::from(scalar),
                ptr::null(),
                &rows,
                values.as_mut_ptr(),
                &mut non_nulls,
                &mut status,
            ),
            Shape::Left(scalar) => tess_int2_arith_scalar_left(
                op,
                i32::from(scalar),
                &right_column,
                ptr::null(),
                &rows,
                values.as_mut_ptr(),
                &mut non_nulls,
                &mut status,
            ),
            Shape::Columns => tess_int2_arith_columns(
                op,
                &left_column,
                ptr::null(),
                &right_column,
                ptr::null(),
                &rows,
                values.as_mut_ptr(),
                &mut non_nulls,
                &mut status,
            ),
        }
    };
    (code, status.message().to_owned(), values, non_null_words)
}

/// Every operation and shape over 70 rows, a full word and a tail, with
/// NULLs on either side and unselected rows that would fail if read: the
/// values of the core, or the first failing row's error and message.
#[test]
fn every_operation_and_shape_follows_the_core() {
    let nrows = 70;
    let edges = [-32768_i16, -32767, -2, -1, 0, 1, 2, 181, 32767];
    let left: Vec<i16> = (0..nrows).map(|row| edges[row % edges.len()]).collect();
    let right: Vec<i16> = (0..nrows)
        .map(|row| edges[(row * 5 + 3) % edges.len()])
        .collect();
    let left_nulls: Vec<bool> = (0..nrows).map(|row| row % 7 == 3).collect();
    let right_nulls: Vec<bool> = (0..nrows).map(|row| row % 11 == 4).collect();
    let selected = [!(1_u64 << 9), 0b0011_1101];
    for op in OPS {
        for shape in [
            Shape::Right(-1),
            Shape::Right(3),
            Shape::Right(0),
            Shape::Left(-32768),
            Shape::Left(7),
            Shape::Columns,
        ] {
            let expected: Vec<Option<Result<i32, Code>>> = (0..nrows)
                .map(|row| {
                    let chosen = selected[row / 64] >> (row % 64) & 1 == 1;
                    let (a, b, present) = match shape {
                        Shape::Right(scalar) => (left[row], scalar, !left_nulls[row]),
                        Shape::Left(scalar) => (scalar, right[row], !right_nulls[row]),
                        Shape::Columns => {
                            (left[row], right[row], !left_nulls[row] && !right_nulls[row])
                        }
                    };
                    (chosen && present).then(|| model(op, a, b))
                })
                .collect();
            let (code, message, values, words) = call(
                op,
                shape,
                (&left, &left_nulls),
                (&right, &right_nulls),
                &selected,
            );
            let what = format!("op {op} {shape:?}");
            match expected.iter().flatten().find_map(|result| result.err()) {
                Some(first) => {
                    assert_eq!(code, first, "{what}: {message}");
                    let text = match first {
                        Code::DivisionByZero => "division by zero",
                        _ => "smallint out of range",
                    };
                    assert_eq!(message, text, "{what}");
                }
                None => {
                    assert_eq!(code, Code::Ok, "{what}: {message}");
                    for (row, expected) in expected.iter().enumerate() {
                        let present = words[row / 64] >> (row % 64) & 1 == 1;
                        assert_eq!(present, expected.is_some(), "{what} row {row}");
                        if let Some(Ok(value)) = expected {
                            assert_eq!(values[row], *value, "{what} row {row}");
                        }
                    }
                }
            }
        }
    }
}

/// The batch the C version answered with the wrong error: row 2
/// `-32768 / -1`, row 5 a division by zero, and the reverse order.
#[test]
fn the_first_failing_row_gives_its_error() {
    let mut dividends = vec![1_i16; 64];
    dividends[2] = -32768;
    dividends[5] = -32768;
    let nulls = vec![false; 64];
    let all = [u64::MAX];
    for (at_two, at_five, first) in [
        (-1, 0, Code::IntegerOutOfRange),
        (0, -1, Code::DivisionByZero),
    ] {
        let mut divisors = vec![1_i16; 64];
        divisors[2] = at_two;
        divisors[5] = at_five;
        let (code, message, _, _) = call(
            3,
            Shape::Columns,
            (&dividends, &nulls),
            (&divisors, &nulls),
            &all,
        );
        assert_eq!(code, first, "{message}");
        let (code, message, _, _) = call(
            3,
            Shape::Left(-32768),
            (&dividends, &nulls),
            (&divisors, &nulls),
            &all,
        );
        assert_eq!(code, first, "{message}");
    }
}

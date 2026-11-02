//! The smallint entry points called with raw pointers, as C calls them:
//! every operation and shape over Datum columns of smallints, the values
//! and the masks of successful calls, with NULLs, unselected rows and a
//! batch only partly prepared whose other slots stay uninitialized; a
//! result past the range and the error of the first row that fails, its
//! code, message and SQLSTATE; and the cast `x + 0` over any int4. Miri
//! runs it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use std::mem::MaybeUninit;
use std::ptr;

use tessera_capi::c::{
    Code, DatumColumn, Mask, Status, tess_int2_arith_columns, tess_int2_arith_scalar,
    tess_int2_arith_scalar_left,
};

/// `TessArithOp`: + - * / %.
const OPS: [u32; 5] = [0, 1, 2, 3, 4];

/// PostgreSQL's smallint operators: the value, or the code and SQLSTATE of
/// the error.
fn model(op: u32, a: i32, b: i32) -> Result<i32, (Code, &'static str)> {
    let range = (Code::IntegerOutOfRange, "22003");
    let (Ok(a), Ok(b)) = (i16::try_from(a), i16::try_from(b)) else {
        // Only `x + 0`, the cast, takes an int4 operand.
        assert_eq!((op, b), (0, 0));
        return i16::try_from(a).map(i32::from).map_err(|_| range);
    };
    let result = match op {
        0 => a.checked_add(b),
        1 => a.checked_sub(b),
        2 => a.checked_mul(b),
        3 | 4 if b == 0 => return Err((Code::DivisionByZero, "22012")),
        3 => a.checked_div(b),
        _ => Some(a.wrapping_rem(b)),
    };
    result.map(i32::from).ok_or(range)
}

/// A column's Datums and NULL flags; a row `None` in `values` is a slot
/// left uninitialized, which a call must not read.
struct Datums {
    values: Vec<MaybeUninit<u64>>,
    isnull: Vec<MaybeUninit<bool>>,
}

impl Datums {
    fn new(values: &[Option<i32>], nulls: &[bool]) -> Self {
        Self {
            values: values
                .iter()
                .map(|v| {
                    v.map_or(MaybeUninit::uninit(), |v| {
                        MaybeUninit::new(i64::from(v) as u64)
                    })
                })
                .collect(),
            isnull: values
                .iter()
                .zip(nulls)
                .map(|(v, &n)| v.map_or(MaybeUninit::uninit(), |_| MaybeUninit::new(n)))
                .collect(),
        }
    }

    fn of(values: &[i32], nulls: &[bool]) -> Self {
        Self::new(&values.iter().map(|&v| Some(v)).collect::<Vec<_>>(), nulls)
    }

    fn column(&self) -> DatumColumn {
        DatumColumn {
            struct_size: size_of::<DatumColumn>(),
            values: self.values.as_ptr().cast(),
            isnull: self.isnull.as_ptr().cast(),
            nrows: self.values.len() as i32,
            ..DatumColumn::EMPTY
        }
    }
}

/// A call's shape: a scalar on the right, on the left, or two columns.
#[derive(Clone, Copy, Debug)]
enum Shape {
    Right(i32),
    Left(i32),
    Columns,
}

/// What a call gave: its code, message and SQLSTATE, and the value of
/// each row it marked non-NULL.
struct Outcome {
    code: Code,
    message: String,
    sqlstate: String,
    values: Vec<Option<i32>>,
}

/// Call one entry point over the rows `selected`, both columns prepared
/// where `prepared` says (all of them when `None`).
fn call(
    op: u32,
    shape: Shape,
    left: &Datums,
    right: &Datums,
    selected: &[u64],
    prepared: Option<&[u64]>,
) -> Outcome {
    let nrows = left.values.len();
    let left_column = left.column();
    let right_column = right.column();
    let mut rows_words = selected.to_vec();
    let rows = Mask {
        nrows: nrows as i32,
        bits: rows_words.as_mut_ptr(),
    };
    let mut prepared_words = prepared.map(<[u64]>::to_vec);
    let prepared_mask = prepared_words.as_mut().map(|words| Mask {
        nrows: nrows as i32,
        bits: words.as_mut_ptr(),
    });
    let ready = prepared_mask
        .as_ref()
        .map_or(ptr::null(), |mask| mask as *const Mask);
    let mut values = vec![MaybeUninit::<i32>::uninit(); nrows];
    let mut non_null_words = vec![0_u64; nrows.div_ceil(64)];
    let mut non_nulls = Mask {
        nrows: nrows as i32,
        bits: non_null_words.as_mut_ptr(),
    };
    let mut status = Status::new();
    let out = values.as_mut_ptr().cast::<i32>();
    // SAFETY: local columns, masks, buffers and status, alive for the
    // call; the uninitialized slots are of rows the selection leaves out.
    let code = unsafe {
        match shape {
            Shape::Right(scalar) => tess_int2_arith_scalar(
                op,
                &left_column,
                scalar,
                ready,
                &rows,
                out,
                &mut non_nulls,
                &mut status,
            ),
            Shape::Left(scalar) => tess_int2_arith_scalar_left(
                op,
                scalar,
                &right_column,
                ready,
                &rows,
                out,
                &mut non_nulls,
                &mut status,
            ),
            Shape::Columns => tess_int2_arith_columns(
                op,
                &left_column,
                ready,
                &right_column,
                ready,
                &rows,
                out,
                &mut non_nulls,
                &mut status,
            ),
        }
    };
    let written = (0..nrows)
        .map(|row| {
            let marked = code == Code::Ok && non_null_words[row / 64] >> (row % 64) & 1 == 1;
            // SAFETY: a row the call marks non-NULL was written.
            marked.then(|| unsafe { values[row].assume_init() })
        })
        .collect();
    Outcome {
        code,
        message: status.message().to_owned(),
        sqlstate: status.sqlstate().to_owned(),
        values: written,
    }
}

/// Every operation and shape over 70 rows, a full word and a tail, with
/// NULLs and unselected rows: successful calls write each selected
/// non-NULL row's value and mark exactly those rows.
#[test]
fn successful_calls_write_the_values_and_the_masks() {
    let nrows = 70;
    let left: Vec<i32> = (0..nrows as i32).map(|row| row * 37 % 401 - 200).collect();
    let right: Vec<i32> = (0..nrows as i32)
        .map(|row| (row * 53 % 199 - 99) | 1)
        .collect();
    let left_nulls: Vec<bool> = (0..nrows).map(|row| row % 7 == 3).collect();
    let right_nulls: Vec<bool> = (0..nrows).map(|row| row % 11 == 4).collect();
    let (left_datums, right_datums) = (
        Datums::of(&left, &left_nulls),
        Datums::of(&right, &right_nulls),
    );
    let selected = [!(1_u64 << 9), 0b0011_1101];
    let shapes = [
        Shape::Right(3),
        Shape::Right(-7),
        Shape::Left(7),
        Shape::Left(-150),
        Shape::Columns,
    ];
    for op in OPS {
        for shape in shapes {
            let outcome = call(op, shape, &left_datums, &right_datums, &selected, None);
            let what = format!("op {op} {shape:?}");
            assert_eq!(outcome.code, Code::Ok, "{what}: {}", outcome.message);
            for row in 0..nrows {
                let chosen = selected[row / 64] >> (row % 64) & 1 == 1;
                let operands = match shape {
                    Shape::Right(s) => (!left_nulls[row]).then_some((left[row], s)),
                    Shape::Left(s) => (!right_nulls[row]).then_some((s, right[row])),
                    Shape::Columns => {
                        (!left_nulls[row] && !right_nulls[row]).then_some((left[row], right[row]))
                    }
                };
                let want = operands
                    .filter(|_| chosen)
                    .map(|(a, b)| model(op, a, b).unwrap());
                assert_eq!(outcome.values[row], want, "{what} row {row}");
            }
        }
    }
}

/// The same over a batch only partly prepared: every third row is not, its
/// Datum and its flag never written, and the selection keeps to the
/// prepared rows.
#[test]
fn a_partly_prepared_batch_reads_only_its_prepared_rows() {
    let nrows = 70;
    let ready = |row: usize| row % 3 != 2;
    let left: Vec<Option<i32>> = (0..nrows)
        .map(|row| ready(row).then_some(row as i32 - 30))
        .collect();
    let right: Vec<Option<i32>> = (0..nrows)
        .map(|row| ready(row).then_some(row as i32 % 9 + 1))
        .collect();
    let nulls = vec![false; nrows];
    let (left_datums, right_datums) = (Datums::new(&left, &nulls), Datums::new(&right, &nulls));
    let mut prepared = [0_u64; 2];
    for row in (0..nrows).filter(|&row| ready(row)) {
        prepared[row / 64] |= 1 << (row % 64);
    }
    for op in OPS {
        for shape in [Shape::Right(3), Shape::Left(5), Shape::Columns] {
            let outcome = call(
                op,
                shape,
                &left_datums,
                &right_datums,
                &prepared,
                Some(&prepared),
            );
            assert_eq!(
                outcome.code,
                Code::Ok,
                "op {op} {shape:?}: {}",
                outcome.message
            );
            for row in 0..nrows {
                let want = ready(row).then(|| {
                    let (a, b) = match shape {
                        Shape::Right(s) => (left[row].unwrap(), s),
                        Shape::Left(s) => (s, right[row].unwrap()),
                        Shape::Columns => (left[row].unwrap(), right[row].unwrap()),
                    };
                    model(op, a, b).unwrap()
                });
                assert_eq!(outcome.values[row], want, "op {op} {shape:?} row {row}");
            }
        }
    }
}

/// Every operation and shape over 70 rows at the edges: the first failing
/// row's code, message and SQLSTATE; and the batch of a range error before
/// a division by zero, and the reverse.
#[test]
fn the_first_failing_row_gives_its_error() {
    let nrows = 70;
    let edges = [-32768, -32767, -2, -1, 0, 1, 2, 181, 32767];
    let left: Vec<i32> = (0..nrows).map(|row| edges[row % edges.len()]).collect();
    let right: Vec<i32> = (0..nrows)
        .map(|row| edges[(row * 5 + 3) % edges.len()])
        .collect();
    let nulls = vec![false; nrows];
    let (left_datums, right_datums) = (Datums::of(&left, &nulls), Datums::of(&right, &nulls));
    let selected = [u64::MAX, 0b11_1111];
    for op in OPS {
        for shape in [
            Shape::Right(-1),
            Shape::Right(0),
            Shape::Left(-32768),
            Shape::Columns,
        ] {
            let first = (0..nrows).find_map(|row| {
                let (a, b) = match shape {
                    Shape::Right(s) => (left[row], s),
                    Shape::Left(s) => (s, right[row]),
                    Shape::Columns => (left[row], right[row]),
                };
                model(op, a, b).err()
            });
            let outcome = call(op, shape, &left_datums, &right_datums, &selected, None);
            let what = format!("op {op} {shape:?}");
            match first {
                Some((code, sqlstate)) => {
                    assert_eq!(outcome.code, code, "{what}: {}", outcome.message);
                    assert_eq!(outcome.sqlstate, sqlstate, "{what}");
                    let text = if code == Code::DivisionByZero {
                        "division by zero"
                    } else {
                        "smallint out of range"
                    };
                    assert_eq!(outcome.message, text, "{what}");
                }
                None => assert_eq!(outcome.code, Code::Ok, "{what}: {}", outcome.message),
            }
        }
    }
    let mut dividends = vec![1; 64];
    dividends[2] = -32768;
    dividends[5] = -32768;
    let dividends = Datums::of(&dividends, &[false; 64]);
    for (at_two, at_five, sqlstate) in [(-1, 0, "22003"), (0, -1, "22012")] {
        let mut divisors = vec![1; 64];
        divisors[2] = at_two;
        divisors[5] = at_five;
        let divisors = Datums::of(&divisors, &[false; 64]);
        for shape in [Shape::Columns, Shape::Left(-32768)] {
            let outcome = call(3, shape, &dividends, &divisors, &[u64::MAX], None);
            assert_eq!(outcome.sqlstate, sqlstate, "{shape:?}: {}", outcome.message);
        }
    }
}

/// The cast `x + 0` over any int4 through the entry point: past either end
/// of the range and at the int4 extremes it fails with 22003; a bad value
/// in an unselected or NULL row does not.
#[test]
fn the_cast_through_the_entry_point() {
    let all = [u64::MAX, 0b11_1111];
    let unselected = [u64::MAX, 0b11_1011];
    for value in [32767, -32768, 32768, -32769, i32::MAX, i32::MIN] {
        let mut values = vec![9; 70];
        values[66] = value;
        let column = Datums::of(&values, &[false; 70]);
        let outcome = call(0, Shape::Right(0), &column, &column, &all, None);
        if i16::try_from(value).is_ok() {
            assert_eq!(outcome.code, Code::Ok, "{value}: {}", outcome.message);
            assert_eq!(outcome.values[66], Some(value));
        } else {
            assert_eq!(
                (outcome.code, outcome.sqlstate.as_str()),
                (Code::IntegerOutOfRange, "22003"),
                "{value}"
            );
        }
        let outcome = call(0, Shape::Right(0), &column, &column, &unselected, None);
        assert_eq!(outcome.code, Code::Ok, "{value} unselected");
        let mut nulls = vec![false; 70];
        nulls[66] = true;
        let column = Datums::of(&values, &nulls);
        let outcome = call(0, Shape::Right(0), &column, &column, &all, None);
        assert_eq!(outcome.code, Code::Ok, "{value} NULL");
        assert_eq!(outcome.values[66], None);
    }
}

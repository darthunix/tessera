use anyhow::Result;
use proptest::prelude::*;
use proptest::sample::select;
use tessera_testing::{flags, nrows, property, words};

use super::*;

const COMPARES: [CompareOp; 6] = [
    CompareOp::Eq,
    CompareOp::Ne,
    CompareOp::Lt,
    CompareOp::Le,
    CompareOp::Gt,
    CompareOp::Ge,
];

const OPS: [FloatOp; 6] = [
    FloatOp::Add,
    FloatOp::Sub,
    FloatOp::Mul,
    FloatOp::Div,
    FloatOp::Neg,
    FloatOp::Abs,
];

/// The values where the rules turn: NaN, infinities, zeros of both signs,
/// the extremes and the least normal and subnormal values of both widths,
/// ties of rounding, and the edges of each integer range.
const F64_EDGES: &[f64] = &[
    f64::NAN,
    f64::INFINITY,
    f64::NEG_INFINITY,
    0.0,
    -0.0,
    1.0,
    -1.0,
    0.5,
    -0.5,
    1.5,
    2.5,
    -2.5,
    3.0,
    f64::MAX,
    -f64::MAX,
    f64::MIN_POSITIVE,
    -f64::MIN_POSITIVE,
    5e-324,
    -5e-324,
    3.4028234663852886e38,
    3.4028235677973366e38,
    1.1754943508222875e-38,
    1.401298464324817e-45,
    7.006492321624085e-46,
    7.1e-46,
    32767.5,
    32768.0,
    -32768.5,
    -32769.0,
    2147483646.5,
    2147483647.5,
    2147483648.0,
    -2147483648.5,
    -2147483649.0,
    9223372036854775807.0,
    -9223372036854775808.0,
    -9223372036854777856.0,
];

const F32_EDGES: &[f32] = &[
    f32::NAN,
    f32::INFINITY,
    f32::NEG_INFINITY,
    0.0,
    -0.0,
    1.0,
    -1.0,
    0.5,
    1.5,
    2.5,
    -2.5,
    f32::MAX,
    -f32::MAX,
    f32::MIN_POSITIVE,
    1.0e-45,
    -1.0e-45,
    1.0e-30,
    1.0e30,
    32767.5,
    -32768.5,
    32768.0,
    2147483520.0,
    2147483648.0,
    -2147483648.0,
    9.223372e18,
    -9.223372e18,
];

fn f64_value() -> BoxedStrategy<f64> {
    prop_oneof![
        select(F64_EDGES),
        any::<u64>().prop_map(f64::from_bits),
        (-1e6..1e6_f64),
        (-1000_i32..1000).prop_map(|x| f64::from(x) / 2.0),
    ]
    .boxed()
}

fn f32_value() -> BoxedStrategy<f32> {
    prop_oneof![
        select(F32_EDGES),
        any::<u32>().prop_map(f32::from_bits),
        (-1e6..1e6_f32),
        (-1000_i16..1000).prop_map(|x| f32::from(x) / 2.0),
    ]
    .boxed()
}

/// The core's order as a key: NaN after every number, -0 equal to 0.
fn order(x: f64) -> (u8, f64) {
    if x.is_nan() { (1, 0.0) } else { (0, x) }
}

fn compared(op: CompareOp, left: f64, right: f64) -> bool {
    let (left, right) = (order(left), order(right));
    match op {
        CompareOp::Eq => left == right,
        CompareOp::Ne => left != right,
        CompareOp::Lt => left < right,
        CompareOp::Le => left <= right,
        CompareOp::Gt => left > right,
        CompareOp::Ge => left >= right,
    }
}

#[test]
fn comparisons_follow_the_order_of_the_core() {
    let widened: Vec<f64> = F32_EDGES.iter().map(|&x| f64::from(x)).collect();
    for &left in F64_EDGES.iter().chain(&widened) {
        for &right in F64_EDGES.iter().chain(&widened) {
            for op in COMPARES {
                assert_eq!(
                    compare(op, left, right),
                    compared(op, left, right),
                    "{left} {op:?} {right}"
                );
            }
        }
    }
    property((f64_value(), f64_value()), |(left, right)| -> Result<()> {
        for op in COMPARES {
            assert_eq!(
                compare(op, left, right),
                compared(op, left, right),
                "{left} {op:?} {right}"
            );
        }
        Ok(())
    });
}

/// float4 arithmetic computed in float8, then narrowed: the result of
/// `+ - * /` rounded twice is the one rounded once, since float8 has more
/// than twice float4's digits; an error from the exact float8 result.
fn float4_reference(op: FloatOp, left: f32, right: f32) -> Result<f32, FloatError> {
    let (wide_left, wide_right) = (f64::from(left), f64::from(right));
    if op == FloatOp::Div && right == 0.0 && !left.is_nan() {
        return Err(FloatError::DivisionByZero);
    }
    let wide = match op {
        FloatOp::Add => wide_left + wide_right,
        FloatOp::Sub => wide_left - wide_right,
        FloatOp::Mul => wide_left * wide_right,
        FloatOp::Div => wide_left / wide_right,
        FloatOp::Neg => -wide_left,
        FloatOp::Abs => wide_left.abs(),
    };
    let narrow = wide as f32;
    let operands_finite = !left.is_infinite() && (op.is_unary() || !right.is_infinite());
    if narrow.is_infinite() && operands_finite {
        return Err(FloatError::Overflow);
    }
    if narrow == 0.0 && wide != 0.0 {
        return Err(FloatError::Underflow);
    }
    Ok(narrow)
}

fn same_float4(left: Result<f32, FloatError>, right: Result<f32, FloatError>) -> bool {
    match (left, right) {
        (Ok(left), Ok(right)) => {
            (left.is_nan() && right.is_nan()) || left.to_bits() == right.to_bits()
        }
        (left, right) => left == right,
    }
}

#[test]
fn float4_arithmetic_is_float8_arithmetic_narrowed() {
    for &left in F32_EDGES {
        for &right in F32_EDGES {
            for op in OPS {
                let (got, want) = (arith(op, left, right), float4_reference(op, left, right));
                assert!(
                    same_float4(got, want),
                    "{left} {op:?} {right}: {got:?} {want:?}"
                );
            }
        }
    }
    property((f32_value(), f32_value()), |(left, right)| -> Result<()> {
        for op in OPS {
            let (got, want) = (arith(op, left, right), float4_reference(op, left, right));
            assert!(
                same_float4(got, want),
                "{left} {op:?} {right}: {got:?} {want:?}"
            );
        }
        Ok(())
    });
}

#[test]
fn float8_arithmetic_raises_where_the_core_raises() {
    use FloatError::{DivisionByZero, Overflow, Underflow};
    let tiny = 5e-324;
    let cases: &[(FloatOp, f64, f64, Result<f64, FloatError>)] = &[
        (FloatOp::Add, f64::MAX, f64::MAX, Err(Overflow)),
        (FloatOp::Add, f64::INFINITY, 1.0, Ok(f64::INFINITY)),
        (FloatOp::Add, f64::INFINITY, f64::NEG_INFINITY, Ok(f64::NAN)),
        (FloatOp::Add, tiny, -tiny, Ok(0.0)),
        (FloatOp::Sub, -f64::MAX, f64::MAX, Err(Overflow)),
        (FloatOp::Sub, f64::NEG_INFINITY, 1.0, Ok(f64::NEG_INFINITY)),
        (FloatOp::Mul, f64::MAX, 2.0, Err(Overflow)),
        (FloatOp::Mul, f64::INFINITY, 0.0, Ok(f64::NAN)),
        (FloatOp::Mul, f64::INFINITY, 2.0, Ok(f64::INFINITY)),
        (
            FloatOp::Mul,
            f64::MIN_POSITIVE,
            f64::MIN_POSITIVE,
            Err(Underflow),
        ),
        (FloatOp::Mul, tiny, 0.5, Err(Underflow)),
        (FloatOp::Mul, 0.0, f64::MAX, Ok(0.0)),
        (FloatOp::Mul, -0.0, 3.0, Ok(-0.0)),
        (FloatOp::Div, 1.0, 0.0, Err(DivisionByZero)),
        (FloatOp::Div, 0.0, 0.0, Err(DivisionByZero)),
        (FloatOp::Div, f64::INFINITY, -0.0, Err(DivisionByZero)),
        (FloatOp::Div, f64::NAN, 0.0, Ok(f64::NAN)),
        (FloatOp::Div, f64::MAX, 0.5, Err(Overflow)),
        (FloatOp::Div, f64::INFINITY, 2.0, Ok(f64::INFINITY)),
        (FloatOp::Div, 1.0, f64::INFINITY, Ok(0.0)),
        (FloatOp::Div, tiny, 2.0, Err(Underflow)),
        (FloatOp::Div, 0.0, 5.0, Ok(0.0)),
        (FloatOp::Neg, 0.0, 0.0, Ok(-0.0)),
        (FloatOp::Neg, f64::INFINITY, 0.0, Ok(f64::NEG_INFINITY)),
        (FloatOp::Abs, -0.0, 0.0, Ok(0.0)),
        (FloatOp::Abs, f64::NEG_INFINITY, 0.0, Ok(f64::INFINITY)),
        (FloatOp::Abs, -f64::NAN, 0.0, Ok(f64::NAN)),
    ];
    for &(op, left, right, want) in cases {
        let got = arith(op, left, right);
        let same = match (got, want) {
            (Ok(got), Ok(want)) => {
                (got.is_nan() && want.is_nan()) || got.to_bits() == want.to_bits()
            }
            (got, want) => got == want,
        };
        assert!(same, "{left} {op:?} {right}: {got:?}, not {want:?}");
    }
    assert!(
        !arith(FloatOp::Abs, -f64::NAN, 0.0)
            .unwrap()
            .is_sign_negative()
    );
}

/// `rint`'s rounding by another derivation: the floor and the fraction
/// left, a tie to the even neighbor.
fn rint_reference(x: f64) -> f64 {
    let floor = x.floor();
    let fraction = x - floor;
    if fraction < 0.5 {
        floor
    } else if fraction > 0.5 {
        floor + 1.0
    } else if (floor / 2.0).floor() * 2.0 == floor {
        floor
    } else {
        floor + 1.0
    }
}

/// The cast as the core gives it: the rounded value, if it is an integer
/// of `to`'s range.
fn integer_reference(x: f64, to: Integer) -> Result<i64, FloatError> {
    let rounded = rint_reference(x);
    let (least, most, error) = match to {
        Integer::Int2 => (
            i128::from(i16::MIN),
            i128::from(i16::MAX),
            FloatError::SmallintOutOfRange,
        ),
        Integer::Int4 => (
            i128::from(i32::MIN),
            i128::from(i32::MAX),
            FloatError::IntegerOutOfRange,
        ),
        Integer::Int8 => (
            i128::from(i64::MIN),
            i128::from(i64::MAX),
            FloatError::BigintOutOfRange,
        ),
    };
    if !rounded.is_finite() {
        return Err(error);
    }
    let value = rounded as i128;
    if (least..=most).contains(&value) {
        Ok(value as i64)
    } else {
        Err(error)
    }
}

#[test]
fn casts_to_integers_round_to_even_and_check_the_range() {
    let check = |x: f64, single: Option<f32>| {
        for to in [Integer::Int2, Integer::Int4, Integer::Int8] {
            assert_eq!(x.integer(to), integer_reference(x, to), "{x} {to:?}");
            if let Some(single) = single {
                assert_eq!(
                    single.integer(to),
                    integer_reference(x, to),
                    "{single} {to:?}"
                );
            }
        }
    };
    for &x in F64_EDGES {
        check(x, None);
        for step in [-1, 1] {
            check(next(x, step), None);
        }
    }
    for &x in F32_EDGES {
        check(f64::from(x), Some(x));
    }
    property(f64_value(), |x| -> Result<()> {
        check(x, None);
        Ok(())
    });
    property(f32_value(), |x| -> Result<()> {
        check(f64::from(x), Some(x));
        Ok(())
    });
    assert_eq!(2.5_f64.integer(Integer::Int4), Ok(2));
    assert_eq!((-2.5_f64).integer(Integer::Int4), Ok(-2));
    assert_eq!(
        32767.5_f32.integer(Integer::Int2),
        Err(FloatError::SmallintOutOfRange)
    );
    assert_eq!((-32768.5_f32).integer(Integer::Int2), Ok(-32768));
    assert_eq!(
        2147483647.5_f64.integer(Integer::Int4),
        Err(FloatError::IntegerOutOfRange)
    );
    assert_eq!(
        9.223372e18_f32.integer(Integer::Int8),
        Err(FloatError::BigintOutOfRange)
    );
    assert_eq!((-9.223372e18_f32).integer(Integer::Int8), Ok(i64::MIN));
}

/// The neighbor of `x` one step of its bits away.
fn next(x: f64, step: i64) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    f64::from_bits(x.to_bits().wrapping_add_signed(step))
}

#[test]
fn casts_to_floats_round_to_nearest_and_check_float4() {
    for &(x, want) in &[
        (f64::MAX, Err(FloatError::Overflow)),
        (-3.5e38, Err(FloatError::Overflow)),
        (1e-300, Err(FloatError::Underflow)),
        (7.006492321624085e-46, Err(FloatError::Underflow)),
        (7.1e-46, Ok(f32::from_bits(1))),
        (1.401298464324817e-45, Ok(f32::from_bits(1))),
        // The tie between float4's greatest value and 2^128 rounds to
        // infinity; the float8 just below it rounds to the greatest value.
        (3.4028235677973366e38, Err(FloatError::Overflow)),
        (3.4028235677973362e38, Ok(f32::MAX)),
        (f64::INFINITY, Ok(f32::INFINITY)),
        (-0.0, Ok(-0.0)),
        (0.1, Ok(0.1_f32)),
    ] {
        let got = x.float4();
        assert!(same_float4(got, want), "{x}: {got:?}, not {want:?}");
    }
    assert!(f64::NAN.float4().unwrap().is_nan());
    property(f64_value(), |x| -> Result<()> {
        match x.float4() {
            Ok(narrow) => {
                assert!(narrow.is_nan() == x.is_nan(), "{x}");
                assert!(narrow != 0.0 || x == 0.0, "{x}");
                assert!(!narrow.is_infinite() || x.is_infinite(), "{x}");
            }
            Err(FloatError::Overflow) => {
                assert!(x.is_finite() && x.abs() > f64::from(f32::MAX), "{x}")
            }
            Err(FloatError::Underflow) => {
                assert!(x != 0.0 && x.abs() < f64::from(f32::from_bits(1)), "{x}")
            }
            Err(error) => panic!("{x}: {error}"),
        }
        Ok(())
    });
    assert_eq!(i64::MAX.float8(), 9223372036854775808.0);
    assert_eq!(i64::MAX.float4(), Ok(9.223372e18_f32));
    assert_eq!(16_777_217_i32.float4(), Ok(16_777_216.0));
    assert_eq!(i32::MIN.float8(), -2147483648.0);
    assert_eq!(1.5_f32.float8(), 1.5);
    assert_eq!((-2.5_f64).float8().to_bits(), (-2.5_f64).to_bits());
    assert_eq!(f32::MAX.float4(), Ok(f32::MAX));
}

#[test]
fn errors_carry_the_core_sqlstate_and_message() {
    for (error, sqlstate, message) in [
        (
            FloatError::Overflow,
            "22003",
            "value out of range: overflow",
        ),
        (
            FloatError::Underflow,
            "22003",
            "value out of range: underflow",
        ),
        (FloatError::DivisionByZero, "22012", "division by zero"),
        (
            FloatError::IntegerOutOfRange,
            "22003",
            "integer out of range",
        ),
        (
            FloatError::SmallintOutOfRange,
            "22003",
            "smallint out of range",
        ),
        (FloatError::BigintOutOfRange, "22003", "bigint out of range"),
    ] {
        assert_eq!(error.sqlstate(), sqlstate);
        assert_eq!(error.to_string(), message);
        let wrapped = anyhow::Error::new(error).context("a call");
        let classified = crate::error::classify(&wrapped).map(|e| (e.sqlstate(), e.kind()));
        assert_eq!(classified, Some((sqlstate, crate::error::ErrorKind::Data)));
    }
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

/// The value a kernel wrote into a slot that was created initialized.
#[allow(unsafe_code)]
fn written<T: Copy>(slot: &MaybeUninit<T>) -> T {
    // SAFETY: every slot of these tests is created initialized, and a
    // kernel overwrites a slot only with an initialized value.
    unsafe { slot.assume_init() }
}

/// A column of a test: a value and a NULL flag a row.
#[derive(Debug)]
struct Column<T> {
    values: Vec<T>,
    nulls: Vec<bool>,
}

impl<T: Copy> Source<T> for Column<T> {
    fn get(&self, row: usize) -> Option<T> {
        if self.nulls[row] {
            None
        } else {
            Some(self.values[row])
        }
    }
}

/// A scalar of a test.
struct Scalar<T>(T);

impl<T: Copy> Source<T> for Scalar<T> {
    fn get(&self, _row: usize) -> Option<T> {
        Some(self.0)
    }
}

/// A batch: its selection, two columns of their own NULLs, and an operation.
fn batch() -> impl Strategy<Value = (Vec<bool>, Column<f32>, Column<f64>, usize)> {
    nrows().prop_flat_map(|nrows| {
        (
            flags(nrows),
            proptest::collection::vec(f32_value(), nrows),
            proptest::collection::vec(proptest::bool::weighted(0.2), nrows),
            proptest::collection::vec(f64_value(), nrows),
            proptest::collection::vec(proptest::bool::weighted(0.2), nrows),
            0..6_usize,
        )
            .prop_map(|(selected, l, l_nulls, r, r_nulls, op)| {
                (
                    selected,
                    Column {
                        values: l,
                        nulls: l_nulls,
                    },
                    Column {
                        values: r,
                        nulls: r_nulls,
                    },
                    op,
                )
            })
    })
}

#[test]
fn comparisons_keep_the_selected_rows_that_hold() {
    property(batch(), |(selected, left, right, op)| -> Result<()> {
        let op = COMPARES[op];
        let mut kept = words(&selected);
        compare_rows(
            op,
            &left,
            &right,
            &mut RowMask::try_new(selected.len(), &mut kept)?,
        )?;
        let mut scalar = words(&selected);
        compare_rows(
            op,
            &left,
            &Scalar(2.5_f64),
            &mut RowMask::try_new(selected.len(), &mut scalar)?,
        )?;
        for row in 0..selected.len() {
            let (l, r) = (left.get(row), right.get(row));
            let want =
                selected[row] && matches!((l, r), (Some(l), Some(r)) if compared(op, l.into(), r));
            assert_eq!(kept[row / 64] >> (row % 64) & 1 == 1, want, "row {row}");
            let want = selected[row] && l.is_some_and(|l| compared(op, l.into(), 2.5));
            assert_eq!(scalar[row / 64] >> (row % 64) & 1 == 1, want, "row {row}");
        }
        Ok(())
    });
}

#[test]
fn arithmetic_writes_the_rows_or_fails_at_the_first_error() {
    property(batch(), |(selected, left, right, op)| -> Result<()> {
        let op = OPS[op % 4];
        let nrows = selected.len();
        let rows = words(&selected);
        let mut values = vec![MaybeUninit::new(u64::MAX); nrows];
        let mut non_nulls = vec![0; nrows.div_ceil(64)];
        let outcome = binary_float8(
            op,
            &left,
            &right,
            RowMaskView::try_new(nrows, &rows)?,
            &mut values,
            &mut RowMask::try_new(nrows, &mut non_nulls)?,
        );
        let first_error = (0..nrows)
            .filter(|&row| selected[row])
            .filter_map(|row| Some((left.get(row)?, right.get(row)?)))
            .find_map(|(l, r)| arith(op, f64::from(l), r).err());
        match (outcome, first_error) {
            (Ok(()), None) => {
                for row in 0..nrows {
                    let present = non_nulls[row / 64] >> (row % 64) & 1 == 1;
                    let pair = left.get(row).zip(right.get(row));
                    assert_eq!(present, selected[row] && pair.is_some(), "row {row}");
                    if let (true, Some((l, r))) = (present, pair) {
                        let got = f64::from_bits(written(&values[row]));
                        let want = arith(op, f64::from(l), r).unwrap();
                        assert!(got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()));
                    }
                }
            }
            (Err(error), Some(want)) => assert_eq!(error.downcast_ref::<FloatError>(), Some(&want)),
            (outcome, want) => panic!("{outcome:?}, not {want:?}"),
        }
        Ok(())
    });
}

#[test]
fn a_later_row_fails_when_the_earlier_one_is_not_read() {
    // Squared, row 70 underflows and row 100 overflows.
    let nrows = 130;
    let mut values = vec![1.0_f64; nrows];
    values[70] = 1e-200;
    values[100] = 1e200;
    let column = |nulls: Vec<bool>| Column {
        values: values.clone(),
        nulls,
    };
    let run = |selected: &[bool], nulls: Vec<bool>| -> Result<()> {
        let rows = words(selected);
        let mut out = vec![MaybeUninit::new(0); nrows];
        let mut non_nulls = vec![0; 3];
        binary_float8(
            FloatOp::Mul,
            &column(nulls),
            &column(vec![false; nrows]),
            RowMaskView::try_new(nrows, &rows)?,
            &mut out,
            &mut RowMask::try_new(nrows, &mut non_nulls)?,
        )
    };
    let all = vec![true; nrows];
    let no_nulls = vec![false; nrows];
    let error = |outcome: Result<()>| outcome.unwrap_err().downcast::<FloatError>().unwrap();
    assert_eq!(error(run(&all, no_nulls.clone())), FloatError::Underflow);
    let mut skip = all.clone();
    skip[70] = false;
    assert_eq!(error(run(&skip, no_nulls.clone())), FloatError::Overflow);
    let mut null = no_nulls;
    null[70] = true;
    assert_eq!(error(run(&all, null)), FloatError::Overflow);
}

#[test]
fn unary_operations_and_casts_over_rows() {
    let nrows = 3;
    let rows = [0b111];
    let column = Column {
        values: vec![-0.0_f32, 1.5, 32767.5],
        nulls: vec![false, true, false],
    };
    let mut values = vec![MaybeUninit::new(0); nrows];
    let mut non_nulls = [0];
    let view = RowMaskView::try_new(nrows, &rows).unwrap();
    unary_float4(
        FloatOp::Neg,
        &column,
        view,
        &mut values,
        &mut RowMask::try_new(nrows, &mut non_nulls).unwrap(),
    )
    .unwrap();
    assert_eq!(non_nulls, [0b101]);
    assert_eq!(
        (written(&values[0]), written(&values[2])),
        (datum4(0.0), datum4(-32767.5))
    );
    unary_float8(
        FloatOp::Abs,
        &Scalar(-2.0),
        view,
        &mut values,
        &mut RowMask::try_new(nrows, &mut non_nulls).unwrap(),
    )
    .unwrap();
    assert_eq!(written(&values[1]), 2.0_f64.to_bits());

    let mut ints = vec![MaybeUninit::new(0); nrows];
    let error = to_int32(
        Integer::Int2,
        &column,
        view,
        &mut ints,
        &mut RowMask::try_new(nrows, &mut non_nulls).unwrap(),
    );
    assert_eq!(
        error.unwrap_err().downcast::<FloatError>().unwrap(),
        FloatError::SmallintOutOfRange
    );
    to_int32(
        Integer::Int4,
        &column,
        view,
        &mut ints,
        &mut RowMask::try_new(nrows, &mut non_nulls).unwrap(),
    )
    .unwrap();
    assert_eq!((written(&ints[0]), written(&ints[2])), (0, 32768));
    let mut longs = vec![MaybeUninit::new(0); nrows];
    to_int64(
        &Scalar(-9.5_f64),
        view,
        &mut longs,
        &mut RowMask::try_new(nrows, &mut non_nulls).unwrap(),
    )
    .unwrap();
    assert_eq!(written(&longs[2]), -10);
    to_float4(
        &Scalar(i64::MAX),
        view,
        &mut values,
        &mut RowMask::try_new(nrows, &mut non_nulls).unwrap(),
    )
    .unwrap();
    assert_eq!(written(&values[0]), datum4(9.223372e18));
    let error = to_float4(
        &Scalar(1e300_f64),
        view,
        &mut values,
        &mut RowMask::try_new(nrows, &mut non_nulls).unwrap(),
    );
    assert_eq!(
        error.unwrap_err().downcast::<FloatError>().unwrap(),
        FloatError::Overflow
    );
    to_float8(
        &column,
        view,
        &mut values,
        &mut RowMask::try_new(nrows, &mut non_nulls).unwrap(),
    )
    .unwrap();
    assert_eq!(written(&values[2]), 32767.5_f64.to_bits());
    assert_eq!(non_nulls, [0b101]);
}

#[test]
fn a_call_with_the_wrong_number_of_arguments_is_refused() {
    let rows = [1];
    let view = RowMaskView::try_new(1, &rows).unwrap();
    let mut values = [MaybeUninit::new(0)];
    let mut non_nulls = [0];
    let mut mask = RowMask::try_new(1, &mut non_nulls).unwrap();
    for op in [FloatOp::Neg, FloatOp::Abs] {
        assert!(
            binary_float4(op, &Scalar(1.0), &Scalar(1.0), view, &mut values, &mut mask).is_err()
        );
        assert!(
            binary_float8(
                op,
                &Scalar(1.0_f64),
                &Scalar(1.0_f64),
                view,
                &mut values,
                &mut mask
            )
            .is_err()
        );
    }
    for op in [FloatOp::Add, FloatOp::Sub, FloatOp::Mul, FloatOp::Div] {
        assert!(unary_float4(op, &Scalar(1.0), view, &mut values, &mut mask).is_err());
        assert!(unary_float8(op, &Scalar(1.0), view, &mut values, &mut mask).is_err());
    }
    let mut ints = [MaybeUninit::new(0)];
    assert!(to_int32(Integer::Int8, &Scalar(1.0_f64), view, &mut ints, &mut mask).is_err());
}

#[test]
fn float4_operands_compute_in_their_own_width() {
    let rows = [1];
    let view = RowMaskView::try_new(1, &rows).unwrap();
    let mut values = [MaybeUninit::new(0)];
    let mut non_nulls = [0];
    for op in [FloatOp::Add, FloatOp::Sub, FloatOp::Mul, FloatOp::Div] {
        let (left, right) = (16_777_216.0_f32, 3.0_f32);
        binary_float4(
            op,
            &Scalar(left),
            &Scalar(right),
            view,
            &mut values,
            &mut RowMask::try_new(1, &mut non_nulls).unwrap(),
        )
        .unwrap();
        let got = written(&values[0]);
        assert_eq!(got, datum4(arith(op, left, right).unwrap()), "{op:?}");
    }
}

/// The value one call of a batch function wrote into its one row.
fn one_row(
    call: impl FnOnce(RowMaskView<'_>, &mut [MaybeUninit<u64>], &mut RowMask<'_>) -> Result<()>,
) -> Result<u64> {
    let rows = [1];
    let mut values = [MaybeUninit::new(u64::MAX)];
    let mut non_nulls = [0];
    call(
        RowMaskView::try_new(1, &rows)?,
        &mut values,
        &mut RowMask::try_new(1, &mut non_nulls)?,
    )?;
    assert_eq!(non_nulls, [1]);
    Ok(written(&values[0]))
}

fn float_error(outcome: Result<u64>) -> FloatError {
    outcome.unwrap_err().downcast::<FloatError>().unwrap()
}

#[test]
fn float4_rows_are_checked_in_float4() {
    let tiny = f32::from_bits(1);
    let binary = |op, left: f32, right: f32| {
        one_row(|rows, values, non_nulls| {
            binary_float4(op, &Scalar(left), &Scalar(right), rows, values, non_nulls)
        })
    };
    // Each would pass if checked in float8 and narrowed after.
    assert_eq!(
        float_error(binary(FloatOp::Mul, f32::MAX, 2.0)),
        FloatError::Overflow
    );
    assert_eq!(
        float_error(binary(FloatOp::Add, f32::MAX, f32::MAX)),
        FloatError::Overflow
    );
    assert_eq!(
        float_error(binary(FloatOp::Div, tiny, 2.0)),
        FloatError::Underflow
    );
    assert_eq!(
        float_error(binary(FloatOp::Mul, tiny, 0.5)),
        FloatError::Underflow
    );
    // A subnormal result that is not zero is no underflow.
    assert_eq!(binary(FloatOp::Mul, tiny, 2.0).unwrap(), 2);
    // A negative result's Datum has its high half set, as Float4GetDatum
    // gives it.
    assert_eq!(
        binary(FloatOp::Sub, 1.0, 2.0).unwrap(),
        0xffff_ffff_bf80_0000
    );
    assert_eq!(binary(FloatOp::Div, f32::from_bits(4), 2.0).unwrap(), 2);
}

#[test]
fn negation_and_abs_of_rows_keep_the_signs_of_zeros() {
    let float4 = |op, x: f32| {
        one_row(|rows, values, non_nulls| unary_float4(op, &Scalar(x), rows, values, non_nulls))
            .unwrap()
    };
    let float8 = |op, x: f64| {
        one_row(|rows, values, non_nulls| unary_float8(op, &Scalar(x), rows, values, non_nulls))
            .unwrap()
    };
    for (x, neg, abs) in [
        (0.0_f32, -0.0_f32, 0.0_f32),
        (-0.0, 0.0, 0.0),
        (2.0, -2.0, 2.0),
        (-2.5, 2.5, 2.5),
    ] {
        assert_eq!(float4(FloatOp::Neg, x), datum4(neg), "-{x}");
        assert_eq!(float4(FloatOp::Abs, x), datum4(abs), "abs {x}");
        let (x, neg, abs) = (f64::from(x), f64::from(neg), f64::from(abs));
        assert_eq!(float8(FloatOp::Neg, x), neg.to_bits(), "-{x}");
        assert_eq!(float8(FloatOp::Abs, x), abs.to_bits(), "abs {x}");
    }
}

#[test]
fn integers_round_once_to_each_float() {
    // 2^62 + 2^38 + 1: above the midpoint of two float4 only by its last
    // bit, which a rounding through float8 first loses.
    let above = (1_i64 << 62) + (1 << 38) + 1;
    for (value, bits) in [(above, 0x5e80_0001_u32), (-above, 0xde80_0001)] {
        assert_eq!(value.float4().unwrap().to_bits(), bits, "{value}");
        let row =
            one_row(|rows, values, non_nulls| to_float4(&Scalar(value), rows, values, non_nulls));
        assert_eq!(row.unwrap(), datum4(f32::from_bits(bits)), "{value}");
    }
    // 2^24 + 1 is exact in float8, not in float4.
    for value in [16_777_217_i64, -16_777_217] {
        let row =
            one_row(|rows, values, non_nulls| to_float8(&Scalar(value), rows, values, non_nulls));
        assert_eq!(
            f64::from_bits(row.unwrap()),
            if value > 0 {
                16_777_217.0
            } else {
                -16_777_217.0
            }
        );
        assert_eq!(
            (value as i32).float4(),
            Ok(if value > 0 {
                16_777_216.0
            } else {
                -16_777_216.0
            })
        );
    }
}

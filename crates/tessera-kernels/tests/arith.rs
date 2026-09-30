#![deny(unsafe_code)]

//! int4 and int8 arithmetic: every check is written once over [`Lane`] and
//! runs for both widths as `<check>_int4` and `<check>_int8`.

use std::fmt::{Debug, Display};
use std::mem::MaybeUninit;
use std::ops::{Add, Rem, Shl, Sub};

use anyhow::Result;
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView};
use tessera_kernels::ops::{ArithOp, ArithmeticError};
use tessera_kernels::{int32, int64};

/// One integer width under test: its kernels, its error, and the values its
/// checks use beyond those shared by both widths.
trait Lane:
    Copy
    + Debug
    + Display
    + PartialEq
    + From<i32>
    + Into<i128>
    + TryFrom<i128>
    + Add<Output = Self>
    + Sub<Output = Self>
    + Rem<Output = Self>
    + Shl<u32, Output = Self>
    + 'static
{
    const MIN: Self;
    const MAX: Self;
    /// What an untouched result slot holds.
    const SENTINEL: Self;
    /// The error of a result that does not fit the width.
    const OUT_OF_RANGE: ArithmeticError;
    /// Two powers of two whose product is the first one past `MAX`.
    const PRODUCT_PAST_MAX: (Self, Self);
    /// Semantics cases past the int4 range, so that a 32-bit slip shows.
    const CASES_PAST_INT4: &'static [Case<Self>];
    /// Scalars past the int4 range, appended to the scalar sets.
    const SCALARS_PAST_INT4: &'static [Self];
    /// Divisors past the int4 range, before the extremes and ±1, 0.
    const DIVISORS_PAST_INT4: &'static [Self];
    /// A large left scalar for a column of small divisors.
    const LARGE_LEFT_SCALAR: Self;
    /// The shift of the random left column of the model check.
    const RANDOM_LEFT_SHIFT: u32;
    /// The shift of the random left column of the whole-word check.
    const WHOLE_WORD_LEFT_SHIFT: u32;

    /// The low bits of a random number, as `as` casts them.
    fn truncate(bits: u64) -> Self;
    /// A random number spread over the whole width.
    fn full_range(random: u64) -> Self;

    fn arith_scalar<C: ColumnReader<Value = Self>>(
        op: ArithOp,
        column: &C,
        scalar: Self,
        rows: &RowMaskView<'_>,
        values: &mut [MaybeUninit<Self>],
        non_nulls: &mut RowMask<'_>,
    ) -> Result<()>;
    fn arith_scalar_left<C: ColumnReader<Value = Self>>(
        op: ArithOp,
        scalar: Self,
        column: &C,
        rows: &RowMaskView<'_>,
        values: &mut [MaybeUninit<Self>],
        non_nulls: &mut RowMask<'_>,
    ) -> Result<()>;
    fn arith_columns<L: ColumnReader<Value = Self>, R: ColumnReader<Value = Self>>(
        op: ArithOp,
        left: &L,
        right: &R,
        rows: &RowMaskView<'_>,
        values: &mut [MaybeUninit<Self>],
        non_nulls: &mut RowMask<'_>,
    ) -> Result<()>;
}

/// The three kernels of one width's module.
macro_rules! kernels {
    ($module:ident) => {
        fn arith_scalar<C: ColumnReader<Value = Self>>(
            op: ArithOp,
            column: &C,
            scalar: Self,
            rows: &RowMaskView<'_>,
            values: &mut [MaybeUninit<Self>],
            non_nulls: &mut RowMask<'_>,
        ) -> Result<()> {
            $module::arith_scalar(op, column, scalar, rows, values, non_nulls)
        }
        fn arith_scalar_left<C: ColumnReader<Value = Self>>(
            op: ArithOp,
            scalar: Self,
            column: &C,
            rows: &RowMaskView<'_>,
            values: &mut [MaybeUninit<Self>],
            non_nulls: &mut RowMask<'_>,
        ) -> Result<()> {
            $module::arith_scalar_left(op, scalar, column, rows, values, non_nulls)
        }
        fn arith_columns<L: ColumnReader<Value = Self>, R: ColumnReader<Value = Self>>(
            op: ArithOp,
            left: &L,
            right: &R,
            rows: &RowMaskView<'_>,
            values: &mut [MaybeUninit<Self>],
            non_nulls: &mut RowMask<'_>,
        ) -> Result<()> {
            $module::arith_columns(op, left, right, rows, values, non_nulls)
        }
    };
}

impl Lane for i32 {
    const MIN: Self = i32::MIN;
    const MAX: Self = i32::MAX;
    const SENTINEL: Self = 0x5a5a_5a5a;
    const OUT_OF_RANGE: ArithmeticError = ArithmeticError::IntegerOutOfRange;
    const PRODUCT_PAST_MAX: (Self, Self) = (65536, 32768);
    const CASES_PAST_INT4: &'static [Case<Self>] = &[];
    const SCALARS_PAST_INT4: &'static [Self] = &[];
    const DIVISORS_PAST_INT4: &'static [Self] = &[];
    const LARGE_LEFT_SCALAR: Self = 1_000_000;
    const RANDOM_LEFT_SHIFT: u32 = 0;
    const WHOLE_WORD_LEFT_SHIFT: u32 = 0;

    fn truncate(bits: u64) -> Self {
        bits as i32
    }
    fn full_range(random: u64) -> Self {
        (random >> 32) as i32
    }
    kernels!(int32);
}

impl Lane for i64 {
    const MIN: Self = i64::MIN;
    const MAX: Self = i64::MAX;
    const SENTINEL: Self = 0x5a5a_5a5a_5a5a_5a5a;
    const OUT_OF_RANGE: ArithmeticError = ArithmeticError::BigintOutOfRange;
    const PRODUCT_PAST_MAX: (Self, Self) = (1 << 32, 1 << 31);
    /// A sum and a lane-by-lane product that fit int8 but not int4.
    const CASES_PAST_INT4: &'static [Case<Self>] = &[
        (ArithOp::Add, 1 << 40, 1 << 40, Ok(1 << 41)),
        (ArithOp::Mul, 1 << 31, 1 << 31, Ok(1 << 62)),
    ];
    const SCALARS_PAST_INT4: &'static [Self] = &[1 << 40];
    const DIVISORS_PAST_INT4: &'static [Self] = &[1 << 40, (1 << 40) + 1];
    const LARGE_LEFT_SCALAR: Self = 1_000_000_000_000;
    /// Beyond the int4 range on the left, so that a 32-bit slip shows.
    const RANDOM_LEFT_SHIFT: u32 = 33;
    const WHOLE_WORD_LEFT_SHIFT: u32 = 25;

    fn truncate(bits: u64) -> Self {
        bits as i64
    }
    fn full_range(random: u64) -> Self {
        random as i64
    }
    kernels!(int64);
}

/// Each generic check as a test per width.
macro_rules! for_both_widths {
    ($($check:ident: $int4:ident, $int8:ident;)*) => {
        $(
            #[test]
            fn $int4() -> Result<()> {
                $check::<i32>()
            }
            #[test]
            fn $int8() -> Result<()> {
                $check::<i64>()
            }
        )*
    };
}

for_both_widths! {
    semantics_follow_postgresql:
        semantics_follow_postgresql_int4, semantics_follow_postgresql_int8;
    nulls_propagate_and_unselected_rows_stay_unmarked:
        nulls_propagate_and_unselected_rows_stay_unmarked_int4,
        nulls_propagate_and_unselected_rows_stay_unmarked_int8;
    random_data_matches_the_model_in_every_shape:
        random_data_matches_the_model_in_every_shape_int4,
        random_data_matches_the_model_in_every_shape_int8;
    whole_words_agree_with_the_row_path:
        whole_words_agree_with_the_row_path_int4, whole_words_agree_with_the_row_path_int8;
    division_by_scalars_agrees_on_whole_words_with_extremes:
        division_by_scalars_agrees_on_whole_words_with_extremes_int4,
        division_by_scalars_agrees_on_whole_words_with_extremes_int8;
    a_null_row_never_fails_whatever_the_other_operand:
        a_null_row_never_fails_whatever_the_other_operand_int4,
        a_null_row_never_fails_whatever_the_other_operand_int8;
    overflow_in_null_or_unselected_lanes_is_not_an_error:
        overflow_in_null_or_unselected_lanes_is_not_an_error_int4,
        overflow_in_null_or_unselected_lanes_is_not_an_error_int8;
    dimension_errors_come_before_any_mutation:
        dimension_errors_come_before_any_mutation_int4,
        dimension_errors_come_before_any_mutation_int8;
}

/// The same values without bulk storage: every call takes the row path.
struct RowsOnly<'a, T>(&'a ColumnView<'a, T>);

impl<T: Copy> ColumnReader for RowsOnly<'_, T> {
    type Value = T;
    fn nrows(&self) -> usize {
        self.0.nrows()
    }
    fn get(&self, row: usize) -> Result<Option<T>> {
        ColumnReader::get(self.0, row)
    }
    fn word_values(
        &self,
        word_index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<T>)> + '_> {
        self.0.word_values(word_index, selected)
    }
}

const OPS: [ArithOp; 5] = [
    ArithOp::Add,
    ArithOp::Sub,
    ArithOp::Mul,
    ArithOp::Div,
    ArithOp::Mod,
];

/// An operation, its operands and PostgreSQL's answer.
type Case<T> = (ArithOp, T, T, Result<T, ArithmeticError>);

/// A small constant in the width under test.
fn int<T: Lane>(value: i32) -> T {
    T::from(value)
}

fn words_for(flags: &[bool]) -> Vec<u64> {
    let mut words = vec![0; flags.len().div_ceil(64)];
    for (row, &flag) in flags.iter().enumerate() {
        if flag {
            words[row / 64] |= 1 << (row % 64);
        }
    }
    words
}

fn random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// PostgreSQL's int4 and int8 arithmetic on i128, with its errors.
fn model<T: Lane>(op: ArithOp, a: T, b: T) -> Result<T, ArithmeticError> {
    let (a, b): (i128, i128) = (a.into(), b.into());
    let wide = match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div | ArithOp::Mod if b == 0 => return Err(ArithmeticError::DivisionByZero),
        ArithOp::Div => a / b,
        ArithOp::Mod if b == -1 => 0,
        ArithOp::Mod => a % b,
    };
    T::try_from(wide).map_err(|_| T::OUT_OF_RANGE)
}

/// One operand shape of a call.
#[derive(Clone, Copy)]
enum Shape<'a, T> {
    ColumnScalar(&'a ColumnView<'a, T>, T),
    ScalarColumn(T, &'a ColumnView<'a, T>),
    Columns(&'a ColumnView<'a, T>, &'a ColumnView<'a, T>),
}

/// The kernel's result: every slot (sentinel where unwritten) and the
/// non-null words. Slots start initialized, so reading them back is plain.
fn run<T: Lane>(
    op: ArithOp,
    shape: Shape<'_, T>,
    rows: &RowMaskView<'_>,
) -> Result<(Vec<T>, Vec<u64>)> {
    let nrows = rows.nrows();
    let mut values = vec![MaybeUninit::new(T::SENTINEL); nrows];
    let mut words = vec![0; nrows.div_ceil(64)];
    let mut mask = RowMask::try_new(nrows, &mut words)?;
    match shape {
        Shape::ColumnScalar(column, scalar) => {
            T::arith_scalar(op, column, scalar, rows, &mut values, &mut mask)?;
        }
        Shape::ScalarColumn(scalar, column) => {
            T::arith_scalar_left(op, scalar, column, rows, &mut values, &mut mask)?;
        }
        Shape::Columns(left, right) => {
            T::arith_columns(op, left, right, rows, &mut values, &mut mask)?;
        }
    }
    Ok((values.iter().map(written).collect(), words))
}

#[allow(unsafe_code)]
fn written<T: Copy>(slot: &MaybeUninit<T>) -> T {
    // SAFETY: every slot was created initialized with the sentinel and the
    // kernel only overwrites slots with initialized values.
    unsafe { slot.assume_init() }
}

fn arithmetic_error(error: &anyhow::Error) -> Option<ArithmeticError> {
    error.downcast_ref::<ArithmeticError>().copied()
}

fn semantics_follow_postgresql<T: Lane>() -> Result<()> {
    let out_of_range = Err(T::OUT_OF_RANGE);
    let by_zero = Err(ArithmeticError::DivisionByZero);
    let (product_a, product_b) = T::PRODUCT_PAST_MAX;
    let mut cases: Vec<Case<T>> = vec![
        (ArithOp::Add, int(40), int(2), Ok(int(42))),
        (ArithOp::Add, T::MAX, int(1), out_of_range),
        (ArithOp::Add, T::MIN, int(-1), out_of_range),
        (ArithOp::Sub, T::MIN, int(1), out_of_range),
        (ArithOp::Sub, T::MAX, int(-1), out_of_range),
        (ArithOp::Sub, int(-5), int(7), Ok(int(-12))),
        (ArithOp::Mul, int(-3), int(4), Ok(int(-12))),
        (ArithOp::Mul, T::MIN, int(-1), out_of_range),
        (ArithOp::Mul, T::MAX, int(2), out_of_range),
        (ArithOp::Mul, product_a, product_b, out_of_range),
        (ArithOp::Div, int(7), int(0), by_zero),
        (ArithOp::Div, T::MIN, int(-1), out_of_range),
        (ArithOp::Div, int(-7), int(2), Ok(int(-3))),
        (ArithOp::Div, int(7), int(-2), Ok(int(-3))),
        (ArithOp::Mod, int(7), int(0), by_zero),
        (ArithOp::Mod, T::MIN, int(-1), Ok(int(0))),
        (ArithOp::Mod, int(-7), int(2), Ok(int(-1))),
        (ArithOp::Mod, int(7), int(-2), Ok(int(1))),
    ];
    cases.extend_from_slice(T::CASES_PAST_INT4);
    let rows = RowMaskView::try_new(1, &[1])?;
    for (op, a, b, expected) in cases {
        assert_eq!(model(op, a, b), expected, "model {op:?} {a} {b}");
        let left = ColumnView::try_new(std::slice::from_ref(&a), None)?;
        let right = ColumnView::try_new(std::slice::from_ref(&b), None)?;
        let outcomes = [
            run(op, Shape::ColumnScalar(&left, b), &rows),
            run(op, Shape::ScalarColumn(a, &right), &rows),
            run(op, Shape::Columns(&left, &right), &rows),
        ];
        for (shape, outcome) in outcomes.into_iter().enumerate() {
            match (expected, outcome) {
                (Ok(value), Ok((values, words))) => {
                    assert_eq!(
                        (values[0], words[0]),
                        (value, 1),
                        "{op:?} {a} {b} shape {shape}"
                    );
                }
                (Err(error), Err(failure)) => {
                    assert_eq!(
                        arithmetic_error(&failure),
                        Some(error),
                        "{op:?} {a} {b} shape {shape}"
                    );
                    assert_eq!(
                        error.sqlstate(),
                        if error == ArithmeticError::DivisionByZero {
                            "22012"
                        } else {
                            "22003"
                        }
                    );
                }
                (expected, outcome) => {
                    panic!("{op:?} {a} {b} shape {shape}: expected {expected:?}, got {outcome:?}")
                }
            }
        }
    }
    Ok(())
}

fn nulls_propagate_and_unselected_rows_stay_unmarked<T: Lane>() -> Result<()> {
    // Row 1 is NULL with an operand that would overflow; row 3 is not selected.
    let values = [int(40), T::MAX, int(5), int(7)];
    let non_nulls = RowMaskView::try_new(4, &[0b1101])?;
    let column = ColumnView::try_new(&values, Some(non_nulls))?;
    let rows = RowMaskView::try_new(4, &[0b0111])?;
    for op in OPS {
        let (out, words) = run(op, Shape::ColumnScalar(&column, int(2)), &rows)?;
        assert_eq!(words, [0b0101], "{op:?}");
        assert_eq!(out[0], model(op, int(40), int(2))?, "{op:?}");
        assert_eq!(out[2], model(op, int(5), int(2))?, "{op:?}");
        assert_eq!(
            out[3],
            T::SENTINEL,
            "{op:?}: unselected rows are not written on the row path"
        );
    }
    // A NULL on either side of a column-column operation, and a zero divisor
    // hidden behind a NULL on the other side.
    let left_values: [T; 4] = [int(1), int(2), int(3), int(4)];
    let right_values: [T; 4] = [int(0), int(5), int(0), int(7)];
    let left = ColumnView::try_new(&left_values, Some(RowMaskView::try_new(4, &[0b1110])?))?;
    let right = ColumnView::try_new(&right_values, Some(RowMaskView::try_new(4, &[0b1010])?))?;
    let all = RowMaskView::try_new(4, &[0b1111])?;
    let (out, words) = run(ArithOp::Div, Shape::Columns(&left, &right), &all)?;
    assert_eq!(words, [0b1010]);
    assert_eq!((out[1], out[3]), (int(0), int(0)));
    let failure = run(ArithOp::Mod, Shape::ColumnScalar(&right, int(0)), &all).unwrap_err();
    assert_eq!(
        arithmetic_error(&failure),
        Some(ArithmeticError::DivisionByZero)
    );
    Ok(())
}

fn random_data_matches_the_model_in_every_shape<T: Lane>() -> Result<()> {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    let nrows = 3 * 64 + 7;
    // Small enough not to overflow with the scalars below, and full-range
    // divisors so that both signs and large quotients occur.
    let left: Vec<T> = (0..nrows)
        .map(|_| (T::truncate(random(&mut state) >> 8) % int(100_000)) << T::RANDOM_LEFT_SHIFT)
        .collect();
    let right: Vec<T> = (0..nrows)
        .map(|_| match random(&mut state) % 4 {
            0 => T::truncate(random(&mut state) >> 8),
            _ => T::truncate(random(&mut state) >> 8) % int(50) - int(25),
        })
        .collect();
    let non_null: Vec<bool> = (0..nrows)
        .map(|_| !random(&mut state).is_multiple_of(4))
        .collect();
    let selected: Vec<bool> = (0..nrows)
        .map(|_| random(&mut state).is_multiple_of(2))
        .collect();
    let non_null_words = words_for(&non_null);
    let left_column =
        ColumnView::try_new(&left, Some(RowMaskView::try_new(nrows, &non_null_words)?))?;
    let right_column = ColumnView::try_new(&right, None)?;
    let selected_words = words_for(&selected);
    let rows = RowMaskView::try_new(nrows, &selected_words)?;
    let mut scalars = vec![int(-7), int(3)];
    scalars.extend_from_slice(T::SCALARS_PAST_INT4);
    for op in OPS {
        for &scalar in &scalars {
            let expected: Vec<Option<Result<T, ArithmeticError>>> = (0..nrows)
                .map(|row| (selected[row] && non_null[row]).then(|| model(op, left[row], scalar)))
                .collect();
            check(
                op,
                run(op, Shape::ColumnScalar(&left_column, scalar), &rows),
                &expected,
            );
            let expected: Vec<Option<Result<T, ArithmeticError>>> = (0..nrows)
                .map(|row| (selected[row] && non_null[row]).then(|| model(op, scalar, left[row])))
                .collect();
            check(
                op,
                run(op, Shape::ScalarColumn(scalar, &left_column), &rows),
                &expected,
            );
        }
        let expected: Vec<Option<Result<T, ArithmeticError>>> = (0..nrows)
            .map(|row| (selected[row] && non_null[row]).then(|| model(op, left[row], right[row])))
            .collect();
        check(
            op,
            run(op, Shape::Columns(&left_column, &right_column), &rows),
            &expected,
        );
    }
    Ok(())
}

/// The kernel fails exactly when the model fails somewhere in the selection;
/// otherwise every selected non-NULL row matches and the mask says which.
fn check<T: Lane>(
    op: ArithOp,
    outcome: Result<(Vec<T>, Vec<u64>)>,
    expected: &[Option<Result<T, ArithmeticError>>],
) {
    let first_error = expected.iter().flatten().find_map(|result| result.err());
    match (first_error, outcome) {
        (Some(_), Err(failure)) => assert!(arithmetic_error(&failure).is_some(), "{op:?}"),
        (None, Ok((values, words))) => {
            let non_nulls = RowMaskView::try_new(expected.len(), &words).unwrap();
            for (row, expected) in expected.iter().enumerate() {
                assert_eq!(
                    non_nulls.contains(row).unwrap(),
                    expected.is_some(),
                    "{op:?} row {row}"
                );
                if let Some(Ok(value)) = expected {
                    assert_eq!(values[row], *value, "{op:?} row {row}");
                }
            }
        }
        (first_error, outcome) => panic!("{op:?}: model {first_error:?}, kernel {outcome:?}"),
    }
}

/// Run one shape on plain row-path readers, for comparison with the
/// whole-word path of `ColumnView`.
fn run_rows<T: Lane>(
    op: ArithOp,
    shape: Shape<'_, T>,
    rows: &RowMaskView<'_>,
) -> Result<(Vec<T>, Vec<u64>)> {
    let nrows = rows.nrows();
    let mut values = vec![MaybeUninit::new(T::SENTINEL); nrows];
    let mut words = vec![0; nrows.div_ceil(64)];
    let mut mask = RowMask::try_new(nrows, &mut words)?;
    match shape {
        Shape::ColumnScalar(column, scalar) => {
            T::arith_scalar(op, &RowsOnly(column), scalar, rows, &mut values, &mut mask)?;
        }
        Shape::ScalarColumn(scalar, column) => {
            T::arith_scalar_left(op, scalar, &RowsOnly(column), rows, &mut values, &mut mask)?;
        }
        Shape::Columns(left, right) => {
            T::arith_columns(
                op,
                &RowsOnly(left),
                &RowsOnly(right),
                rows,
                &mut values,
                &mut mask,
            )?;
        }
    }
    Ok((values.iter().map(written).collect(), words))
}

fn whole_words_agree_with_the_row_path<T: Lane>() -> Result<()> {
    let mut state = 0x2545_F491_4F6C_DD1D;
    let nrows = 4 * 64 + 11;
    let left: Vec<T> = (0..nrows)
        .map(|_| (T::truncate(random(&mut state) >> 8) % int(40_000)) << T::WHOLE_WORD_LEFT_SHIFT)
        .collect();
    let right: Vec<T> = (0..nrows)
        .map(|_| T::truncate(random(&mut state) >> 8) % int(30) - int(15))
        .collect();
    let non_null: Vec<bool> = (0..nrows)
        .map(|_| !random(&mut state).is_multiple_of(5))
        .collect();
    let non_null_words = words_for(&non_null);
    let left_column =
        ColumnView::try_new(&left, Some(RowMaskView::try_new(nrows, &non_null_words)?))?;
    let right_column = ColumnView::try_new(&right, None)?;
    // A full first word puts the call on the whole-word path; later words
    // range from full to sparse, single-row and empty, then the tail.
    let selected: Vec<bool> = (0..nrows)
        .map(|row| match row / 64 {
            0 => true,
            1 => random(&mut state).is_multiple_of(2),
            2 => row % 64 == 5,
            3 => false,
            _ => row % 2 == 0,
        })
        .collect();
    let words = words_for(&selected);
    let rows = RowMaskView::try_new(nrows, &words)?;
    let scalars = [int(3), int(-7), int(2), int(-4), int(641)]
        .into_iter()
        .chain(T::SCALARS_PAST_INT4.iter().copied());
    let mut shapes: Vec<Shape<'_, T>> = scalars
        .map(|scalar| Shape::ColumnScalar(&left_column, scalar))
        .collect();
    shapes.push(Shape::ScalarColumn(T::LARGE_LEFT_SCALAR, &right_column));
    shapes.push(Shape::Columns(&left_column, &right_column));
    for op in OPS {
        for &shape in &shapes {
            let whole = run(op, shape, &rows);
            let by_rows = run_rows(op, shape, &rows);
            match (whole, by_rows) {
                (Ok((values, words)), Ok((row_values, row_words))) => {
                    assert_eq!(words, row_words, "{op:?}");
                    let non_nulls = RowMaskView::try_new(nrows, &words)?;
                    for row in non_nulls.selected_indices() {
                        assert_eq!(values[row], row_values[row], "{op:?} row {row}");
                    }
                }
                (Err(whole), Err(by_rows)) => {
                    assert_eq!(
                        arithmetic_error(&whole),
                        arithmetic_error(&by_rows),
                        "{op:?}"
                    );
                    assert!(arithmetic_error(&whole).is_some(), "{op:?}");
                }
                (whole, by_rows) => panic!("{op:?}: whole {whole:?}, rows {by_rows:?}"),
            }
        }
    }
    Ok(())
}

/// Division by a scalar on whole words multiplies by a prepared reciprocal;
/// the dividends where a wrong multiplier shows are the extremes, and the
/// divisors 0 and ±1 keep their checks.
fn division_by_scalars_agrees_on_whole_words_with_extremes<T: Lane>() -> Result<()> {
    let extremes: [T; 8] = [
        T::MIN,
        T::MIN + int(1),
        int(-1),
        int(0),
        int(1),
        T::MAX - int(1),
        T::MAX,
        int(-7),
    ];
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let nrows = 2 * 64 + 9;
    let values: Vec<T> = (0..nrows)
        .map(|row| {
            if row % 3 == 0 {
                extremes[row / 3 % extremes.len()]
            } else {
                T::full_range(random(&mut state))
            }
        })
        .collect();
    // A full first word puts the call on the whole-word path.
    let selected: Vec<bool> = (0..nrows).map(|row| row < 64 || row % 5 != 0).collect();
    let words = words_for(&selected);
    let rows = RowMaskView::try_new(nrows, &words)?;
    let divisors: Vec<T> = [2, -2, 3, -7, 4, -4, 641, 1 << 30]
        .into_iter()
        .map(int)
        .chain(T::DIVISORS_PAST_INT4.iter().copied())
        .chain([T::MIN, T::MAX, int(1), int(-1), int(0)])
        .collect();
    // Scattered NULLs, and a first word of nothing but NULLs, which divides
    // nothing and leaves the divisor to be prepared at the second word.
    let patterns: [fn(usize) -> bool; 2] = [|row| row % 11 != 4, |row| row >= 64 && row % 11 != 4];
    for pattern in patterns {
        let non_null: Vec<bool> = (0..nrows).map(pattern).collect();
        let non_null_words = words_for(&non_null);
        let column =
            ColumnView::try_new(&values, Some(RowMaskView::try_new(nrows, &non_null_words)?))?;
        for op in [ArithOp::Div, ArithOp::Mod] {
            for &scalar in &divisors {
                let shape = Shape::ColumnScalar(&column, scalar);
                let expected: Vec<Option<Result<T, ArithmeticError>>> = (0..nrows)
                    .map(|row| {
                        (selected[row] && non_null[row]).then(|| model(op, values[row], scalar))
                    })
                    .collect();
                check(op, run(op, shape, &rows), &expected);
                check(op, run_rows(op, shape, &rows), &expected);
            }
        }
    }
    let non_null: Vec<bool> = (0..nrows).map(patterns[0]).collect();
    let non_null_words = words_for(&non_null);
    let column = ColumnView::try_new(&values, Some(RowMaskView::try_new(nrows, &non_null_words)?))?;
    let failure = run(ArithOp::Div, Shape::ColumnScalar(&column, int(-1)), &rows).unwrap_err();
    assert_eq!(arithmetic_error(&failure), Some(T::OUT_OF_RANGE));
    let failure = run(ArithOp::Mod, Shape::ColumnScalar(&column, int(0)), &rows).unwrap_err();
    assert_eq!(
        arithmetic_error(&failure),
        Some(ArithmeticError::DivisionByZero)
    );
    Ok(())
}

/// A NULL row never fails, whatever the other operand holds: an extreme
/// scalar or column value beside a NULL computes nothing, on the row path
/// (five rows, every target) and on whole words, a word of a single row
/// and a partial tail (3·64 + 5 rows); beside non-NULL rows of a harmless
/// value the model decides. `MAX + NULL` is NULL, as in PostgreSQL.
fn a_null_row_never_fails_whatever_the_other_operand<T: Lane>() -> Result<()> {
    for nrows in [5_usize, 3 * 64 + 5] {
        let mut selection = vec![u64::MAX; nrows.div_ceil(64)];
        if nrows > 64 {
            selection[1] = 1 << 7;
        }
        if nrows % 64 != 0 {
            *selection.last_mut().unwrap() &= (1 << (nrows % 64)) - 1;
        }
        let rows = RowMaskView::try_new(nrows, &selection)?;
        let selected = |row: usize| selection[row / 64] >> (row % 64) & 1 == 1;
        let none = vec![0; nrows.div_ceil(64)];
        let mixed_flags: Vec<bool> = (0..nrows).map(|row| row % 3 != 0).collect();
        let mixed_words = words_for(&mixed_flags);
        for extreme in [T::MIN, T::MAX] {
            let extremes = vec![extreme; nrows];
            let nulls = ColumnView::try_new(&extremes, Some(RowMaskView::try_new(nrows, &none)?))?;
            let extreme_column = ColumnView::try_new(&extremes, None)?;
            for op in OPS {
                // A value that no operation with the extreme rejects.
                let harmless = if matches!(op, ArithOp::Div | ArithOp::Mod) {
                    int(1)
                } else {
                    int(0)
                };
                let mixed_values: Vec<T> = mixed_flags
                    .iter()
                    .map(|&flag| if flag { harmless } else { T::MIN })
                    .collect();
                let mixed = ColumnView::try_new(
                    &mixed_values,
                    Some(RowMaskView::try_new(nrows, &mixed_words)?),
                )?;
                for runner in [run, run_rows] {
                    for shape in [
                        Shape::ScalarColumn(extreme, &nulls),
                        Shape::Columns(&extreme_column, &nulls),
                        Shape::Columns(&nulls, &extreme_column),
                        Shape::ColumnScalar(&nulls, extreme),
                    ] {
                        let (_, words) = runner(op, shape, &rows)?;
                        assert!(words.iter().all(|&word| word == 0), "{op:?} {extreme}");
                    }
                    let expected: Vec<_> = (0..nrows)
                        .map(|row| {
                            (selected(row) && mixed_flags[row])
                                .then(|| model(op, extreme, harmless))
                        })
                        .collect();
                    check(
                        op,
                        runner(op, Shape::ScalarColumn(extreme, &mixed), &rows),
                        &expected,
                    );
                    check(
                        op,
                        runner(op, Shape::Columns(&extreme_column, &mixed), &rows),
                        &expected,
                    );
                }
            }
        }
    }
    Ok(())
}

fn overflow_in_null_or_unselected_lanes_is_not_an_error<T: Lane>() -> Result<()> {
    // Word 0 is full of extremes in its NULL rows; word 1 is not selected.
    let values: Vec<T> = (0..128)
        .map(|row| if row % 2 == 0 { int(3) } else { T::MAX })
        .collect();
    let non_null: Vec<bool> = (0..128).map(|row| row % 2 == 0 || row >= 64).collect();
    let non_null_words = words_for(&non_null);
    let column = ColumnView::try_new(&values, Some(RowMaskView::try_new(128, &non_null_words)?))?;
    let all = RowMaskView::try_new(128, &[u64::MAX, 0])?;
    for op in [ArithOp::Add, ArithOp::Sub, ArithOp::Mul] {
        let (out, words) = run(op, Shape::ColumnScalar(&column, int(7)), &all)?;
        assert_eq!(words, [non_null_words[0], 0], "{op:?}");
        assert_eq!(out[0], model(op, int(3), int(7))?, "{op:?}");
        assert_eq!(out[2], model(op, int(3), int(7))?, "{op:?}");
    }
    // Overflow in a selected non-NULL lane of a whole word is reported.
    let second = RowMaskView::try_new(128, &[u64::MAX, u64::MAX])?;
    for op in [ArithOp::Add, ArithOp::Mul] {
        let failure = run(op, Shape::ColumnScalar(&column, int(7)), &second).unwrap_err();
        assert_eq!(arithmetic_error(&failure), Some(T::OUT_OF_RANGE), "{op:?}");
    }
    assert_eq!(
        arithmetic_error(
            &run(ArithOp::Sub, Shape::ScalarColumn(int(-7), &column), &second).unwrap_err()
        ),
        Some(T::OUT_OF_RANGE)
    );
    // A zero divisor in a NULL lane of a whole word does not divide.
    let divisors: Vec<T> = (0..128)
        .map(|row| if row % 2 == 0 { int(2) } else { int(0) })
        .collect();
    let divisor =
        ColumnView::try_new(&divisors, Some(RowMaskView::try_new(128, &non_null_words)?))?;
    let (out, words) = run(ArithOp::Div, Shape::Columns(&column, &divisor), &all)?;
    assert_eq!(words, [non_null_words[0], 0]);
    assert_eq!(out[0], int(1));
    let failure = run(ArithOp::Mod, Shape::ScalarColumn(int(9), &divisor), &second).unwrap_err();
    assert_eq!(
        arithmetic_error(&failure),
        Some(ArithmeticError::DivisionByZero)
    );
    Ok(())
}

fn dimension_errors_come_before_any_mutation<T: Lane>() -> Result<()> {
    let values: [T; 3] = [int(1), int(2), int(3)];
    let column = ColumnView::try_new(&values, None)?;
    let rows = RowMaskView::try_new(3, &[0b111])?;
    let mut out = [MaybeUninit::new(T::SENTINEL); 3];
    let mut words = [0];
    let short_rows = RowMaskView::try_new(2, &[0b11])?;
    let mut mask = RowMask::try_new(3, &mut words)?;
    assert!(
        T::arith_scalar(
            ArithOp::Add,
            &column,
            int(2),
            &short_rows,
            &mut out,
            &mut mask
        )
        .is_err()
    );
    let mut short_out = [MaybeUninit::new(T::SENTINEL); 2];
    assert!(
        T::arith_scalar(
            ArithOp::Add,
            &column,
            int(2),
            &rows,
            &mut short_out,
            &mut mask
        )
        .is_err()
    );
    let short_values: [T; 2] = [int(1), int(2)];
    let short_column = ColumnView::try_new(&short_values, None)?;
    assert!(
        T::arith_columns(
            ArithOp::Add,
            &column,
            &short_column,
            &rows,
            &mut out,
            &mut mask
        )
        .is_err()
    );
    assert!(
        T::arith_scalar_left(
            ArithOp::Add,
            int(2),
            &short_column,
            &rows,
            &mut out,
            &mut mask
        )
        .is_err()
    );
    assert_eq!(mask.as_view().selected_count(), 0);
    assert!(out.iter().all(|slot| written(slot) == T::SENTINEL));
    Ok(())
}

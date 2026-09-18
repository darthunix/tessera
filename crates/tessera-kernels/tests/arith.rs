#![deny(unsafe_code)]

use std::mem::MaybeUninit;

use anyhow::Result;
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView};
use tessera_kernels::int32::{
    ArithOp, ArithmeticError, arith_columns, arith_scalar, arith_scalar_left,
};

/// The same values without bulk storage: every call takes the row path.
struct RowsOnly<'a>(&'a ColumnView<'a, i32>);

impl ColumnReader for RowsOnly<'_> {
    type Value = i32;
    fn nrows(&self) -> usize {
        self.0.nrows()
    }
    fn get(&self, row: usize) -> Result<Option<i32>> {
        ColumnReader::get(self.0, row)
    }
    fn word_values(
        &self,
        word_index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<i32>)> + '_> {
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
/// What an untouched result slot holds.
const SENTINEL: i32 = 0x5a5a_5a5a;

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

/// PostgreSQL's int4 arithmetic on i64, with its errors.
fn model(op: ArithOp, a: i32, b: i32) -> Result<i32, ArithmeticError> {
    let (a, b) = (i64::from(a), i64::from(b));
    let wide = match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div | ArithOp::Mod if b == 0 => return Err(ArithmeticError::DivisionByZero),
        ArithOp::Div => a / b,
        ArithOp::Mod if b == -1 => 0,
        ArithOp::Mod => a % b,
    };
    i32::try_from(wide).map_err(|_| ArithmeticError::IntegerOutOfRange)
}

/// One operand shape of a call.
#[derive(Clone, Copy)]
enum Shape<'a> {
    ColumnScalar(&'a ColumnView<'a, i32>, i32),
    ScalarColumn(i32, &'a ColumnView<'a, i32>),
    Columns(&'a ColumnView<'a, i32>, &'a ColumnView<'a, i32>),
}

/// The kernel's result: every slot (sentinel where unwritten) and the
/// non-null words. Slots start initialized, so reading them back is plain.
fn run(op: ArithOp, shape: Shape<'_>, rows: &RowMaskView<'_>) -> Result<(Vec<i32>, Vec<u64>)> {
    let nrows = rows.nrows();
    let mut values = vec![MaybeUninit::new(SENTINEL); nrows];
    let mut words = vec![0; nrows.div_ceil(64)];
    let mut mask = RowMask::try_new(nrows, &mut words)?;
    match shape {
        Shape::ColumnScalar(column, scalar) => {
            arith_scalar(op, column, scalar, rows, &mut values, &mut mask)?;
        }
        Shape::ScalarColumn(scalar, column) => {
            arith_scalar_left(op, scalar, column, rows, &mut values, &mut mask)?;
        }
        Shape::Columns(left, right) => {
            arith_columns(op, left, right, rows, &mut values, &mut mask)?;
        }
    }
    Ok((values.iter().map(written).collect(), words))
}

#[allow(unsafe_code)]
fn written(slot: &MaybeUninit<i32>) -> i32 {
    // SAFETY: every slot was created initialized with the sentinel and the
    // kernel only overwrites slots with initialized values.
    unsafe { slot.assume_init() }
}

fn arithmetic_error(error: &anyhow::Error) -> Option<ArithmeticError> {
    error.downcast_ref::<ArithmeticError>().copied()
}

#[test]
fn semantics_follow_postgresql() -> Result<()> {
    let cases: [(ArithOp, i32, i32, Result<i32, ArithmeticError>); 18] = [
        (ArithOp::Add, 40, 2, Ok(42)),
        (
            ArithOp::Add,
            i32::MAX,
            1,
            Err(ArithmeticError::IntegerOutOfRange),
        ),
        (
            ArithOp::Add,
            i32::MIN,
            -1,
            Err(ArithmeticError::IntegerOutOfRange),
        ),
        (
            ArithOp::Sub,
            i32::MIN,
            1,
            Err(ArithmeticError::IntegerOutOfRange),
        ),
        (
            ArithOp::Sub,
            i32::MAX,
            -1,
            Err(ArithmeticError::IntegerOutOfRange),
        ),
        (ArithOp::Sub, -5, 7, Ok(-12)),
        (ArithOp::Mul, -3, 4, Ok(-12)),
        (
            ArithOp::Mul,
            i32::MIN,
            -1,
            Err(ArithmeticError::IntegerOutOfRange),
        ),
        (
            ArithOp::Mul,
            i32::MAX,
            2,
            Err(ArithmeticError::IntegerOutOfRange),
        ),
        (
            ArithOp::Mul,
            65536,
            32768,
            Err(ArithmeticError::IntegerOutOfRange),
        ),
        (ArithOp::Div, 7, 0, Err(ArithmeticError::DivisionByZero)),
        (
            ArithOp::Div,
            i32::MIN,
            -1,
            Err(ArithmeticError::IntegerOutOfRange),
        ),
        (ArithOp::Div, -7, 2, Ok(-3)),
        (ArithOp::Div, 7, -2, Ok(-3)),
        (ArithOp::Mod, 7, 0, Err(ArithmeticError::DivisionByZero)),
        (ArithOp::Mod, i32::MIN, -1, Ok(0)),
        (ArithOp::Mod, -7, 2, Ok(-1)),
        (ArithOp::Mod, 7, -2, Ok(1)),
    ];
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

#[test]
fn nulls_propagate_and_unselected_rows_stay_unmarked() -> Result<()> {
    // Row 1 is NULL with an operand that would overflow; row 3 is not selected.
    let values = [40, i32::MAX, 5, 7];
    let non_nulls = RowMaskView::try_new(4, &[0b1101])?;
    let column = ColumnView::try_new(&values, Some(non_nulls))?;
    let rows = RowMaskView::try_new(4, &[0b0111])?;
    for op in OPS {
        let (out, words) = run(op, Shape::ColumnScalar(&column, 2), &rows)?;
        assert_eq!(words, [0b0101], "{op:?}");
        assert_eq!(out[0], model(op, 40, 2)?, "{op:?}");
        assert_eq!(out[2], model(op, 5, 2)?, "{op:?}");
        assert_eq!(
            out[3], SENTINEL,
            "{op:?}: unselected rows are not written on the row path"
        );
    }
    // A NULL on either side of a column-column operation, and a zero divisor
    // hidden behind a NULL on the other side.
    let left_values = [1, 2, 3, 4];
    let right_values = [0, 5, 0, 7];
    let left = ColumnView::try_new(&left_values, Some(RowMaskView::try_new(4, &[0b1110])?))?;
    let right = ColumnView::try_new(&right_values, Some(RowMaskView::try_new(4, &[0b1010])?))?;
    let all = RowMaskView::try_new(4, &[0b1111])?;
    let (out, words) = run(ArithOp::Div, Shape::Columns(&left, &right), &all)?;
    assert_eq!(words, [0b1010]);
    assert_eq!((out[1], out[3]), (0, 0));
    let failure = run(ArithOp::Mod, Shape::ColumnScalar(&right, 0), &all).unwrap_err();
    assert_eq!(
        arithmetic_error(&failure),
        Some(ArithmeticError::DivisionByZero)
    );
    Ok(())
}

#[test]
fn random_data_matches_the_model_in_every_shape() -> Result<()> {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    let nrows = 3 * 64 + 7;
    // Small enough not to overflow with the scalars below, and full-range
    // divisors so that both signs and large quotients occur.
    let left: Vec<i32> = (0..nrows)
        .map(|_| (random(&mut state) >> 8) as i32 % 100_000)
        .collect();
    let right: Vec<i32> = (0..nrows)
        .map(|_| match random(&mut state) % 4 {
            0 => (random(&mut state) >> 8) as i32,
            _ => (random(&mut state) >> 8) as i32 % 50 - 25,
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
    for op in OPS {
        for scalar in [-7, 3] {
            let expected: Vec<Option<Result<i32, ArithmeticError>>> = (0..nrows)
                .map(|row| (selected[row] && non_null[row]).then(|| model(op, left[row], scalar)))
                .collect();
            check(
                op,
                run(op, Shape::ColumnScalar(&left_column, scalar), &rows),
                &expected,
            );
            let expected: Vec<Option<Result<i32, ArithmeticError>>> = (0..nrows)
                .map(|row| (selected[row] && non_null[row]).then(|| model(op, scalar, left[row])))
                .collect();
            check(
                op,
                run(op, Shape::ScalarColumn(scalar, &left_column), &rows),
                &expected,
            );
        }
        let expected: Vec<Option<Result<i32, ArithmeticError>>> = (0..nrows)
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
fn check(
    op: ArithOp,
    outcome: Result<(Vec<i32>, Vec<u64>)>,
    expected: &[Option<Result<i32, ArithmeticError>>],
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
fn run_rows(op: ArithOp, shape: Shape<'_>, rows: &RowMaskView<'_>) -> Result<(Vec<i32>, Vec<u64>)> {
    let nrows = rows.nrows();
    let mut values = vec![MaybeUninit::new(SENTINEL); nrows];
    let mut words = vec![0; nrows.div_ceil(64)];
    let mut mask = RowMask::try_new(nrows, &mut words)?;
    match shape {
        Shape::ColumnScalar(column, scalar) => {
            arith_scalar(op, &RowsOnly(column), scalar, rows, &mut values, &mut mask)?;
        }
        Shape::ScalarColumn(scalar, column) => {
            arith_scalar_left(op, scalar, &RowsOnly(column), rows, &mut values, &mut mask)?;
        }
        Shape::Columns(left, right) => {
            arith_columns(
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

#[test]
fn whole_words_agree_with_the_row_path() -> Result<()> {
    let mut state = 0x2545_F491_4F6C_DD1D;
    let nrows = 4 * 64 + 11;
    let left: Vec<i32> = (0..nrows)
        .map(|_| (random(&mut state) >> 8) as i32 % 40_000)
        .collect();
    let right: Vec<i32> = (0..nrows)
        .map(|_| (random(&mut state) >> 8) as i32 % 30 - 15)
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
    for op in OPS {
        for shape in [
            Shape::ColumnScalar(&left_column, 3),
            Shape::ColumnScalar(&left_column, -7),
            Shape::ColumnScalar(&left_column, 2),
            Shape::ColumnScalar(&left_column, -4),
            Shape::ColumnScalar(&left_column, 641),
            Shape::ScalarColumn(1_000_000, &right_column),
            Shape::Columns(&left_column, &right_column),
        ] {
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
#[test]
fn division_by_scalars_agrees_on_whole_words_with_extremes() -> Result<()> {
    const EXTREMES: [i32; 8] = [i32::MIN, i32::MIN + 1, -1, 0, 1, i32::MAX - 1, i32::MAX, -7];
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let nrows = 2 * 64 + 9;
    let values: Vec<i32> = (0..nrows)
        .map(|row| {
            if row % 3 == 0 {
                EXTREMES[row / 3 % EXTREMES.len()]
            } else {
                (random(&mut state) >> 32) as i32
            }
        })
        .collect();
    let non_null: Vec<bool> = (0..nrows).map(|row| row % 11 != 4).collect();
    let non_null_words = words_for(&non_null);
    let column = ColumnView::try_new(&values, Some(RowMaskView::try_new(nrows, &non_null_words)?))?;
    // A full first word puts the call on the whole-word path.
    let selected: Vec<bool> = (0..nrows).map(|row| row < 64 || row % 5 != 0).collect();
    let words = words_for(&selected);
    let rows = RowMaskView::try_new(nrows, &words)?;
    for op in [ArithOp::Div, ArithOp::Mod] {
        for scalar in [
            2,
            -2,
            3,
            -7,
            4,
            -4,
            641,
            1 << 30,
            i32::MIN,
            i32::MAX,
            1,
            -1,
            0,
        ] {
            let shape = Shape::ColumnScalar(&column, scalar);
            let expected: Vec<Option<Result<i32, ArithmeticError>>> = (0..nrows)
                .map(|row| (selected[row] && non_null[row]).then(|| model(op, values[row], scalar)))
                .collect();
            check(op, run(op, shape, &rows), &expected);
            check(op, run_rows(op, shape, &rows), &expected);
        }
    }
    let failure = run(ArithOp::Div, Shape::ColumnScalar(&column, -1), &rows).unwrap_err();
    assert_eq!(
        arithmetic_error(&failure),
        Some(ArithmeticError::IntegerOutOfRange)
    );
    let failure = run(ArithOp::Mod, Shape::ColumnScalar(&column, 0), &rows).unwrap_err();
    assert_eq!(
        arithmetic_error(&failure),
        Some(ArithmeticError::DivisionByZero)
    );
    Ok(())
}

#[test]
fn overflow_in_null_or_unselected_lanes_is_not_an_error() -> Result<()> {
    // Word 0 is full of extremes in its NULL rows; word 1 is not selected.
    let values: Vec<i32> = (0..128)
        .map(|row| if row % 2 == 0 { 3 } else { i32::MAX })
        .collect();
    let non_null: Vec<bool> = (0..128).map(|row| row % 2 == 0 || row >= 64).collect();
    let non_null_words = words_for(&non_null);
    let column = ColumnView::try_new(&values, Some(RowMaskView::try_new(128, &non_null_words)?))?;
    let all = RowMaskView::try_new(128, &[u64::MAX, 0])?;
    for op in [ArithOp::Add, ArithOp::Sub, ArithOp::Mul] {
        let (out, words) = run(op, Shape::ColumnScalar(&column, 7), &all)?;
        assert_eq!(words, [non_null_words[0], 0], "{op:?}");
        assert_eq!(out[0], model(op, 3, 7)?, "{op:?}");
        assert_eq!(out[2], model(op, 3, 7)?, "{op:?}");
    }
    // Overflow in a selected non-NULL lane of a whole word is reported.
    let second = RowMaskView::try_new(128, &[u64::MAX, u64::MAX])?;
    for op in [ArithOp::Add, ArithOp::Mul] {
        let failure = run(op, Shape::ColumnScalar(&column, 7), &second).unwrap_err();
        assert_eq!(
            arithmetic_error(&failure),
            Some(ArithmeticError::IntegerOutOfRange),
            "{op:?}"
        );
    }
    assert_eq!(
        arithmetic_error(
            &run(ArithOp::Sub, Shape::ScalarColumn(-7, &column), &second).unwrap_err()
        ),
        Some(ArithmeticError::IntegerOutOfRange)
    );
    // A zero divisor in a NULL lane of a whole word does not divide.
    let divisors: Vec<i32> = (0..128)
        .map(|row| if row % 2 == 0 { 2 } else { 0 })
        .collect();
    let divisor =
        ColumnView::try_new(&divisors, Some(RowMaskView::try_new(128, &non_null_words)?))?;
    let (out, words) = run(ArithOp::Div, Shape::Columns(&column, &divisor), &all)?;
    assert_eq!(words, [non_null_words[0], 0]);
    assert_eq!(out[0], 1);
    let failure = run(ArithOp::Mod, Shape::ScalarColumn(9, &divisor), &second).unwrap_err();
    assert_eq!(
        arithmetic_error(&failure),
        Some(ArithmeticError::DivisionByZero)
    );
    Ok(())
}

#[test]
fn dimension_errors_come_before_any_mutation() -> Result<()> {
    let values = [1, 2, 3];
    let column = ColumnView::try_new(&values, None)?;
    let rows = RowMaskView::try_new(3, &[0b111])?;
    let mut out = [MaybeUninit::new(SENTINEL); 3];
    let mut words = [0];
    let short_rows = RowMaskView::try_new(2, &[0b11])?;
    let mut mask = RowMask::try_new(3, &mut words)?;
    assert!(arith_scalar(ArithOp::Add, &column, 2, &short_rows, &mut out, &mut mask).is_err());
    let mut short_out = [MaybeUninit::new(SENTINEL); 2];
    assert!(arith_scalar(ArithOp::Add, &column, 2, &rows, &mut short_out, &mut mask).is_err());
    let short_values = [1, 2];
    let short_column = ColumnView::try_new(&short_values, None)?;
    assert!(
        arith_columns(
            ArithOp::Add,
            &column,
            &short_column,
            &rows,
            &mut out,
            &mut mask
        )
        .is_err()
    );
    assert!(arith_scalar_left(ArithOp::Add, 2, &short_column, &rows, &mut out, &mut mask).is_err());
    assert_eq!(mask.as_view().selected_count(), 0);
    assert!(out.iter().all(|slot| written(slot) == SENTINEL));
    Ok(())
}

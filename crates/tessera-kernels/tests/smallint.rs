#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]
#![deny(unsafe_code)]

//! smallint arithmetic: every operation in every operand shape against
//! PostgreSQL's smallint operators on i16, on the whole-word path of
//! `ColumnView` and on the row path, and the error of the first row that
//! fails, exactly, whichever kind it is.

use std::mem::MaybeUninit;

use anyhow::Result;
use proptest::prelude::*;
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView};
use tessera_kernels::smallint::{self, ArithOp, ArithmeticError};
use tessera_testing::{flags, nrows, property, words};

const OPS: [ArithOp; 5] = [
    ArithOp::Add,
    ArithOp::Sub,
    ArithOp::Mul,
    ArithOp::Div,
    ArithOp::Mod,
];

/// What an untouched result slot holds.
const SENTINEL: i32 = 0x5EED;

/// PostgreSQL's smallint operators (`int2pl` … `int2mod`) on i16.
fn model(op: ArithOp, a: i32, b: i32) -> Result<i32, ArithmeticError> {
    let (a, b) = (i16::try_from(a).unwrap(), i16::try_from(b).unwrap());
    let result = match op {
        ArithOp::Add => a.checked_add(b),
        ArithOp::Sub => a.checked_sub(b),
        ArithOp::Mul => a.checked_mul(b),
        ArithOp::Div | ArithOp::Mod if b == 0 => return Err(ArithmeticError::DivisionByZero),
        ArithOp::Div => a.checked_div(b),
        // -32768 % -1 is 0, as int2mod defines it.
        ArithOp::Mod => Some(a.wrapping_rem(b)),
    };
    result
        .map(i32::from)
        .ok_or(ArithmeticError::SmallintOutOfRange)
}

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

/// One operand shape of a call.
#[derive(Clone, Copy)]
enum Shape<'a> {
    ColumnScalar(&'a ColumnView<'a, i32>, i32),
    ScalarColumn(i32, &'a ColumnView<'a, i32>),
    Columns(&'a ColumnView<'a, i32>, &'a ColumnView<'a, i32>),
}

/// The kernel's result, on the whole-word path or the row path: every slot
/// (the sentinel where unwritten) and the non-null words.
fn run(
    op: ArithOp,
    shape: Shape<'_>,
    rows: &RowMaskView<'_>,
    row_path: bool,
) -> Result<(Vec<i32>, Vec<u64>)> {
    let nrows = rows.nrows();
    let mut values = vec![MaybeUninit::new(SENTINEL); nrows];
    let mut words = vec![0; nrows.div_ceil(64)];
    let mut mask = RowMask::try_new(nrows, &mut words)?;
    let (out, mask) = (&mut values, &mut mask);
    match (shape, row_path) {
        (Shape::ColumnScalar(c, s), false) => smallint::arith_scalar(op, c, s, rows, out, mask)?,
        (Shape::ColumnScalar(c, s), true) => {
            smallint::arith_scalar(op, &RowsOnly(c), s, rows, out, mask)?;
        }
        (Shape::ScalarColumn(s, c), false) => {
            smallint::arith_scalar_left(op, s, c, rows, out, mask)?;
        }
        (Shape::ScalarColumn(s, c), true) => {
            smallint::arith_scalar_left(op, s, &RowsOnly(c), rows, out, mask)?;
        }
        (Shape::Columns(l, r), false) => smallint::arith_columns(op, l, r, rows, out, mask)?,
        (Shape::Columns(l, r), true) => {
            smallint::arith_columns(op, &RowsOnly(l), &RowsOnly(r), rows, out, mask)?;
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

/// The kernel fails exactly when the model fails at a selected non-NULL
/// row, with the error of the first such row; otherwise every such row
/// matches and the mask says which.
fn check(
    what: &str,
    outcome: Result<(Vec<i32>, Vec<u64>)>,
    expected: &[Option<Result<i32, ArithmeticError>>],
) {
    let first = expected.iter().flatten().find_map(|result| result.err());
    match (first, outcome) {
        (Some(first), Err(failure)) => assert_eq!(
            failure.downcast_ref::<ArithmeticError>(),
            Some(&first),
            "{what}: kernel {failure:?}"
        ),
        (None, Ok((values, words))) => {
            let non_nulls = RowMaskView::try_new(expected.len(), &words).unwrap();
            for (row, expected) in expected.iter().enumerate() {
                assert_eq!(
                    non_nulls.contains(row).unwrap(),
                    expected.is_some(),
                    "{what} row {row}"
                );
                if let Some(Ok(value)) = expected {
                    assert_eq!(values[row], *value, "{what} row {row}");
                }
            }
        }
        (first, outcome) => panic!("{what}: model {first:?}, kernel {outcome:?}"),
    }
}

/// A smallint leaning to the edges of the range and of the operations.
fn smallint_value() -> BoxedStrategy<i32> {
    let edges = prop::sample::select(vec![
        -32768, -32767, -256, -2, -1, 0, 1, 2, 181, 32766, 32767,
    ]);
    prop_oneof![1 => edges, 1 => any::<i16>().prop_map(i32::from), 1 => -300..=300].boxed()
}

/// One batch: the selection, both columns with their non-NULL flags, and
/// a scalar.
#[derive(Debug, Clone)]
struct Batch {
    selected: Vec<bool>,
    left: Vec<i32>,
    left_non_null: Vec<bool>,
    right: Vec<i32>,
    right_non_null: Vec<bool>,
    scalar: i32,
}

fn batches() -> impl Strategy<Value = Batch> {
    nrows()
        .prop_flat_map(|nrows| {
            (
                flags(nrows),
                prop::collection::vec(smallint_value(), nrows),
                flags(nrows),
                prop::collection::vec(smallint_value(), nrows),
                flags(nrows),
                smallint_value(),
            )
        })
        .prop_map(
            |(selected, left, left_non_null, right, right_non_null, scalar)| Batch {
                selected,
                left,
                left_non_null,
                right,
                right_non_null,
                scalar,
            },
        )
}

/// Every operation in every operand shape matches the model, with the
/// first failing row's error, on both paths.
#[test]
fn every_shape_matches_postgresql_on_both_paths() {
    property(batches(), |batch| -> Result<()> {
        let nrows = batch.selected.len();
        let selection = words(&batch.selected);
        let rows = RowMaskView::try_new(nrows, &selection)?;
        let left_words = words(&batch.left_non_null);
        let left =
            ColumnView::try_new(&batch.left, Some(RowMaskView::try_new(nrows, &left_words)?))?;
        let right_words = words(&batch.right_non_null);
        let right = ColumnView::try_new(
            &batch.right,
            Some(RowMaskView::try_new(nrows, &right_words)?),
        )?;
        let scalar = batch.scalar;
        for op in OPS {
            let shapes = [
                Shape::ColumnScalar(&left, scalar),
                Shape::ScalarColumn(scalar, &right),
                Shape::Columns(&left, &right),
            ];
            for (index, shape) in shapes.into_iter().enumerate() {
                let expected: Vec<_> = (0..nrows)
                    .map(|row| {
                        let (a, b, read) = match shape {
                            Shape::ColumnScalar(..) => {
                                (batch.left[row], scalar, batch.left_non_null[row])
                            }
                            Shape::ScalarColumn(..) => {
                                (scalar, batch.right[row], batch.right_non_null[row])
                            }
                            Shape::Columns(..) => (
                                batch.left[row],
                                batch.right[row],
                                batch.left_non_null[row] && batch.right_non_null[row],
                            ),
                        };
                        (batch.selected[row] && read).then(|| model(op, a, b))
                    })
                    .collect();
                for row_path in [false, true] {
                    let what = format!("{op:?} shape {index} rows {row_path}");
                    check(&what, run(op, shape, &rows, row_path), &expected);
                }
            }
        }
        Ok(())
    });
}

/// A full word of 64 rows, every row selected and non-NULL, whose values
/// are 1 but at the rows given.
fn word(changes: &[(usize, i32)]) -> Vec<i32> {
    let mut values = vec![1; 64];
    for &(row, value) in changes {
        values[row] = value;
    }
    values
}

fn first_error(op: ArithOp, shape: Shape<'_>, row_path: bool) -> Option<ArithmeticError> {
    let selection = [u64::MAX];
    let rows = RowMaskView::try_new(64, &selection).unwrap();
    run(op, shape, &rows, row_path)
        .err()
        .and_then(|error| error.downcast_ref::<ArithmeticError>().copied())
}

/// The batch where checking the range after the whole batch reported the
/// wrong error: row 2 `-32768 / -1`, row 5 a division by zero. The core
/// meets row 2 first; so does the kernel, in each shape, on both paths,
/// and the other way round when the division by zero comes first.
#[test]
fn the_first_failing_row_gives_its_error() {
    let range = Some(ArithmeticError::SmallintOutOfRange);
    let zero = Some(ArithmeticError::DivisionByZero);
    for row_path in [false, true] {
        let dividends = word(&[(2, -32768), (5, -32768)]);
        let dividends = ColumnView::try_new(&dividends, None).unwrap();
        let range_first = word(&[(2, -1), (5, 0)]);
        let range_first = ColumnView::try_new(&range_first, None).unwrap();
        let zero_first = word(&[(2, 0), (5, -1)]);
        let zero_first = ColumnView::try_new(&zero_first, None).unwrap();
        let columns = |divisors| Shape::Columns(&dividends, divisors);
        assert_eq!(
            first_error(ArithOp::Div, columns(&range_first), row_path),
            range
        );
        assert_eq!(
            first_error(ArithOp::Div, columns(&zero_first), row_path),
            zero
        );
        // -32768 / b, b -1 at row 1 and 0 at row 3, and the reverse.
        let left_first = word(&[(1, -1), (3, 0)]);
        let left_first = ColumnView::try_new(&left_first, None).unwrap();
        let left_zero = word(&[(1, 0), (3, -1)]);
        let left_zero = ColumnView::try_new(&left_zero, None).unwrap();
        let left = |divisors| Shape::ScalarColumn(-32768, divisors);
        assert_eq!(
            first_error(ArithOp::Div, left(&left_first), row_path),
            range
        );
        assert_eq!(first_error(ArithOp::Div, left(&left_zero), row_path), zero);
        // By a scalar -1, the least smallint at row 4; % -1 is 0.
        let least = word(&[(4, -32768)]);
        let least = ColumnView::try_new(&least, None).unwrap();
        assert_eq!(
            first_error(ArithOp::Div, Shape::ColumnScalar(&least, -1), row_path),
            range
        );
        assert_eq!(
            first_error(ArithOp::Mod, Shape::ColumnScalar(&least, -1), row_path),
            None
        );
        assert_eq!(
            first_error(ArithOp::Div, Shape::ColumnScalar(&least, 0), row_path),
            zero
        );
    }
}

/// The vector forms: a word that leaves the range at its last lane, after
/// a word that does not, fails; NULL and unselected lanes past the range
/// do not.
#[test]
fn a_whole_word_past_the_range_fails_and_absent_lanes_do_not() {
    let mut values = vec![1; 128];
    values[127] = 32767;
    let column = ColumnView::try_new(&values, None).unwrap();
    let all = [u64::MAX, u64::MAX];
    let rows = RowMaskView::try_new(128, &all).unwrap();
    for (op, scalar) in [(ArithOp::Add, 1), (ArithOp::Sub, -1), (ArithOp::Mul, 2)] {
        for row_path in [false, true] {
            let failure = run(op, Shape::ColumnScalar(&column, scalar), &rows, row_path)
                .err()
                .and_then(|error| error.downcast_ref::<ArithmeticError>().copied());
            assert_eq!(failure, Some(ArithmeticError::SmallintOutOfRange), "{op:?}");
        }
    }
    // Row 127 unselected, then NULL: no error, every other row computed.
    let selection = [u64::MAX, u64::MAX >> 1];
    let rows = RowMaskView::try_new(128, &selection).unwrap();
    let (out, _) = run(ArithOp::Add, Shape::ColumnScalar(&column, 1), &rows, false).unwrap();
    assert!(out[..127].iter().all(|&value| value == 2));
    let non_null_words = [u64::MAX, u64::MAX >> 1];
    let non_nulls = RowMaskView::try_new(128, &non_null_words).unwrap();
    let column = ColumnView::try_new(&values, Some(non_nulls)).unwrap();
    let rows = RowMaskView::try_new(128, &all).unwrap();
    let (out, words) = run(ArithOp::Add, Shape::ColumnScalar(&column, 1), &rows, false).unwrap();
    assert_eq!(words, [u64::MAX, u64::MAX >> 1]);
    assert!(out[..127].iter().all(|&value| value == 2));
}

/// The cast int2(int4) is `x + 0` over any int4: a value past the smallint
/// range at either end, and the int4 extremes, fail; the edges of the
/// range pass; a bad value in a NULL or unselected row does not fail; on
/// the whole-word path and the row path, at a row of the first word and of
/// the tail.
#[test]
fn the_cast_is_x_plus_zero_over_any_int4() {
    let nrows = 70;
    for (value, fits) in [
        (32767, true),
        (-32768, true),
        (32768, false),
        (-32769, false),
        (i32::MAX, false),
        (i32::MIN, false),
    ] {
        for row in [3, 66] {
            let mut values = vec![5; nrows];
            values[row] = value;
            for row_path in [false, true] {
                let column = ColumnView::try_new(&values, None).unwrap();
                let all = [u64::MAX, (1 << (nrows - 64)) - 1];
                let rows = RowMaskView::try_new(nrows, &all).unwrap();
                let outcome = run(
                    ArithOp::Add,
                    Shape::ColumnScalar(&column, 0),
                    &rows,
                    row_path,
                );
                match outcome {
                    Ok((out, _)) => {
                        assert!(fits, "{value} at {row} passed");
                        assert_eq!(out[row], value);
                    }
                    Err(error) => {
                        assert!(!fits, "{value} at {row}: {error}");
                        assert_eq!(
                            error.downcast_ref::<ArithmeticError>(),
                            Some(&ArithmeticError::SmallintOutOfRange)
                        );
                    }
                }
                // The bad row unselected, then NULL: no error.
                let mut unselected = all;
                unselected[row / 64] &= !(1 << (row % 64));
                let rows = RowMaskView::try_new(nrows, &unselected).unwrap();
                assert!(
                    run(
                        ArithOp::Add,
                        Shape::ColumnScalar(&column, 0),
                        &rows,
                        row_path
                    )
                    .is_ok()
                );
                let non_nulls = RowMaskView::try_new(nrows, &unselected).unwrap();
                let column = ColumnView::try_new(&values, Some(non_nulls)).unwrap();
                let rows = RowMaskView::try_new(nrows, &all).unwrap();
                assert!(
                    run(
                        ArithOp::Add,
                        Shape::ColumnScalar(&column, 0),
                        &rows,
                        row_path
                    )
                    .is_ok()
                );
            }
        }
    }
}

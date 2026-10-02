//! The batch functions of decimals against a row-by-row model: the rows
//! each keeps, computes, leaves to the caller and reads.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use proptest::prelude::*;
use tessera_core::{RowMask, RowMaskView};
use tessera_kernels::decimal::{
    self, Arg, Compare, Decimal, MAX_READ_SCALE, MAX_SCALE, Op, POWERS, Results, SUM_BOUND, Scales,
    Source, Sum, SumState, Term,
};
use tessera_testing::{bounded_sum, decimal_parts, flags, nrows, property};

/// A column of arguments by row.
#[derive(Clone, Debug)]
struct Column(Vec<Arg>);

impl Source for Column {
    fn get(&self, row: usize) -> Arg {
        self.0[row]
    }
}

/// An argument: NULL or not a decimal one time in ten each, otherwise a
/// decimal leaning to the digit edges at a scale the batch reads.
fn arg() -> BoxedStrategy<Arg> {
    let decimal = decimal_parts(MAX_READ_SCALE)
        .prop_map(|(value, scale)| Arg::Decimal(Decimal::new(value, scale).unwrap()));
    prop_oneof![1 => Just(Arg::Null), 1 => Just(Arg::Other), 8 => decimal].boxed()
}

/// Two columns of one row count and a selection.
fn pairs() -> impl Strategy<Value = (Column, Column, Vec<bool>)> {
    nrows().prop_flat_map(|nrows| {
        (
            proptest::collection::vec(arg(), nrows).prop_map(Column),
            proptest::collection::vec(arg(), nrows).prop_map(Column),
            flags(nrows),
        )
    })
}

fn selection(selected: &[bool]) -> Vec<u64> {
    words(selected.len(), |row| selected[row])
}

fn words(nrows: usize, bits: impl Fn(usize) -> bool) -> Vec<u64> {
    let mut words = vec![0; nrows.div_ceil(64)];
    for row in (0..nrows).filter(|&row| bits(row)) {
        words[row / 64] |= 1 << (row % 64);
    }
    words
}

fn bit(words: &[u64], row: usize) -> bool {
    words[row / 64] >> (row % 64) & 1 == 1
}

/// The exact result of an operation over i128, or `None` past 18 digits,
/// by the definitions of `add_var` and `mul_var`.
fn reference(op: Op, left: Decimal, right: Decimal) -> Option<Decimal> {
    let at = |decimal: Decimal, scale: u32| {
        i128::from(decimal.value()) * 10_i128.pow(scale - decimal.scale())
    };
    let scale = left.scale().max(right.scale());
    let (value, scale) = match op {
        Op::Add => (at(left, scale) + at(right, scale), scale),
        Op::Sub => (at(left, scale) - at(right, scale), scale),
        Op::Mul => (
            i128::from(left.value()) * i128::from(right.value()),
            left.scale() + right.scale(),
        ),
        Op::Negate => (-i128::from(left.value()), left.scale()),
        Op::Abs => (i128::from(left.value()).abs(), left.scale()),
    };
    if value.abs() >= i128::from(POWERS[18]) || scale > MAX_SCALE {
        return None;
    }
    Decimal::new(value as i64, scale)
}

#[test]
fn filter_keeps_the_rows_that_hold_and_leaves_the_rest() {
    property(pairs(), |(left, right, selected)| -> Result<()> {
        filter_keeps(&left, &right, &selection(&selected));
        Ok(())
    });
}

fn filter_keeps(left: &Column, right: &Column, selected: &[u64]) {
    let nrows = left.0.len();
    for (op, holds) in [
        (Compare::Eq, [false, true, false]),
        (Compare::Ne, [true, false, true]),
        (Compare::Lt, [true, false, false]),
        (Compare::Le, [true, true, false]),
        (Compare::Gt, [false, false, true]),
        (Compare::Ge, [false, true, true]),
    ] {
        let mut kept = selected.to_vec();
        // Every bit of the rest is written, set ones included.
        let mut rest = words(nrows, |_| true);
        decimal::filter(
            op,
            left,
            right,
            &mut RowMask::try_new(nrows, &mut kept).unwrap(),
            &mut RowMask::try_new(nrows, &mut rest).unwrap(),
        )
        .unwrap();
        for row in 0..nrows {
            let (keep, other) = match (left.0[row], right.0[row]) {
                _ if !bit(selected, row) => (false, false),
                (Arg::Null, _) | (_, Arg::Null) => (false, false),
                (Arg::Decimal(l), Arg::Decimal(r)) => {
                    (holds[(l.compare(r) as i8 + 1) as usize], false)
                }
                _ => (false, true),
            };
            assert_eq!(
                (bit(&kept, row), bit(&rest, row)),
                (keep, other),
                "{op:?} {row}"
            );
        }
    }
}

#[test]
fn compute_writes_results_decimals_and_the_rest() {
    let cases = (
        pairs(),
        prop_oneof![Just(None), (0..=MAX_SCALE).prop_map(Some)],
    );
    property(cases, |((left, right, selected), scale)| -> Result<()> {
        compute_writes(&left, &right, &selection(&selected), scale);
        Ok(())
    });
}

fn compute_writes(left: &Column, right: &Column, selected: &[u64], scale: Option<u32>) {
    let nrows = left.0.len();
    for op in [Op::Add, Op::Sub, Op::Mul, Op::Negate, Op::Abs] {
        let mut values = vec![MaybeUninit::uninit(); nrows];
        let mut scales = vec![MaybeUninit::uninit(); nrows];
        let mut present = words(nrows, |_| true);
        let mut decimals = present.clone();
        let mut rest = present.clone();
        let mut results = Results {
            values: &mut values,
            scales: &mut scales,
            non_nulls: RowMask::try_new(nrows, &mut present).unwrap(),
            decimals: RowMask::try_new(nrows, &mut decimals).unwrap(),
            rest: RowMask::try_new(nrows, &mut rest).unwrap(),
        };
        let rows = RowMaskView::try_new(nrows, selected).unwrap();
        decimal::compute(op, left, right, rows, scale, &mut results).unwrap();
        for row in 0..nrows {
            let flags = (bit(&present, row), bit(&rest, row), bit(&decimals, row));
            let l = left.0[row];
            let r = if op.binary() { right.0[row] } else { l };
            if !bit(selected, row) || l == Arg::Null || r == Arg::Null {
                assert_eq!(flags, (false, false, false), "{op:?} {row}");
                continue;
            }
            let expected = match (l, r) {
                (Arg::Decimal(l), Arg::Decimal(r)) => reference(op, l, r),
                _ => None,
            };
            match expected {
                Some(result) => {
                    assert_eq!(
                        flags,
                        (true, false, Some(result.scale()) == scale),
                        "{op:?} {row}"
                    );
                    // SAFETY: the call wrote the result of every computed row.
                    let (value, scale) =
                        unsafe { (values[row].assume_init(), scales[row].assume_init()) };
                    assert_eq!((value, u32::from(scale)), (result.value(), result.scale()));
                }
                None => assert_eq!(flags, (true, true, false), "{op:?} {row}"),
            }
        }
    }
}

#[test]
fn casts_to_integers_leave_out_of_range_values() {
    let args = Column(vec![
        Arg::Decimal(Decimal::new(25, 1).unwrap()),
        Arg::Null,
        Arg::Other,
        Arg::Decimal(Decimal::new(i64::from(i32::MAX) * 10 + 4, 1).unwrap()),
        Arg::Decimal(Decimal::new(i64::from(i32::MAX) * 10 + 5, 1).unwrap()),
        Arg::Decimal(Decimal::new(i64::from(i32::MIN) * 10 - 5, 1).unwrap()),
    ]);
    let selected = [0b11_1111];
    let rows = RowMaskView::try_new(6, &selected).unwrap();
    let mut ints = [MaybeUninit::uninit(); 6];
    let (mut present, mut rest) = ([0], [0]);
    decimal::to_int4(
        &args,
        rows,
        &mut ints,
        &mut RowMask::try_new(6, &mut present).unwrap(),
        &mut RowMask::try_new(6, &mut rest).unwrap(),
    )
    .unwrap();
    assert_eq!((present[0], rest[0]), (0b11_1101, 0b11_0100));
    // SAFETY: rows 0 and 3 are computed.
    assert_eq!(
        unsafe { (ints[0].assume_init(), ints[3].assume_init()) },
        (3, i32::MAX)
    );
    let mut longs = [MaybeUninit::uninit(); 6];
    decimal::to_int8(
        &args,
        rows,
        &mut longs,
        &mut RowMask::try_new(6, &mut present).unwrap(),
        &mut RowMask::try_new(6, &mut rest).unwrap(),
    )
    .unwrap();
    assert_eq!((present[0], rest[0]), (0b11_1101, 0b100));
    // SAFETY: rows 4 and 5 are computed.
    assert_eq!(
        unsafe { (longs[4].assume_init(), longs[5].assume_init()) },
        (i64::from(i32::MAX) + 1, i64::from(i32::MIN) - 1)
    );
}

/// Values that start initialized and are only ever written.
fn values_now(values: &[MaybeUninit<i64>; 6]) -> [i64; 6] {
    // SAFETY: every value is initialized.
    values.map(|value| unsafe { value.assume_init() })
}

#[test]
fn read_takes_one_scale_or_every_scale() {
    let args = Column(vec![
        Arg::Null,
        Arg::Decimal(Decimal::new(15, 1).unwrap()),
        Arg::Other,
        Arg::Decimal(Decimal::new(-7, 2).unwrap()),
        Arg::Decimal(Decimal::new(8, 1).unwrap()),
        Arg::Decimal(Decimal::new(9, 1).unwrap()),
    ]);
    let selected = [0b01_1111];
    let rows = RowMaskView::try_new(6, &selected).unwrap();
    let mut values = [MaybeUninit::new(-1); 6];
    let mut decimals = [0b10_0101];
    let mut scale = None;
    decimal::read(
        &args,
        rows,
        Scales::Uniform(&mut scale),
        &mut values,
        &mut RowMask::try_new(6, &mut decimals).unwrap(),
    )
    .unwrap();
    // Row 5 is outside the selection: its bit and value stay.
    assert_eq!((scale, decimals[0]), (Some(1), 0b11_0010));
    assert_eq!(values_now(&values), [-1, 15, -1, -1, 8, -1]);
    let mut fixed = Some(2);
    decimal::read(
        &args,
        rows,
        Scales::Uniform(&mut fixed),
        &mut values,
        &mut RowMask::try_new(6, &mut decimals).unwrap(),
    )
    .unwrap();
    assert_eq!((fixed, decimals[0]), (Some(2), 0b10_1000));
    assert_eq!(values_now(&values), [-1, 15, -1, -7, 8, -1]);
    let mut scales = [MaybeUninit::new(0); 6];
    decimal::read(
        &args,
        rows,
        Scales::ByRow(&mut scales),
        &mut values,
        &mut RowMask::try_new(6, &mut decimals).unwrap(),
    )
    .unwrap();
    assert_eq!(decimals[0], 0b11_1010);
    // SAFETY: every scale starts initialized.
    assert_eq!(
        scales.map(|scale| unsafe { scale.assume_init() }),
        [0, 1, 0, 2, 1, 0]
    );
}

#[test]
fn different_row_counts_fail_before_writing() {
    let args = Column(vec![Arg::Decimal(Decimal::new(1, 0).unwrap()); 3]);
    let selected = [0b111];
    let rows = RowMaskView::try_new(3, &selected).unwrap();
    let mut values = [MaybeUninit::uninit(); 2];
    let mut decimals = [0];
    let mut scale = None;
    assert!(
        decimal::read(
            &args,
            rows,
            Scales::Uniform(&mut scale),
            &mut values,
            &mut RowMask::try_new(3, &mut decimals).unwrap(),
        )
        .is_err()
    );
    assert_eq!((scale, decimals[0]), (None, 0));
    let mut ints = [MaybeUninit::uninit(); 3];
    let (mut present, mut rest) = ([0], [0]);
    assert!(
        decimal::to_int4(
            &args,
            rows,
            &mut ints,
            &mut RowMask::try_new(3, &mut present).unwrap(),
            &mut RowMask::try_new(2, &mut rest).unwrap(),
        )
        .is_err()
    );
    assert_eq!((present[0], rest[0]), (0, 0));
}

#[test]
fn sum_adds_every_decimal_and_leaves_the_rest() {
    property(pairs(), |(column, _, selected)| -> Result<()> {
        sum_adds(&column, &selection(&selected));
        Ok(())
    });
}

fn sum_adds(column: &Column, selected: &[u64]) {
    let nrows = column.0.len();
    let mut total = Sum::default();
    let mut rest = words(nrows, |_| true);
    // Two calls over halves of the selection, as batches come.
    for half in 0..2 {
        let part: Vec<u64> = selected
            .iter()
            .enumerate()
            .map(|(word, &bits)| if word % 2 == half { bits } else { 0 })
            .collect();
        let mut left = words(nrows, |_| false);
        decimal::sum(
            column,
            RowMaskView::try_new(nrows, &part).unwrap(),
            &mut total,
            &mut RowMask::try_new(nrows, &mut left).unwrap(),
        )
        .unwrap();
        for (word, bits) in left.iter().enumerate() {
            if word % 2 == half {
                rest[word] = *bits;
            }
        }
    }
    // The model adds in the calls' order, even words before odd ones, and
    // leaves to the caller a decimal whose sum, or the sum rescaled to it,
    // would reach the bound.
    let (mut value, mut scale, mut count) = (0_i128, 0_u32, 0_u64);
    let order = (0..nrows)
        .filter(|row| row / 64 % 2 == 0)
        .chain((0..nrows).filter(|row| row / 64 % 2 == 1));
    for row in order {
        let (taken, other) = match column.0[row] {
            _ if !bit(selected, row) => (false, false),
            Arg::Decimal(decimal) => {
                let at = scale.max(decimal.scale());
                let sum = value
                    .checked_mul(10_i128.pow(at - scale))
                    .filter(|rescaled| rescaled.abs() < SUM_BOUND)
                    .map(|rescaled| {
                        rescaled + i128::from(decimal.value()) * 10_i128.pow(at - decimal.scale())
                    })
                    .filter(|sum| sum.abs() < SUM_BOUND);
                match sum {
                    Some(sum) => {
                        (value, scale) = (sum, at);
                        count += 1;
                        (true, false)
                    }
                    None => (false, true),
                }
            }
            Arg::Other => (false, true),
            Arg::Null => (false, false),
        };
        assert_eq!(bit(&rest, row), other && !taken, "{row}");
    }
    assert_eq!(
        (total.value, total.scale, total.count),
        (value, scale, count)
    );
    // A sum already past its bound is refused.
    let mut past = Sum {
        value: SUM_BOUND,
        scale: 0,
        count: 0,
    };
    assert!(
        decimal::sum(
            column,
            RowMaskView::try_new(nrows, selected).unwrap(),
            &mut past,
            &mut RowMask::try_new(nrows, &mut rest).unwrap(),
        )
        .is_err()
    );
}

/// A sum: a value leaning to the bound's edges at a scale the batch reads,
/// and a count.
fn sum() -> impl Strategy<Value = Sum> {
    (bounded_sum(SUM_BOUND), 0..=MAX_READ_SCALE, 0..1_000_000_u64).prop_map(
        |(value, scale, count)| Sum {
            value,
            scale,
            count,
        },
    )
}

/// `value` at scale `from` rescaled to `to`, at least `from`, exactly, or
/// `None` past an i128.
fn rescaled(value: i128, from: u32, to: u32) -> Option<i128> {
    value.checked_mul(10_i128.checked_pow(to - from)?)
}

/// What a sum takes `value` at `scale` into, by exact arithmetic: the sum
/// rescaled to the larger scale and the result, `None` past an i128.
fn exact(sum: &Sum, value: i128, scale: u32) -> (Option<i128>, Option<i128>, u32) {
    let to = sum.scale.max(scale);
    let ours = rescaled(sum.value, sum.scale, to);
    let total = ours.and_then(|ours| rescaled(value, scale, to)?.checked_add(ours));
    (ours, total, to)
}

/// Whether a sum must take a term: its rescaled value and the result both
/// stay below the bound.
fn fits(ours: Option<i128>, total: Option<i128>) -> bool {
    ours.is_some_and(|ours| ours.abs() < SUM_BOUND)
        && total.is_some_and(|total| total.abs() < SUM_BOUND)
}

#[test]
fn a_sum_adds_a_decimal_exactly_or_refuses_it_unchanged() {
    property(
        (sum(), decimal_parts(MAX_READ_SCALE)),
        |(sum, (value, scale))| -> Result<()> {
            let (ours, total, to) = exact(&sum, i128::from(value), scale);
            let mut after = sum;
            let taken = after.add(Decimal::new(value, scale).unwrap());
            ensure!(
                taken == fits(ours, total),
                "taken {taken}: {sum:?} + {value}e-{scale}"
            );
            if taken {
                ensure!(
                    after
                        == Sum {
                            value: total.unwrap(),
                            scale: to,
                            count: sum.count + 1
                        }
                );
            } else {
                ensure!(after == sum, "a refused decimal changed {sum:?}");
            }
            Ok(())
        },
    );
}

#[test]
fn a_sum_adds_another_sum_exactly_or_refuses_it_unchanged() {
    property((sum(), sum()), |(sum, other)| -> Result<()> {
        let (ours, total, to) = exact(&sum, other.value, other.scale);
        let mut after = sum;
        let taken = after.add_many(other.value, other.scale, other.count);
        ensure!(
            taken == fits(ours, total),
            "taken {taken}: {sum:?} + {other:?}"
        );
        if taken {
            ensure!(
                after
                    == Sum {
                        value: total.unwrap(),
                        scale: to,
                        count: sum.count + other.count
                    }
            );
        } else {
            ensure!(after == sum, "a refused sum changed {sum:?}");
        }
        Ok(())
    });
}

/// A decimal of the state's scale is added to its words without decoding
/// them: the words come out as the decoded state's would, taken or not.
#[test]
fn a_state_takes_a_decimal_in_its_words_as_decoded() {
    property(
        (sum(), decimal_parts(MAX_READ_SCALE), any::<[bool; 3]>()),
        |(sum, (value, scale), [nan, positive_infinity, negative_infinity])| -> Result<()> {
            let state = SumState {
                sum,
                nan,
                positive_infinity,
                negative_infinity,
            };
            // The same scale one time in two or so: the fast path.
            for scale in [scale, sum.scale] {
                let term = Term::Decimal(Decimal::new(value, scale).unwrap());
                let mut words = state.to_words();
                let mut decoded = state;
                let taken = SumState::add_to(&mut words, term);
                ensure!(taken == decoded.add(term), "{state:?} + {term:?}");
                ensure!(words == decoded.to_words(), "{state:?} + {term:?}");
            }
            Ok(())
        },
    );
}

/// The bound itself is never reached, whatever the scale and the sign: a
/// sum 10^k short of it refuses a decimal of 10^k and takes 10^k − 1 to
/// the last value below it, at every scale, one at a time and in a sum of
/// many.
#[test]
fn a_sum_stops_one_short_of_its_bound() {
    for scale in 0..=MAX_READ_SCALE {
        for digits in 0..=17 {
            let power = 10_i64.pow(digits);
            for sign in [1_i64, -1] {
                let start = Sum {
                    value: i128::from(sign) * (SUM_BOUND - i128::from(power)),
                    scale,
                    count: 1,
                };
                let mut sum = start;
                assert!(!sum.add(Decimal::new(sign * power, scale).unwrap()));
                assert_eq!(sum, start);
                assert!(!sum.add_many(i128::from(sign * power), scale, 1));
                assert_eq!(sum, start);
                assert!(sum.add(Decimal::new(sign * (power - 1), scale).unwrap()));
                assert_eq!(sum.value, i128::from(sign) * (SUM_BOUND - 1));
                let mut many = start;
                assert!(many.add_many(i128::from(sign * (power - 1)), scale, 3));
                assert_eq!((many.value, many.count), (sum.value, 4));
            }
        }
    }
}

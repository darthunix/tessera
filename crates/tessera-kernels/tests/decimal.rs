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
    self, Arg, Compare, Decimal, DecimalWord, ExtremeState, ExtremeValue, MAX_READ_SCALE,
    MAX_SCALE, Offer, Op, POWERS, Partial, Partials, Results, SUM_BOUND, Scales, Source, Special,
    Sum, SumState, Term, Terms,
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

/// A column's arguments as terms (a value not a decimal is one the
/// caller takes), handed over in bulk or not: the decimals of a word of the
/// scale of its first one, as a numeric column read in place hands them.
struct ArgTerms<'a> {
    column: &'a Column,
    bulk: bool,
}

impl Terms for ArgTerms<'_> {
    fn term(&self, row: usize) -> Term {
        match self.column.0[row] {
            Arg::Null => Term::Null,
            Arg::Decimal(decimal) => Term::Decimal(decimal),
            Arg::Other => Term::Other,
        }
    }

    fn fold_decimals(
        &self,
        index: usize,
        rows: u64,
        mut add: impl FnMut(usize, i64),
    ) -> Option<DecimalWord> {
        if !self.bulk {
            return None;
        }
        let mut scale = None;
        let mut bulk = 0;
        for bit in (0..64).filter(|bit| rows >> bit & 1 == 1) {
            if let Arg::Decimal(decimal) = self.column.0[index * 64 + bit]
                && *scale.get_or_insert(decimal.scale()) == decimal.scale()
            {
                add(bit, decimal.value());
                bulk |= 1 << bit;
            }
        }
        Some(DecimalWord {
            rows: bulk,
            scale: scale.unwrap_or(0),
        })
    }
}

/// [`decimal::extreme`] over a selection, with the column's decimals
/// handed over in bulk and without: the row and decimal found, and the
/// rows left, the same both ways.
fn extreme_of(
    column: &Column,
    rows: &[u64],
    max: bool,
    state: Option<Decimal>,
) -> (Option<(usize, Decimal)>, Vec<u64>) {
    let nrows = column.0.len();
    let mut results = [true, false].map(|bulk| {
        let mut rest = words(nrows, |_| true);
        let found = decimal::extreme(
            &ArgTerms { column, bulk },
            RowMaskView::try_new(nrows, rows).unwrap(),
            max,
            state,
            &mut RowMask::try_new(nrows, &mut rest).unwrap(),
        )
        .unwrap();
        (found, rest)
    });
    assert_eq!(results[0], results[1], "in bulk and row by row");
    std::mem::take(&mut results[0])
}

#[test]
fn extreme_keeps_the_last_of_equal_decimals_and_leaves_the_rest() {
    let state = prop_oneof![
        Just(None),
        decimal_parts(MAX_READ_SCALE).prop_map(|(value, scale)| Decimal::new(value, scale)),
    ];
    property(
        (pairs(), state, any::<bool>()),
        |((column, _, selected), state, max)| -> Result<()> {
            let (got, rest) = extreme_of(&column, &selection(&selected), max, state);
            // The model: values at scale 18, exactly; a decimal replaces
            // the best unless it loses to it.
            let at_18 = |decimal: Decimal| {
                i128::from(decimal.value()) * 10_i128.pow(MAX_READ_SCALE - decimal.scale())
            };
            let mut best = state;
            let mut found = None;
            for (row, (&chosen, &arg)) in selected.iter().zip(&column.0).enumerate() {
                let other = chosen && arg == Arg::Other;
                ensure!(
                    bit(&rest, row) == other,
                    "row {row} left: {}",
                    bit(&rest, row)
                );
                if let (true, Arg::Decimal(decimal)) = (chosen, arg) {
                    let wins = best.is_none_or(|best| {
                        if max {
                            at_18(decimal) >= at_18(best)
                        } else {
                            at_18(decimal) <= at_18(best)
                        }
                    });
                    if wins {
                        (best, found) = (Some(decimal), Some(row));
                    }
                }
            }
            let expected = found.zip(best);
            ensure!(got == expected, "{got:?} for {expected:?}");
            Ok(())
        },
    );
}

#[test]
fn extreme_takes_the_later_of_equal_values_of_other_scales() {
    let one = Decimal::new(1, 0).unwrap();
    let longer = Decimal::new(100, 2).unwrap();
    let column = Column(vec![
        Arg::Decimal(longer),
        Arg::Null,
        Arg::Decimal(one),
        Arg::Other,
    ]);
    for max in [false, true] {
        assert_eq!(
            extreme_of(&column, &[0b101], max, None),
            (Some((2, one)), vec![0])
        );
        // The state is earlier than the batch: an equal row replaces it.
        let state = Some(Decimal::new(10, 1).unwrap());
        assert_eq!(
            extreme_of(&column, &[0b1101], max, state),
            (Some((2, one)), vec![0b1000])
        );
    }
    // Of equal values of one scale, the last row.
    let same = Column(vec![Arg::Decimal(one), Arg::Null, Arg::Decimal(one)]);
    for max in [false, true] {
        assert_eq!(
            extreme_of(&same, &[0b101], max, None),
            (Some((2, one)), vec![0])
        );
    }
    // A state that beats the batch stays; so does one where the rows are
    // NULL.
    let two = Some(Decimal::new(2, 0).unwrap());
    assert_eq!(extreme_of(&column, &[0b101], true, two), (None, vec![0]));
    assert_eq!(extreme_of(&column, &[0b10], false, two), (None, vec![0]));
}

/// A special value of numeric.
fn special() -> impl Strategy<Value = Special> {
    prop_oneof![
        Just(Special::NaN),
        Just(Special::PositiveInfinity),
        Just(Special::NegativeInfinity),
    ]
}

/// A decimal leaning to the digit edges at a scale the batch reads.
fn read_decimal() -> impl Strategy<Value = Decimal> {
    decimal_parts(MAX_READ_SCALE).prop_map(|(value, scale)| Decimal::new(value, scale).unwrap())
}

/// The order `numeric_cmp` gives two values, by their place among the
/// specials and their values at scale 18; `None` for a longer value and
/// another finite one, which only the core orders.
fn numeric_order(left: ExtremeValue, right: ExtremeValue) -> Option<std::cmp::Ordering> {
    let place = |value: ExtremeValue| match value {
        ExtremeValue::Special(Special::NegativeInfinity) => (0, None),
        ExtremeValue::Decimal(decimal) => (
            1,
            Some(i128::from(decimal.value()) * 10_i128.pow(MAX_READ_SCALE - decimal.scale())),
        ),
        ExtremeValue::Empty | ExtremeValue::Numeric => (1, None),
        ExtremeValue::Special(Special::PositiveInfinity) => (2, None),
        ExtremeValue::Special(Special::NaN) => (3, None),
    };
    match (place(left), place(right)) {
        ((1, Some(left)), (1, Some(right))) => Some(left.cmp(&right)),
        ((1, _), (1, _)) => None,
        ((left, _), (right, _)) => Some(left.cmp(&right)),
    }
}

#[test]
fn extreme_states_offer_rows_in_numeric_order() {
    let value = prop_oneof![
        Just(ExtremeValue::Empty),
        read_decimal().prop_map(ExtremeValue::Decimal),
        special().prop_map(ExtremeValue::Special),
        Just(ExtremeValue::Numeric),
    ];
    let term = prop_oneof![
        1 => Just(Term::Null),
        1 => Just(Term::Other),
        2 => special().prop_map(Term::Special),
        6 => read_decimal().prop_map(Term::Decimal),
    ];
    property(
        (value, proptest::collection::vec(term, 0..64), any::<bool>()),
        |(first, terms, max)| -> Result<()> {
            let mut state = ExtremeState {
                value: first,
                pending: false,
            };
            let (mut best, mut pending) = (first, false);
            for term in terms {
                let offer = state.offer(term, max);
                let row = match term {
                    Term::Null => None,
                    Term::Decimal(decimal) => Some(ExtremeValue::Decimal(decimal)),
                    Term::Special(special) => Some(ExtremeValue::Special(special)),
                    Term::Other => Some(ExtremeValue::Numeric),
                };
                // The model: a row after one left, or one only the core
                // orders, is left; else the later of equal values wins.
                let expected = match row {
                    None => Offer::Kept,
                    Some(_) if pending => Offer::Rest,
                    Some(ExtremeValue::Numeric) => Offer::Rest,
                    Some(row) if best == ExtremeValue::Empty => {
                        best = row;
                        Offer::Taken
                    }
                    Some(row) => match numeric_order(row, best) {
                        None => Offer::Rest,
                        Some(order)
                            if order
                                == if max {
                                    std::cmp::Ordering::Less
                                } else {
                                    std::cmp::Ordering::Greater
                                } =>
                        {
                            Offer::Kept
                        }
                        Some(_) => {
                            best = row;
                            Offer::Taken
                        }
                    },
                };
                pending |= expected == Offer::Rest;
                ensure!(
                    offer == expected,
                    "{term:?} to {state:?}: {offer:?}, not {expected:?}"
                );
                ensure!(state.value == best && state.pending == pending, "{state:?}");
                let (value, flags) = state.to_words();
                ensure!(
                    ExtremeState::from_words(value, flags)? == state,
                    "{state:?} round trip"
                );
            }
            Ok(())
        },
    );
}

#[test]
fn extreme_states_refuse_unknown_words() {
    assert_eq!(ExtremeState::from_words(0, 0).unwrap(), ExtremeState::EMPTY);
    // A kind past the six, a flag past bit 11, a decimal of 19 digits.
    assert!(ExtremeState::from_words(0, 6 << 8).is_err());
    assert!(ExtremeState::from_words(0, 1 << 12).is_err());
    assert!(ExtremeState::from_words(10_u64.pow(18), 1 << 8).is_err());
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

/// Partial states by row, as a final aggregation's batch gives them.
#[derive(Clone, Debug)]
struct PartialRows(Vec<Partial>);

impl Partials for PartialRows {
    fn partial(&self, row: usize) -> Result<Partial> {
        Ok(self.0[row])
    }
}

#[test]
fn merged_partial_states_add_exactly_or_go_to_the_rest() {
    let partial = prop_oneof![
        1 => Just(Partial::Null),
        1 => Just(Partial::Other),
        8 => (sum(), any::<u8>()).prop_map(|(sum, flags)| Partial::State(SumState {
            sum,
            nan: flags % 13 == 0,
            positive_infinity: flags % 17 == 0,
            negative_infinity: flags % 19 == 0,
        })),
    ];
    let rows = nrows().prop_flat_map(move |nrows| {
        (
            proptest::collection::vec(partial.clone(), nrows).prop_map(PartialRows),
            flags(nrows),
            sum(),
        )
    });
    property(rows, |(partials, selected, start)| -> Result<()> {
        let nrows = partials.0.len();
        let rows = selection(&selected);
        let mut total = start;
        let mut rest = words(nrows, |_| true);
        decimal::merge_partials(
            &partials,
            RowMaskView::try_new(nrows, &rows).unwrap(),
            &mut total,
            &mut RowMask::try_new(nrows, &mut rest).unwrap(),
        )?;
        // The model: in row order, a state without flags whose sum the
        // total takes below the bound, exactly; any other state left.
        let mut model = start;
        for (row, partial) in partials.0.iter().enumerate() {
            let left = match partial {
                _ if !selected[row] => false,
                Partial::Null => false,
                Partial::Other => true,
                Partial::State(state) => {
                    let (ours, sum, to) = exact(&model, state.sum.value, state.sum.scale);
                    let plain = !state.nan && !state.positive_infinity && !state.negative_infinity;
                    if plain && state.sum.scale <= MAX_READ_SCALE && fits(ours, sum) {
                        model = Sum {
                            value: sum.unwrap(),
                            scale: to,
                            count: model.count + state.sum.count,
                        };
                        false
                    } else {
                        true
                    }
                }
            };
            ensure!(
                bit(&rest, row) == left,
                "row {row}: left {}",
                bit(&rest, row)
            );
        }
        ensure!(total == model, "{total:?} for {model:?}");
        Ok(())
    });
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

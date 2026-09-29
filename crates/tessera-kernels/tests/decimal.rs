//! The batch functions of decimals against a row-by-row model: the rows
//! each keeps, computes, leaves to the caller and reads.

use std::mem::MaybeUninit;

use tessera_core::{RowMask, RowMaskView};
use tessera_kernels::decimal::{
    self, Arg, Compare, Decimal, MAX_READ_SCALE, MAX_SCALE, Op, POWERS, Results, SUM_BOUND, Scales,
    Source, Sum,
};

/// xorshift64*, fixed seed: the same data on every run.
fn random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// A decimal of up to 18 digits at a scale up to `max_scale`.
fn random_decimal(state: &mut u64, max_scale: u32) -> Decimal {
    let digits = (random(state) % 19) as usize;
    let magnitude = (random(state) % POWERS[digits] as u64) as i64;
    let value = if random(state).is_multiple_of(2) {
        magnitude
    } else {
        -magnitude
    };
    Decimal::new(value, (random(state) % u64::from(max_scale + 1)) as u32).unwrap()
}

/// A column of arguments by row.
struct Column(Vec<Arg>);

impl Source for Column {
    fn get(&self, row: usize) -> Arg {
        self.0[row]
    }
}

fn random_column(state: &mut u64, nrows: usize) -> Column {
    Column(
        (0..nrows)
            .map(|_| match random(state) % 10 {
                0 => Arg::Null,
                1 => Arg::Other,
                _ => Arg::Decimal(random_decimal(state, MAX_READ_SCALE)),
            })
            .collect(),
    )
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
    let mut state = 0xDEAD_BEEF_CAFE_F00D;
    let nrows = 1000;
    let left = random_column(&mut state, nrows);
    let right = random_column(&mut state, nrows);
    let selected = words(nrows, |row| row % 7 != 3);
    for (op, holds) in [
        (Compare::Eq, [false, true, false]),
        (Compare::Ne, [true, false, true]),
        (Compare::Lt, [true, false, false]),
        (Compare::Le, [true, true, false]),
        (Compare::Gt, [false, false, true]),
        (Compare::Ge, [false, true, true]),
    ] {
        let mut kept = selected.clone();
        // Every bit of the rest is written, set ones included.
        let mut rest = words(nrows, |_| true);
        decimal::filter(
            op,
            &left,
            &right,
            &mut RowMask::try_new(nrows, &mut kept).unwrap(),
            &mut RowMask::try_new(nrows, &mut rest).unwrap(),
        )
        .unwrap();
        for row in 0..nrows {
            let (keep, other) = match (left.0[row], right.0[row]) {
                _ if !bit(&selected, row) => (false, false),
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
    let mut state = 0x0BAD_C0DE_1234_5678;
    let nrows = 777;
    let left = random_column(&mut state, nrows);
    let right = random_column(&mut state, nrows);
    let selected = words(nrows, |row| row % 5 != 2);
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
        let rows = RowMaskView::try_new(nrows, &selected).unwrap();
        decimal::compute(op, &left, &right, rows, Some(4), &mut results).unwrap();
        for row in 0..nrows {
            let flags = (bit(&present, row), bit(&rest, row), bit(&decimals, row));
            let l = left.0[row];
            let r = if op.binary() { right.0[row] } else { l };
            if !bit(&selected, row) || l == Arg::Null || r == Arg::Null {
                assert_eq!(flags, (false, false, false), "{op:?} {row}");
                continue;
            }
            let expected = match (l, r) {
                (Arg::Decimal(l), Arg::Decimal(r)) => reference(op, l, r),
                _ => None,
            };
            match expected {
                Some(result) => {
                    assert_eq!(flags, (true, false, result.scale() == 4), "{op:?} {row}");
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
    let mut state = 0x5EED_0F5A_5A5A_A5A5;
    let nrows = 900;
    let column = random_column(&mut state, nrows);
    let selected = words(nrows, |row| row % 9 != 4);
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
            &column,
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
    let (mut value, mut scale, mut count) = (0_i128, 0_u32, 0_u64);
    for row in 0..nrows {
        let (taken, other) = match column.0[row] {
            _ if !bit(&selected, row) => (false, false),
            Arg::Decimal(decimal) => {
                let at = scale.max(decimal.scale());
                value = value * 10_i128.pow(at - scale)
                    + i128::from(decimal.value()) * 10_i128.pow(at - decimal.scale());
                scale = at;
                count += 1;
                (true, false)
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
            &column,
            RowMaskView::try_new(nrows, &selected).unwrap(),
            &mut past,
            &mut RowMask::try_new(nrows, &mut rest).unwrap(),
        )
        .is_err()
    );
}

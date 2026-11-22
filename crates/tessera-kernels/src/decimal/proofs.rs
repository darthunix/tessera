//! Kani proofs of the arithmetic of integer sums: a term or a word's bulk
//! sum of an int8 column, at scale 0, into a sum below the bound, at each
//! scale a state may have. For every such state and term the sum never
//! panics, takes the term exactly when the exact sum, the term rescaled to
//! the state's scale, fits an `i128` and is below the bound, `i128::MIN`
//! among the sums refused, and a refused term leaves the state as it was;
//! a state's words take an integer in place as the decoded state does.
//! The scale is a literal in each harness, so that the power of ten is a
//! constant: Kani does not finish a product of two unknowns.

use super::{POWERS, SUM_BOUND, Sum, SumState, Term, below_bound};

/// The most a word's bulk sum of an int8 column reaches in magnitude: 64
/// values of 2^63.
const BULK_MOST: u128 = 64 << 63;

/// A state the entry points let in, at `scale`: its count an int64's, as
/// C keeps it (`TessDecimalSum.count`), so that the rows a call adds,
/// which the sum counts unchecked, never reach 2^64.
fn state(scale: u32) -> Sum {
    let value: i128 = kani::any();
    kani::assume(below_bound(value));
    let count: u64 = kani::any();
    kani::assume(count <= i64::MAX as u64);
    Sum {
        value,
        scale,
        count,
    }
}

/// The exact sum of `sum` and `term` at scale 0, the term rescaled to the
/// sum's scale, or `None` past an `i128`.
fn exact(sum: Sum, term: i128) -> Option<i128> {
    let factor = i128::from(POWERS[sum.scale as usize]);
    term.checked_mul(factor)?.checked_add(sum.value)
}

/// [`Sum::add_many`] of a bulk sum of `rows` values at scale 0 into a state
/// at `scale`.
fn bulk_into(scale: u32) {
    let rows: u64 = kani::any();
    kani::assume((1..=64).contains(&rows));
    let start = state(scale);
    let term: i128 = kani::any();
    kani::assume(term.unsigned_abs() <= BULK_MOST);
    let mut sum = start;
    let taken = sum.add_many(term, 0, rows);
    let exact = exact(start, term);
    assert!(taken == exact.is_some_and(below_bound));
    if taken {
        assert!(Some(sum.value) == exact && sum.scale == scale);
        assert!(sum.count == start.count + rows);
    } else {
        assert!(sum == start);
    }
    kani::cover!(taken);
    kani::cover!(!taken && exact.is_some());
}

macro_rules! bulk_proofs {
    ($($name:ident: $scale:literal;)*) => {
        $(
            #[kani::proof]
            fn $name() {
                bulk_into($scale);
            }
        )*
    };
}

bulk_proofs! {
    a_bulk_sum_into_scale_0: 0;
    a_bulk_sum_into_scale_1: 1;
    a_bulk_sum_into_scale_2: 2;
    a_bulk_sum_into_scale_3: 3;
    a_bulk_sum_into_scale_4: 4;
    a_bulk_sum_into_scale_5: 5;
    a_bulk_sum_into_scale_6: 6;
    a_bulk_sum_into_scale_7: 7;
    a_bulk_sum_into_scale_8: 8;
    a_bulk_sum_into_scale_9: 9;
    a_bulk_sum_into_scale_10: 10;
    a_bulk_sum_into_scale_11: 11;
    a_bulk_sum_into_scale_12: 12;
    a_bulk_sum_into_scale_13: 13;
    a_bulk_sum_into_scale_14: 14;
    a_bulk_sum_into_scale_15: 15;
    a_bulk_sum_into_scale_16: 16;
    a_bulk_sum_into_scale_17: 17;
    a_bulk_sum_into_scale_18: 18;
}

/// Into a state of scale 18, the one scale where a bulk sum can land
/// exactly on `i128::MIN`, both checked operations allow that sum, and the
/// bound refuses it.
#[kani::proof]
fn a_bulk_sum_landing_on_i128_min_is_refused() {
    let start = state(18);
    let term: i128 = kani::any();
    kani::assume(term.unsigned_abs() <= BULK_MOST);
    kani::assume(exact(start, term) == Some(i128::MIN));
    let mut sum = start;
    assert!(!sum.add_many(term, 0, 1));
    assert!(sum == start);
    kani::cover!(sum == start);
}

/// [`SumState::add_to`] of an integer term into a state's words at `scale`,
/// as the decoded state takes it.
fn integer_in_place(scale: u32) {
    let start = SumState {
        sum: state(scale),
        nan: kani::any(),
        positive_infinity: kani::any(),
        negative_infinity: kani::any(),
    };
    let value: i64 = kani::any();
    let term = Term::Integer(value);
    let mut words = start.to_words();
    let mut decoded = start;
    let taken = SumState::add_to(&mut words, term);
    assert!(taken == decoded.add(term));
    assert!(words == decoded.to_words());
    let exact = exact(start.sum, i128::from(value));
    assert!(taken == exact.is_some_and(below_bound));
    let after = SumState::from_words(words);
    if taken {
        assert!(Some(after.sum.value) == exact && after.sum.scale == scale);
        assert!(after.sum.count == start.sum.count + 1);
    } else {
        assert!(after == start);
    }
    kani::cover!(taken);
    kani::cover!(!taken);
}

/// In place at scale 0, the fast path of the words: an integer taken as
/// the decoded state takes it.
#[kani::proof]
fn an_integer_in_place_at_scale_0() {
    integer_in_place(0);
}

/// In place at another scale, the words decoded and the term taken by
/// [`Sum::add_many`], whose arithmetic the proofs above cover at every
/// scale: one scale shows the path.
#[kani::proof]
fn an_integer_in_place_at_scale_1() {
    integer_in_place(1);
}

/// The bound by `unsigned_abs`: every `i128`, `i128::MIN` among them, is
/// below it exactly when its magnitude is.
#[kani::proof]
fn the_bound_measures_every_i128() {
    let value: i128 = kani::any();
    let magnitude = if value < 0 {
        (value as u128).wrapping_neg()
    } else {
        value as u128
    };
    assert!(below_bound(value) == (magnitude < SUM_BOUND as u128));
    kani::cover!(value == i128::MIN);
}

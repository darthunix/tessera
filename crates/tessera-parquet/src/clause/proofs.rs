//! The proofs by Kani of `IN`, for every list of up to two values and
//! every stored value, over plans of integers, days and microseconds:
//! `cargo kani -p tessera-parquet` runs them. The exactness of each
//! value's test of equality is `datum-arrow`'s proof; these show that the
//! bounds of `IN` are the union of those tests. They ask of a test only
//! that an equality gives one bound or none, which `datum-arrow` proves
//! for every unit, seconds and milliseconds among them, whose division
//! Kani does not finish here. The
//! sort, the removal of repeats and the binary search over the bounds
//! are the standard library's, whose sort takes Kani past the limit of a
//! harness; the property tests compare the whole, over arrays, with
//! PostgreSQL's answer.

use datum_arrow::arrow_schema::DataType;
use datum_arrow::{Comparison, PgType, ReadOptions, ReadPlan, Unit};

use super::equalities;

/// The bounds of `IN` of two values, any of them `NULL` or repeated,
/// hold a stored value exactly when one value's test of equality passes
/// it. A `NULL` adds no bound, so two values stand for every shorter
/// list. Gives the values, and whether one that is not `NULL` was left
/// out.
fn check_in(arrow: &DataType, pg: PgType, unit: Option<Unit>) -> ([Option<i64>; 2], bool) {
    let options = ReadOptions {
        unit,
        text: None,
        server_utf8: true,
    };
    let Ok(plan) = ReadPlan::new(arrow, pg, options) else {
        panic!("the plans are made");
    };
    let values: [Option<i64>; 2] = kani::any();
    let Ok(bounds) = equalities(&plan, &values) else {
        panic!("a plan of integers or times takes bounds");
    };
    let stored: i64 = kani::any();
    let any = values.iter().any(|value| {
        plan.test(Comparison::Eq, *value)
            .is_ok_and(|test| test.passes(stored))
    });
    assert!(bounds.contains(&stored) == any);
    kani::cover!(any, "a value found");
    kani::cover!(!any && bounds.len() == 2, "two bounds, none found");
    let left_out = bounds.len() < values.iter().filter(|value| value.is_some()).count();
    (values, left_out)
}

#[kani::proof]
#[kani::unwind(5)]
fn in_of_integers_is_any_equality() {
    check_in(&DataType::Int64, PgType::Int8, None);
}

#[kani::proof]
#[kani::unwind(5)]
fn in_of_days_is_any_equality() {
    let (values, left_out) = check_in(&DataType::Int32, PgType::Date, Some(Unit::Days));
    kani::cover!(
        left_out && values.contains(&Some(i64::from(i32::MAX))),
        "infinity left out"
    );
}

#[kani::proof]
#[kani::unwind(5)]
fn in_of_microseconds_is_any_equality() {
    let (values, left_out) = check_in(
        &DataType::Int64,
        PgType::Timestamptz,
        Some(Unit::Microseconds),
    );
    let infinite = |value: &Option<i64>| matches!(value, Some(i64::MAX | i64::MIN));
    kani::cover!(
        left_out && !values.iter().any(infinite),
        "a finite value out of reach left out"
    );
    kani::cover!(
        left_out && values.contains(&Some(i64::MIN)),
        "-infinity left out"
    );
}

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
//!
//! The proof of a group ruled out by its statistics shows that such a
//! group holds no row that may pass, for every kind of clause and every
//! statistic, the emptiness of the least and greatest strings among
//! them; the dictionaries are shown by the unit and property tests.

use datum_arrow::arrow_schema::DataType;
use datum_arrow::{Comparison, PgType, ReadOptions, ReadPlan, Unit};

use datum_arrow::Test;

use super::{ClauseTest, Kind, equalities, found};
use crate::stats::{Bounds, Empties, GroupStats};

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

fn any_comparison() -> Comparison {
    match kani::any::<u8>() % 6 {
        0 => Comparison::Eq,
        1 => Comparison::Ne,
        2 => Comparison::Lt,
        3 => Comparison::Le,
        4 => Comparison::Gt,
        _ => Comparison::Ge,
    }
}

/// A group the statistics rule out holds no row that passes, for every
/// kind of clause, every count of rows, of them `NULL` and of the count
/// the footer gives or not, and every least and greatest value: `IS
/// NULL` ruled out holds no `NULL` row, `IS NOT NULL` ruled out only
/// `NULL` rows; a comparison passes no value from the least to the
/// greatest; and `IN` of up to three bounds is ruled out exactly where a
/// range the right way round holds none of them.
#[kani::proof]
#[kani::unwind(5)]
fn a_group_ruled_out_holds_no_passing_row() {
    let list: [i64; 3] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= list.len());
    let list = &list[..len];
    kani::assume(list.windows(2).all(|pair| pair[0] < pair[1]));
    let kind = match kani::any::<u8>() % 7 {
        0 => Kind::IsNull,
        1 => Kind::IsNotNull,
        2 => Kind::Compare(match kani::any::<u8>() % 3 {
            0 => Test::Every,
            1 => Test::Nothing,
            _ => Test::Compare {
                op: any_comparison(),
                bound: kani::any(),
            },
        }),
        3 => Kind::In(list.to_vec()),
        4 => Kind::Bytes {
            equal: kani::any(),
            value: if kani::any() {
                Vec::new()
            } else {
                b"x".to_vec()
            },
        },
        5 => Kind::Like {
            pattern: b"%".to_vec(),
            negated: kani::any(),
        },
        _ => Kind::NoRow,
    };
    let test = ClauseTest::of(kind);
    let rows: u64 = kani::any();
    let null_rows: u64 = kani::any();
    kani::assume(null_rows <= rows);
    let stats = GroupStats {
        nulls: kani::any::<bool>().then_some(null_rows),
        bounds: kani::any::<bool>().then(|| Bounds {
            least: kani::any(),
            greatest: kani::any(),
        }),
        empties: kani::any::<bool>().then(|| Empties {
            least_empty: kani::any(),
            greatest_empty: kani::any(),
        }),
        dictionary_only: kani::any(),
    };
    let ruled_out = test.rules_out(&stats, rows);
    // A string of the chunk, not NULL, as its least and greatest values
    // allow: empty where the greatest is, not where the least is not.
    // Flags that contradict each other, a least value not empty and a
    // greatest one empty, allow any row: the footer is damaged.
    let row_empty: bool = kani::any();
    kani::assume(stats.empties.is_none_or(|e| {
        (!e.least_empty && e.greatest_empty)
            || ((e.least_empty || !row_empty) && (!e.greatest_empty || row_empty))
    }));
    let value: i64 = kani::any();
    let within = stats
        .bounds
        .is_some_and(|b| b.least <= value && value <= b.greatest);
    match &test.kind {
        Kind::IsNull => assert!(!ruled_out || null_rows == 0),
        Kind::IsNotNull => assert!(!ruled_out || null_rows == rows),
        Kind::Compare(compare) => assert!(!(ruled_out && within && compare.passes(value))),
        Kind::In(bounds) => {
            let none = stats.bounds.is_some_and(|b| {
                b.least <= b.greatest && !bounds.iter().any(|&x| b.least <= x && x <= b.greatest)
            });
            assert!(ruled_out == none);
            assert!(!(ruled_out && within && found(bounds, value)));
        }
        // Of the strings only the emptiness decides, for `= ''` and `<>
        // ''`: a group ruled out holds no row that passes.
        Kind::Bytes { equal, value } if value.is_empty() => {
            assert!(!(ruled_out && row_empty == *equal));
        }
        Kind::Bytes { .. } | Kind::Like { .. } => assert!(!ruled_out),
        Kind::NoRow => assert!(ruled_out),
    }
    kani::cover!(
        ruled_out && matches!(&test.kind, Kind::Bytes { equal: true, .. }),
        "= '' ruled out"
    );
    kani::cover!(
        ruled_out && matches!(&test.kind, Kind::Bytes { equal: false, .. }),
        "<> '' ruled out"
    );
    kani::cover!(
        stats
            .empties
            .is_some_and(|e| !e.least_empty && e.greatest_empty)
            && matches!(&test.kind, Kind::Bytes { .. })
            && !ruled_out,
        "contradicting flags rule nothing out"
    );
    kani::cover!(
        ruled_out && matches!(test.kind, Kind::Compare(_)),
        "a comparison ruled out"
    );
    kani::cover!(
        ruled_out && len == 3 && matches!(test.kind, Kind::In(_)),
        "IN ruled out"
    );
    kani::cover!(!ruled_out && within, "a group that may pass");
}

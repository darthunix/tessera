use std::path::Path;

use arrow_array::{
    Int8Array, Int16Array, Int32Array, Int64Array, UInt8Array, UInt16Array, UInt32Array,
};
use datum_arrow::{Unit, oid};
use proptest::prelude::*;

use super::*;
use crate::column::{ColumnRequest, map_column};
use crate::file::ParquetFile;
use crate::testing::{self, Column, Values as Written};

fn request(name: &str, type_oid: u32) -> ColumnRequest<'_> {
    ColumnRequest {
        name,
        type_oid,
        typmod: -1,
        unit: None,
        utf8: None,
        server_utf8: true,
    }
}

/// A file of one `INT32` column `a` and one `INT64` column `t`.
fn file(dir: &Path) -> ParquetFile {
    let path = dir.join("clauses.parquet");
    testing::write(
        &path,
        &[
            Column::new("a", Written::Int32(vec![Some(1), None, Some(3)])),
            Column::new("t", Written::Int64(vec![Some(1), None, Some(3)])),
        ],
        3,
        false,
    )
    .unwrap();
    ParquetFile::open(&path).unwrap()
}

fn bits(mask: &[u64], len: usize) -> Vec<bool> {
    (0..len)
        .map(|i| mask[i / 64] >> (i % 64) & 1 == 1)
        .collect()
}

fn compare(op: Comparison, value: i64, constant: i64) -> bool {
    match op {
        Comparison::Eq => value == constant,
        Comparison::Ne => value != constant,
        Comparison::Lt => value < constant,
        Comparison::Le => value <= constant,
        Comparison::Gt => value > constant,
        Comparison::Ge => value >= constant,
    }
}

fn comparison() -> impl Strategy<Value = Comparison> {
    prop_oneof![
        Just(Comparison::Eq),
        Just(Comparison::Ne),
        Just(Comparison::Lt),
        Just(Comparison::Le),
        Just(Comparison::Gt),
        Just(Comparison::Ge),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// A comparison, `IN` and the tests of `NULL` on an `integer` column
    /// over `INT32` values and on a `bigint` one, each slot's bit the
    /// clause's answer as PostgreSQL gives it, `NULL` passing only `IS
    /// NULL`, the bits past the slots clear.
    #[test]
    fn clauses_on_integers(
        values in prop::collection::vec(prop::option::of(-50i32..50), 0..200),
        op in comparison(),
        constant in -60i64..60,
        list in prop::collection::vec(prop::option::of(-60i64..60), 0..6),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let file = file(dir.path());
        let len = values.len();
        let words = mask_words(len) + 1;
        let int4 = map_column(&file, &request("a", oid::INT4)).unwrap();
        let int8 = map_column(&file, &request("t", oid::INT8)).unwrap();
        let narrow = Int32Array::from(values.clone());
        let wide = Int64Array::from(values.iter().map(|v| v.map(i64::from)).collect::<Vec<_>>());
        for (column, array) in [(&int4, &narrow as &dyn Array), (&int8, &wide as &dyn Array)] {
            let mut passed = vec![u64::MAX; words];
            ClauseTest::new(column, ClauseKind::Compare(op), &[Some(constant)])
                .unwrap()
                .apply(array, &mut passed)
                .unwrap();
            let expected: Vec<bool> = values
                .iter()
                .map(|v| v.is_some_and(|v| compare(op, i64::from(v), constant)))
                .collect();
            prop_assert_eq!(bits(&passed, len), expected);
            prop_assert_eq!(passed[words - 1], 0);

            ClauseTest::new(column, ClauseKind::In, &list)
                .unwrap()
                .apply(array, &mut passed)
                .unwrap();
            let expected: Vec<bool> = values
                .iter()
                .map(|v| v.is_some_and(|v| list.contains(&Some(i64::from(v)))))
                .collect();
            prop_assert_eq!(bits(&passed, len), expected);

            for (kind, null) in [(ClauseKind::IsNull, true), (ClauseKind::IsNotNull, false)] {
                ClauseTest::new(column, kind, &[]).unwrap().apply(array, &mut passed).unwrap();
                let expected: Vec<bool> = values.iter().map(|v| v.is_none() == null).collect();
                prop_assert_eq!(bits(&passed, len), expected);
                prop_assert_eq!(passed[words - 1], 0);
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// `IN` and the tests of `NULL` over every integer array a reader
    /// gives, and over a slice of one, as the forward reader gives a
    /// window from what it read past the last: each slot's bit the
    /// clause's answer.
    #[test]
    fn in_and_nulls_over_every_array(
        values in prop::collection::vec(prop::option::of(0u8..100), 0..150),
        list in prop::collection::vec(prop::option::of(-5i64..105), 0..6),
        cut in 0usize..70,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let file = file(dir.path());
        let int4 = map_column(&file, &request("a", oid::INT4)).unwrap();
        let cut = cut.min(values.len());
        let kept = &values[cut..];
        let widened = |v: &Option<u8>| v.map(i64::from);
        let whole32 = Int32Array::from(values.iter().map(|v| v.map(i32::from)).collect::<Vec<_>>());
        let arrays: Vec<Box<dyn Array>> = vec![
            Box::new(Int8Array::from(kept.iter().map(|v| v.map(|x| x as i8)).collect::<Vec<_>>())),
            Box::new(Int16Array::from(kept.iter().map(|v| v.map(i16::from)).collect::<Vec<_>>())),
            Box::new(UInt8Array::from(kept.to_vec())),
            Box::new(UInt16Array::from(kept.iter().map(|v| v.map(u16::from)).collect::<Vec<_>>())),
            Box::new(UInt32Array::from(kept.iter().map(|v| v.map(u32::from)).collect::<Vec<_>>())),
            Box::new(whole32.slice(cut, values.len() - cut)),
        ];
        let len = kept.len();
        let words = mask_words(len) + 1;
        for array in &arrays {
            let mut passed = vec![u64::MAX; words];
            ClauseTest::new(&int4, ClauseKind::In, &list)
                .unwrap()
                .apply(array.as_ref(), &mut passed)
                .unwrap();
            let expected: Vec<bool> = kept
                .iter()
                .map(|v| widened(v).is_some_and(|v| list.contains(&Some(v))))
                .collect();
            prop_assert_eq!(bits(&passed, len), expected);
            prop_assert_eq!(passed[words - 1], 0);
            ClauseTest::new(&int4, ClauseKind::IsNull, &[])
                .unwrap()
                .apply(array.as_ref(), &mut passed)
                .unwrap();
            let expected: Vec<bool> = kept.iter().map(Option::is_none).collect();
            prop_assert_eq!(bits(&passed, len), expected);
        }
    }
}

/// Every pair of types, strategy, negator and order of a comparison: a
/// clause is checked exactly for a pair of one unit and a comparison of
/// the B-tree family or `<>`, and the comparison of the column with the
/// side it gives holds exactly where the operator does.
#[test]
fn comparisons_of_every_operator() {
    const TYPES: [u32; 10] = [
        oid::INT2,
        oid::INT4,
        oid::INT8,
        oid::DATE,
        oid::TIMESTAMP,
        oid::TIMESTAMPTZ,
        oid::FLOAT8,
        oid::TEXT,
        oid::BOOL,
        0,
    ];
    let integers = [oid::INT2, oid::INT4, oid::INT8];
    let times = [oid::DATE, oid::TIMESTAMP, oid::TIMESTAMPTZ];
    // The operator of each strategy of a B-tree family, `left op right`.
    let operator = |strategy: i32, left: i64, right: i64| match strategy {
        1 => left < right,
        2 => left <= right,
        3 => left == right,
        4 => left >= right,
        5 => left > right,
        _ => unreachable!(),
    };
    let mut checked = 0;
    for column in TYPES {
        for side in TYPES {
            for strategy in -1..=6 {
                for negator in -1..=6 {
                    for commuted in [false, true] {
                        let one_unit = (integers.contains(&column) && integers.contains(&side))
                            || (times.contains(&column) && side == column);
                        let known = (1..=5).contains(&strategy) || (strategy == 0 && negator == 3);
                        let got = super::comparison(column, side, strategy, negator, commuted);
                        let case = (column, side, strategy, negator, commuted);
                        assert_eq!(got.is_some(), one_unit && known, "{case:?}");
                        let Some(op) = got else { continue };
                        checked += 1;
                        for x in -2..=2 {
                            for c in -2..=2 {
                                // The column's value x, the side's c.
                                let (left, right) = if commuted { (c, x) } else { (x, c) };
                                let holds = if strategy == 0 {
                                    !operator(3, left, right)
                                } else {
                                    operator(strategy, left, right)
                                };
                                assert_eq!(compare(op, x, c), holds, "{case:?} on {x} and {c}");
                            }
                        }
                    }
                }
            }
        }
    }
    // Nine pairs of integers and three of times, five comparisons and <>
    // in both orders, <> under every negator's strategy but =.
    assert_eq!(checked, 12 * (5 * 8 + 1) * 2);
}

#[test]
fn a_comparison_with_null_passes_no_row() {
    let dir = tempfile::tempdir().unwrap();
    let file = file(dir.path());
    let int4 = map_column(&file, &request("a", oid::INT4)).unwrap();
    let array = Int32Array::from(vec![Some(1), Some(2)]);
    let mut passed = vec![u64::MAX];
    for op in [Comparison::Eq, Comparison::Ne, Comparison::Lt] {
        ClauseTest::new(&int4, ClauseKind::Compare(op), &[None])
            .unwrap()
            .apply(&array, &mut passed)
            .unwrap();
        assert_eq!(passed, [0]);
    }
    ClauseTest::new(&int4, ClauseKind::In, &[None, None])
        .unwrap()
        .apply(&array, &mut passed)
        .unwrap();
    assert_eq!(passed, [0]);
}

#[test]
fn times_are_compared_in_the_files_unit() {
    let dir = tempfile::tempdir().unwrap();
    let file = file(dir.path());
    let seconds = map_column(
        &file,
        &ColumnRequest {
            unit: Some(Unit::Seconds),
            ..request("t", oid::TIMESTAMP)
        },
    )
    .unwrap();
    // 2000-01-01 00:00:00 and one second later, in seconds since 1970;
    // the constants are microseconds since 2000-01-01.
    let epoch = 946_684_800i64;
    let array = Int64Array::from(vec![Some(epoch), Some(epoch + 1), None]);
    let mut passed = vec![0u64];
    let mut run = |kind, values: &[Option<i64>]| {
        ClauseTest::new(&seconds, kind, values)
            .unwrap()
            .apply(&array, &mut passed)
            .unwrap();
        passed[0]
    };
    assert_eq!(
        run(ClauseKind::Compare(Comparison::Eq), &[Some(1_000_000)]),
        0b010
    );
    // Half a second: no stored second equals it, every one differs.
    assert_eq!(
        run(ClauseKind::Compare(Comparison::Eq), &[Some(500_000)]),
        0
    );
    assert_eq!(
        run(ClauseKind::Compare(Comparison::Ne), &[Some(500_000)]),
        0b011
    );
    assert_eq!(
        run(ClauseKind::Compare(Comparison::Lt), &[Some(500_000)]),
        0b001
    );
    assert_eq!(
        run(ClauseKind::Compare(Comparison::Ge), &[Some(500_000)]),
        0b010
    );
    assert_eq!(run(ClauseKind::In, &[Some(500_000), Some(0), None]), 0b001);
    // Infinity is above every stored time.
    assert_eq!(
        run(ClauseKind::Compare(Comparison::Lt), &[Some(i64::MAX)]),
        0b011
    );
}

#[test]
fn broken_calls_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let file = file(dir.path());
    let int4 = map_column(&file, &request("a", oid::INT4)).unwrap();
    assert!(ClauseTest::new(&int4, ClauseKind::Compare(Comparison::Eq), &[]).is_err());
    let array = Int32Array::from(vec![Some(1); 65]);
    let test = ClauseTest::new(&int4, ClauseKind::IsNotNull, &[]).unwrap();
    assert_eq!(
        test.apply(&array, &mut [0u64]).unwrap_err().sqlstate(),
        "XX000"
    );
}

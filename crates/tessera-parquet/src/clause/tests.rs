use std::path::Path;

use arrow_array::{
    Int8Array, Int16Array, Int32Array, Int64Array, LargeBinaryArray, UInt8Array, UInt16Array,
    UInt32Array,
};
use datum_arrow::{Unit, oid};
use proptest::prelude::*;

use super::*;
use crate::column::{ColumnRequest, map_column};
use crate::stats::{Bounds, Empties};
use crate::testing::{self, Column, Options, Rewrite, Values as Written};

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

fn stats(nulls: Option<u64>, bounds: Option<(i64, i64)>) -> GroupStats {
    GroupStats {
        nulls,
        bounds: bounds.map(|(least, greatest)| Bounds { least, greatest }),
        ..GroupStats::default()
    }
}

/// The statistics of a chunk rule a group out by each kind of clause, and
/// do not where they leave a row that may pass.
#[test]
fn statistics_rule_groups_out() {
    let of = ClauseTest::of;
    let less = of(Kind::Compare(Test::Compare {
        op: Comparison::Lt,
        bound: 5,
    }));
    assert!(less.rules_out(&stats(None, Some((5, 9))), 10));
    assert!(!less.rules_out(&stats(None, Some((4, 9))), 10));
    assert!(!less.rules_out(&stats(Some(0), None), 10));
    // A range turned the wrong way rules nothing out.
    assert!(!less.rules_out(&stats(None, Some((9, 5))), 10));
    let listed = of(Kind::In(vec![2, 7]));
    assert!(listed.rules_out(&stats(None, Some((3, 6))), 10));
    assert!(listed.rules_out(&stats(None, Some((8, 9))), 10));
    assert!(!listed.rules_out(&stats(None, Some((3, 7))), 10));
    assert!(!listed.rules_out(&stats(None, Some((2, 2))), 10));
    assert!(!listed.rules_out(&stats(None, Some((6, 3))), 10));
    assert!(!listed.rules_out(&stats(None, None), 10));
    assert!(of(Kind::In(vec![])).rules_out(&stats(None, Some((0, 0))), 10));
    let null = of(Kind::IsNull);
    assert!(null.rules_out(&stats(Some(0), None), 10));
    assert!(!null.rules_out(&stats(Some(1), Some((0, 1))), 10));
    assert!(!null.rules_out(&stats(None, Some((0, 1))), 10));
    let not_null = of(Kind::IsNotNull);
    assert!(not_null.rules_out(&stats(Some(10), None), 10));
    assert!(!not_null.rules_out(&stats(Some(9), None), 10));
    assert!(!not_null.rules_out(&stats(None, None), 10));
}

/// A dictionary rules a group out where no value of it passes, for a
/// comparison and `IN`; never for the tests of `NULL` or bytes.
#[test]
fn dictionaries_rule_groups_out() {
    let of = ClauseTest::of;
    let values = Dictionary::Integers(vec![1, 3]);
    let equal = |bound| {
        of(Kind::Compare(Test::Compare {
            op: Comparison::Eq,
            bound,
        }))
    };
    assert!(equal(2).rules_out_by(&values));
    assert!(!equal(3).rules_out_by(&values));
    assert!(of(Kind::In(vec![0, 2, 4])).rules_out_by(&values));
    assert!(!of(Kind::In(vec![0, 1])).rules_out_by(&values));
    assert!(!of(Kind::IsNull).rules_out_by(&values));
    assert!(!of(Kind::IsNotNull).rules_out_by(&Dictionary::Integers(vec![])));
    assert!(!equal(2).rules_out_by(&Dictionary::Bytes(vec![b"x".to_vec()])));
}

/// A file's groups ruled out through its footer: by the statistics, by
/// the dictionary where every data page is encoded through it, and not by
/// a dictionary that fell back to plain pages.
#[test]
fn groups_of_a_file_ruled_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.parquet");
    // Two groups: 1 and 3 a row, then 5 and 6 with a NULL.
    let values = vec![
        Some(1),
        Some(3),
        Some(1),
        Some(3),
        Some(5),
        None,
        Some(6),
        Some(5),
    ];
    let write = |options| {
        testing::write_with(
            &path,
            &[Column::new("a", Written::Int32(values.clone()))],
            options,
        )
        .unwrap();
        let file = ParquetFile::open(&path).unwrap();
        let a = map_column(&file, &request("a", oid::INT4)).unwrap();
        (file, a)
    };
    let (file, a) = write(Options::new(4, true));
    let test = |kind, values: &[Option<i64>]| ClauseTest::new(&a, kind, values).unwrap();
    let out = |test: &ClauseTest, group, dictionary| {
        test.rules_out_group(&file, &a, group, dictionary).unwrap()
    };
    let above = test(ClauseKind::Compare(Comparison::Gt), &[Some(4)]);
    assert!(out(&above, 0, false));
    assert!(!out(&above, 1, false));
    let null = test(ClauseKind::IsNull, &[]);
    assert!(out(&null, 0, false));
    assert!(!out(&null, 1, false));
    assert!(!out(&null, 0, true));
    // 2 lies between the least and the greatest, in no row.
    let two = test(ClauseKind::In, &[Some(2)]);
    assert!(!out(&two, 0, false));
    assert!(out(&two, 0, true));
    // 5 and 6 rule it out alone.
    assert!(out(&two, 1, false));
    assert!(!out(&two, 2, false));
    // Plain pages after the dictionary grew too large: not tried.
    let options = Options {
        page_rows: Some(1),
        dictionary_bytes: Some(4),
        ..Options::new(4, true)
    };
    let (file, a) = write(options);
    let two = ClauseTest::new(&a, ClauseKind::In, &[Some(2)]).unwrap();
    assert!(!two.rules_out_group(&file, &a, 0, true).unwrap());
}

/// A clause's answer for a row, as PostgreSQL gives it.
type Passes<'a> = Box<dyn Fn(Option<i32>) -> bool + 'a>;

/// A footer whose group gives fewer rows than its chunk holds values
/// rules nothing out: the row the reader reads is not lost to a count of
/// `NULL` values that matches the group's rows.
#[test]
fn a_footer_of_counts_that_disagree_rules_nothing_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("counts.parquet");
    let column = Column::new("a", Written::Int32(vec![Some(7), None]));
    testing::write(&path, &[column], 2, false).unwrap();
    let rewrite = Rewrite {
        first_group_rows: Some(1),
        ..Rewrite::default()
    };
    testing::rewrite_footer(&path, rewrite).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let a = map_column(&file, &request("a", oid::INT4)).unwrap();
    assert_eq!(file.group_rows(0), Some(1));
    assert_eq!(group_stats(&file, &a, 0), GroupStats::default());
    for kind in [ClauseKind::IsNull, ClauseKind::IsNotNull] {
        let test = ClauseTest::new(&a, kind, &[]).unwrap();
        assert!(!test.rules_out_group(&file, &a, 0, false).unwrap());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Every group the footer rules out, by its statistics or by its
    /// dictionary, holds no row that passes: over files of random values,
    /// `NULL` among them, signed and unsigned, of random groups and pages,
    /// dictionaries and their fallback to plain pages, with statistics and
    /// without, under footers of the old rule and without counts of `NULL`
    /// values or statistics of encodings.
    #[test]
    fn groups_ruled_out_hold_no_passing_row(
        values in prop::collection::vec(prop::option::of(-20i32..20), 1..120),
        group_rows in 1usize..40,
        dictionary in any::<bool>(),
        statistics in any::<bool>(),
        small_dictionary in any::<bool>(),
        rewrite in (any::<bool>(), any::<bool>(), any::<bool>()),
        unsigned in any::<bool>(),
        kind in 0u8..4,
        op in comparison(),
        constant in -25i64..25,
        list in prop::collection::vec(prop::option::of(-25i64..25), 0..4),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("groups.parquet");
        let mut column = Column::new("a", Written::Int32(values.clone()));
        if unsigned {
            column = column.logical(parquet::basic::LogicalType::Integer {
                bit_width: 32,
                is_signed: false,
            });
        }
        let options = Options {
            page_rows: Some(7),
            dictionary_bytes: small_dictionary.then_some(16),
            statistics,
            ..Options::new(group_rows, dictionary)
        };
        testing::write_with(&path, &[column], options).unwrap();
        let (old_rule, drop_encoding_stats, drop_null_counts) = rewrite;
        if old_rule || drop_encoding_stats || drop_null_counts {
            let rewrite = Rewrite {
                old_rule,
                drop_encoding_stats,
                drop_null_counts,
                ..Rewrite::default()
            };
            testing::rewrite_footer(&path, rewrite).unwrap();
        }
        let file = ParquetFile::open(&path).unwrap();
        let a = map_column(&file, &request("a", oid::INT8)).unwrap();
        // A row's value as the column reads it.
        let read = |v: i32| if unsigned { i64::from(v as u32) } else { i64::from(v) };
        let (test, passes): (ClauseTest, Passes<'_>) = match kind {
            0 => (ClauseTest::new(&a, ClauseKind::IsNull, &[]).unwrap(), Box::new(|v| v.is_none())),
            1 => (ClauseTest::new(&a, ClauseKind::IsNotNull, &[]).unwrap(), Box::new(|v| v.is_some())),
            2 => (
                ClauseTest::new(&a, ClauseKind::Compare(op), &[Some(constant)]).unwrap(),
                Box::new(move |v| v.is_some_and(|v| compare(op, read(v), constant))),
            ),
            _ => (
                ClauseTest::new(&a, ClauseKind::In, &list).unwrap(),
                Box::new(|v| v.is_some_and(|v| list.contains(&Some(read(v))))),
            ),
        };
        for (group, rows) in values.chunks(group_rows).enumerate() {
            let ruled_out = test.rules_out_group(&file, &a, group, false).unwrap()
                || test.rules_out_group(&file, &a, group, true).unwrap();
            if ruled_out {
                prop_assert!(!rows.iter().any(|&v| passes(v)), "group {} of {:?}", group, rows);
            }
        }
    }
}

// ---- the clauses on strings ----

/// A file of one string column `s`, of `values` in groups of `group_rows`,
/// its pages encoded through a dictionary where `dictionary` says.
fn strings_file(
    dir: &Path,
    values: &[Option<&[u8]>],
    group_rows: usize,
    dictionary: bool,
) -> (ParquetFile, MappedColumn) {
    let path = dir.join("strings.parquet");
    let written = values.iter().map(|v| v.map(<[u8]>::to_vec)).collect();
    testing::write(
        &path,
        &[Column::new("s", Written::Bytes(written))],
        group_rows,
        dictionary,
    )
    .unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let s = map_column(&file, &request("s", oid::TEXT)).unwrap();
    (file, s)
}

/// LIKE of a pattern of fixed parts and `%`, matched one byte at a time:
/// the rule PostgreSQL's `MatchText` follows for such a pattern.
fn naive_like(pattern: &[u8], value: &[u8]) -> bool {
    match pattern.split_first() {
        None => value.is_empty(),
        Some((b'%', rest)) => (0..=value.len()).any(|at| naive_like(rest, &value[at..])),
        Some((byte, rest)) => value.first() == Some(byte) && naive_like(rest, &value[1..]),
    }
}

/// The answer of a test of strings for a value, as PostgreSQL gives it
/// under a deterministic collation; `NULL` passes no test.
fn string_answer(kind: ClauseKind, constant: &[u8], value: Option<&[u8]>) -> bool {
    value.is_some_and(|value| match kind {
        ClauseKind::BytesEq => value == constant,
        ClauseKind::BytesNe => value != constant,
        ClauseKind::Like => naive_like(constant, value),
        ClauseKind::NotLike => !naive_like(constant, value),
        _ => unreachable!("a test of strings"),
    })
}

const STRING_KINDS: [ClauseKind; 4] = [
    ClauseKind::BytesEq,
    ClauseKind::BytesNe,
    ClauseKind::Like,
    ClauseKind::NotLike,
];

fn plain_strings(values: &[Option<Vec<u8>>]) -> LargeBinaryArray {
    values.iter().map(|v| v.as_deref()).collect()
}

/// The same strings as a dictionary of their distinct values, in the
/// order first seen.
fn dictionary_strings(values: &[Option<Vec<u8>>]) -> DictionaryArray<Int32Type> {
    let mut distinct: Vec<&[u8]> = Vec::new();
    let keys: Int32Array = values
        .iter()
        .map(|v| {
            v.as_deref().map(|v| {
                let at = distinct.iter().position(|d| *d == v).unwrap_or_else(|| {
                    distinct.push(v);
                    distinct.len() - 1
                });
                i32::try_from(at).unwrap()
            })
        })
        .collect();
    let values: LargeBinaryArray = distinct.into_iter().map(Some).collect();
    DictionaryArray::try_new(keys, Arc::new(values)).unwrap()
}

/// The examples of the spec, each kind of string test on plain and on
/// dictionary arrays: `<> ''` by the length, `LIKE 'ab%ba'` that rejects
/// `'aba'`, `LIKE 'a%a'` that rejects `'a'`, bytes that are not UTF-8
/// compared as any other, `NULL` passing none.
#[test]
fn strings_checked_by_their_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (_, s) = strings_file(dir.path(), &[Some(b"x")], 1, false);
    let values: Vec<Option<Vec<u8>>> = [
        Some(&b""[..]),
        Some(b"a"),
        Some(b"aba"),
        Some(b"abba"),
        Some(b"x"),
        None,
        Some(b"\xff\xfe"),
        Some(b"ab"),
    ]
    .iter()
    .map(|v| v.map(<[u8]>::to_vec))
    .collect();
    let len = values.len();
    let plain = plain_strings(&values);
    let dictionary = dictionary_strings(&values);
    let cases: [(ClauseKind, &[u8]); 10] = [
        (ClauseKind::BytesNe, b""),
        (ClauseKind::BytesEq, b""),
        (ClauseKind::BytesEq, b"x"),
        (ClauseKind::BytesNe, b"x"),
        (ClauseKind::BytesEq, b"\xff\xfe"),
        (ClauseKind::Like, b"%a%b%"),
        (ClauseKind::NotLike, b"a%"),
        (ClauseKind::Like, b"ab%ba"),
        (ClauseKind::Like, b"a%a"),
        (ClauseKind::Like, b"%"),
    ];
    for (kind, constant) in cases {
        let test = ClauseTest::of_bytes(&s, kind, Some(constant)).unwrap();
        let expected: Vec<bool> = values
            .iter()
            .map(|v| string_answer(kind, constant, v.as_deref()))
            .collect();
        for array in [&plain as &dyn Array, &dictionary as &dyn Array] {
            let mut passed = vec![u64::MAX; 2];
            test.apply(array, &mut passed).unwrap();
            assert_eq!(bits(&passed, len), expected, "{kind:?} {constant:?}");
            assert_eq!(passed[1], 0);
        }
    }
    // NULL passes no row.
    for kind in STRING_KINDS {
        let test = ClauseTest::of_bytes(&s, kind, None).unwrap();
        let mut passed = vec![u64::MAX; 2];
        test.apply(&plain, &mut passed).unwrap();
        assert_eq!(passed, [0, 0]);
    }
}

/// A test of strings is refused for another kind, for a pattern with `_`
/// or the escape character, and for a column that is not of strings;
/// `like_checked` says which patterns the reader takes.
#[test]
fn tests_of_strings_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (_, s) = strings_file(dir.path(), &[Some(b"x")], 1, false);
    let file = file(dir.path());
    let int4 = map_column(&file, &request("a", oid::INT4)).unwrap();
    assert!(ClauseTest::of_bytes(&s, ClauseKind::In, Some(b"x")).is_err());
    assert!(ClauseTest::of_bytes(&s, ClauseKind::IsNull, None).is_err());
    assert!(ClauseTest::of_bytes(&s, ClauseKind::Like, Some(b"a_b")).is_err());
    assert!(ClauseTest::of_bytes(&s, ClauseKind::NotLike, Some(b"a\\%")).is_err());
    assert!(ClauseTest::of_bytes(&int4, ClauseKind::BytesEq, Some(b"1")).is_err());
    assert!(ClauseTest::new(&s, ClauseKind::BytesEq, &[Some(1)]).is_err());
    assert!(like_checked(b"%a%b%") && like_checked(b"") && like_checked(b"%"));
    assert!(!like_checked(b"a_") && !like_checked(b"\\%"));
    // At most 31 % signs, the 32 parts of the kernels' pattern.
    assert!(like_checked(&[b'%'; 31]) && !like_checked(&[b'%'; 32]));
    // An array of another type than the strings the plan reads.
    let test = ClauseTest::of_bytes(&s, ClauseKind::BytesEq, Some(b"x")).unwrap();
    let mut passed = vec![0u64; 1];
    assert!(test.apply(&Int32Array::from(vec![1]), &mut passed).is_err());
}

/// The answers of a dictionary's values are kept for the windows that
/// share it, and made anew for another dictionary, whose answers differ.
#[test]
fn a_dictionary_s_answers_follow_their_dictionary() {
    let dir = tempfile::tempdir().unwrap();
    let (_, s) = strings_file(dir.path(), &[Some(b"x")], 1, false);
    let test = ClauseTest::of_bytes(&s, ClauseKind::Like, Some(b"a%")).unwrap();
    let first: Arc<dyn Array> = Arc::new(LargeBinaryArray::from(vec![&b"ab"[..], b"b"]));
    let second: Arc<dyn Array> = Arc::new(LargeBinaryArray::from(vec![&b"b"[..], b"ab"]));
    let window = |values: &Arc<dyn Array>, keys: Vec<Option<i32>>| {
        DictionaryArray::<Int32Type>::try_new(Int32Array::from(keys), Arc::clone(values)).unwrap()
    };
    let answer = |array: &DictionaryArray<Int32Type>| {
        let mut passed = vec![0u64; 1];
        test.apply(array, &mut passed).unwrap();
        bits(&passed, array.len())
    };
    assert_eq!(
        answer(&window(&first, vec![Some(0), Some(1), None])),
        [true, false, false]
    );
    assert_eq!(
        answer(&window(&first, vec![Some(1), Some(1), Some(0)])),
        [false, false, true]
    );
    assert_eq!(
        answer(&window(&second, vec![Some(0), Some(1), Some(0)])),
        [false, true, false]
    );
    assert_eq!(answer(&window(&first, vec![Some(0)])), [true]);
    // A NULL slot's key may name no value: it is not looked up.
    let keys = Int32Array::new(vec![0, 7].into(), Some(vec![true, false].into()));
    if let Ok(nulls) = DictionaryArray::<Int32Type>::try_new(keys, Arc::clone(&first)) {
        assert_eq!(answer(&nulls), [true, false]);
    }
}

/// The statistics of strings rule a group out by `= ''` where the least
/// value is not empty and by `<> ''` where the greatest is empty, and
/// nothing else of strings; a clause with `NULL` rules out every group.
#[test]
fn statistics_of_strings_rule_groups_out() {
    let of = ClauseTest::of;
    let empties = |least_empty, greatest_empty| GroupStats {
        empties: Some(Empties {
            least_empty,
            greatest_empty,
        }),
        ..GroupStats::default()
    };
    let equal = |value: &[u8]| {
        of(Kind::Bytes {
            equal: true,
            value: value.to_vec(),
        })
    };
    let unequal = |value: &[u8]| {
        of(Kind::Bytes {
            equal: false,
            value: value.to_vec(),
        })
    };
    assert!(equal(b"").rules_out(&empties(false, false), 10));
    assert!(!equal(b"").rules_out(&empties(true, false), 10));
    assert!(!equal(b"").rules_out(&GroupStats::default(), 10));
    assert!(unequal(b"").rules_out(&empties(true, true), 10));
    assert!(!unequal(b"").rules_out(&empties(true, false), 10));
    assert!(!equal(b"x").rules_out(&empties(false, false), 10));
    assert!(!unequal(b"x").rules_out(&empties(true, true), 10));
    let like = of(Kind::Like {
        pattern: b"%".to_vec(),
        negated: false,
    });
    assert!(!like.rules_out(&empties(false, true), 10));
    assert!(of(Kind::NoRow).rules_out(&GroupStats::default(), 10));
    // A least value not empty above a greatest one empty, as a damaged
    // footer may give, rules nothing out.
    assert!(!equal(b"").rules_out(&empties(false, true), 10));
    assert!(!unequal(b"").rules_out(&empties(false, true), 10));
}

/// A dictionary of strings rules a group out where no value passes.
#[test]
fn dictionaries_of_strings_rule_groups_out() {
    let dir = tempfile::tempdir().unwrap();
    let (_, s) = strings_file(dir.path(), &[Some(b"x")], 1, false);
    let values = Dictionary::Bytes(vec![b"ab".to_vec(), b"ba".to_vec()]);
    let test = |kind, value: &[u8]| ClauseTest::of_bytes(&s, kind, Some(value)).unwrap();
    assert!(test(ClauseKind::BytesEq, b"x").rules_out_by(&values));
    assert!(!test(ClauseKind::BytesEq, b"ba").rules_out_by(&values));
    assert!(test(ClauseKind::Like, b"%x%").rules_out_by(&values));
    assert!(!test(ClauseKind::Like, b"b%").rules_out_by(&values));
    assert!(test(ClauseKind::NotLike, b"%").rules_out_by(&values));
    assert!(!test(ClauseKind::BytesNe, b"ab").rules_out_by(&values));
    assert!(!test(ClauseKind::BytesEq, b"x").rules_out_by(&Dictionary::Integers(vec![1])));
}

/// A file's groups of strings ruled out through its footer: `=` and
/// `LIKE` by a dictionary, `= ''` and `<> ''` by the least and greatest
/// values, and `<> ''` not by the dictionary of a group whose greatest
/// value is not empty.
#[test]
fn groups_of_strings_ruled_out() {
    let dir = tempfile::tempdir().unwrap();
    let values: [Option<&[u8]>; 6] = [
        Some(b"ab"),
        Some(b"ba"),
        Some(b""),
        Some(b""),
        Some(b"cd"),
        None,
    ];
    let (file, s) = strings_file(dir.path(), &values, 2, true);
    let out = |kind, value: &[u8], group, dictionary| {
        ClauseTest::of_bytes(&s, kind, Some(value))
            .unwrap()
            .rules_out_group(&file, &s, group, dictionary)
            .unwrap()
    };
    // Group 0 holds "ab" and "ba", group 1 two empty strings, group 2 "cd"
    // and NULL.
    assert!(out(ClauseKind::BytesEq, b"cd", 0, true));
    assert!(!out(ClauseKind::BytesEq, b"cd", 2, true));
    assert!(!out(ClauseKind::BytesEq, b"cd", 0, false));
    assert!(out(ClauseKind::Like, b"c%", 0, true));
    assert!(!out(ClauseKind::Like, b"%b%", 0, true));
    assert!(out(ClauseKind::BytesEq, b"", 0, false));
    assert!(!out(ClauseKind::BytesEq, b"", 1, false));
    assert!(out(ClauseKind::BytesNe, b"", 1, false));
    assert!(!out(ClauseKind::BytesNe, b"", 0, false));
    // The greatest value of group 0 is not empty: its dictionary is not
    // read for <> '', which a damaged dictionary page shows, while = reads
    // it and fails.
    let start = usize::try_from(
        file.metadata()
            .row_group(0)
            .column(0)
            .dictionary_page_offset()
            .unwrap(),
    )
    .unwrap();
    let path = dir.path().join("strings.parquet");
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[start..start + 4].fill(0xff);
    std::fs::write(&path, &bytes).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let s = map_column(&file, &request("s", oid::TEXT)).unwrap();
    let unequal = ClauseTest::of_bytes(&s, ClauseKind::BytesNe, Some(b"")).unwrap();
    assert!(!unequal.rules_out_group(&file, &s, 0, true).unwrap());
    let equal = ClauseTest::of_bytes(&s, ClauseKind::BytesEq, Some(b"cd")).unwrap();
    assert_eq!(
        equal
            .rules_out_group(&file, &s, 0, true)
            .unwrap_err()
            .sqlstate(),
        "XX001"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Each kind of string test over plain and dictionary arrays, of
    /// random strings and patterns of `a`, `b` and `%`, each slot's bit
    /// the answer PostgreSQL gives, slices of an array among them.
    #[test]
    fn strings_as_postgresql_compares_them(
        values in prop::collection::vec(prop::option::of(prop::collection::vec(prop::sample::select(vec![b'a', b'b']), 0..6)), 0..150),
        constant in prop::collection::vec(prop::sample::select(vec![b'a', b'b', b'%']), 0..6),
        cut in 0usize..40,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (_, s) = strings_file(dir.path(), &[Some(b"x")], 1, false);
        let cut = cut.min(values.len());
        let kept = &values[cut..];
        let len = kept.len();
        let words = mask_words(len) + 1;
        let plain = plain_strings(&values);
        let dictionary = dictionary_strings(&values);
        let arrays: [Box<dyn Array>; 2] = [
            Box::new(plain.slice(cut, len)),
            Box::new(dictionary.slice(cut, len)),
        ];
        for kind in STRING_KINDS {
            let test = ClauseTest::of_bytes(&s, kind, Some(&constant)).unwrap();
            let expected: Vec<bool> =
                kept.iter().map(|v| string_answer(kind, &constant, v.as_deref())).collect();
            for array in &arrays {
                let mut passed = vec![u64::MAX; words];
                test.apply(array.as_ref(), &mut passed).unwrap();
                prop_assert_eq!(bits(&passed, len), expected.clone());
                prop_assert_eq!(passed[words - 1], 0);
            }
        }
    }

    /// Every group of strings the footer rules out, by its statistics or
    /// its dictionary, holds no row that passes: files of random strings,
    /// empty ones and `NULL` among them, in random groups, with and
    /// without dictionaries and statistics.
    #[test]
    fn groups_of_strings_ruled_out_hold_no_passing_row(
        values in prop::collection::vec(prop::option::of(prop::collection::vec(prop::sample::select(vec![b'a', b'b']), 0..3)), 1..60),
        group_rows in 1usize..12,
        dictionary in any::<bool>(),
        statistics in any::<bool>(),
        kind in 0usize..4,
        constant in prop::collection::vec(prop::sample::select(vec![b'a', b'b', b'%']), 0..4),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("strings.parquet");
        let options = Options { statistics, page_rows: Some(3), ..Options::new(group_rows, dictionary) };
        testing::write_with(&path, &[Column::new("s", Written::Bytes(values.clone()))], options).unwrap();
        let file = ParquetFile::open(&path).unwrap();
        let s = map_column(&file, &request("s", oid::TEXT)).unwrap();
        let kind = STRING_KINDS[kind];
        let test = ClauseTest::of_bytes(&s, kind, Some(&constant)).unwrap();
        for (group, rows) in values.chunks(group_rows).enumerate() {
            for by_dictionary in [false, true] {
                if test.rules_out_group(&file, &s, group, by_dictionary).unwrap() {
                    prop_assert!(
                        !rows.iter().any(|v| string_answer(kind, &constant, v.as_deref())),
                        "group {} ruled out by {:?} {:?}", group, kind, constant
                    );
                }
            }
        }
    }
}

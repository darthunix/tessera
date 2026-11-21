use std::sync::Arc;

use arrow_schema::{DataType, TimeUnit as ArrowUnit};
use proptest::prelude::*;

use super::*;
use crate::calendar::{EPOCH_DAYS, EPOCH_USECS, MAX_DATE, MAX_TIMESTAMP, MIN_DATE, MIN_TIMESTAMP};
use crate::read::{ReadOptions, TextBytes, Unit};

const OPTIONS: ReadOptions = ReadOptions {
    unit: None,
    text: None,
    server_utf8: true,
};

const COMPARISONS: [Comparison; 6] = [
    Comparison::Eq,
    Comparison::Ne,
    Comparison::Lt,
    Comparison::Le,
    Comparison::Gt,
    Comparison::Ge,
];

/// A plan of stored integers, the stored type's range, and how a stored
/// value counts in PostgreSQL's terms: `stored * scale - shift`.
struct Case {
    plan: ReadPlan,
    low: i64,
    high: i64,
    scale: i128,
    shift: i128,
    pg: PgType,
}

fn cases() -> Vec<Case> {
    use DataType as D;
    let unit = |unit| ReadOptions {
        unit: Some(unit),
        ..OPTIONS
    };
    let int = |data_type: DataType, pg, low, high| Case {
        plan: ReadPlan::new(&data_type, pg, OPTIONS).unwrap(),
        low,
        high,
        scale: 1,
        shift: 0,
        pg,
    };
    let days = |data_type: DataType, options, low, high| Case {
        plan: ReadPlan::new(&data_type, PgType::Date, options).unwrap(),
        low,
        high,
        scale: 1,
        shift: i128::from(EPOCH_DAYS),
        pg: PgType::Date,
    };
    let time = |data_type: DataType, options, scale: i128, pg| Case {
        plan: ReadPlan::new(&data_type, pg, options).unwrap(),
        low: i64::MIN,
        high: i64::MAX,
        scale,
        shift: i128::from(EPOCH_USECS),
        pg,
    };
    vec![
        int(D::Int8, PgType::Int2, -128, 127),
        int(D::Int16, PgType::Int2, -32_768, 32_767),
        int(
            D::Int32,
            PgType::Int4,
            i64::from(i32::MIN),
            i64::from(i32::MAX),
        ),
        int(D::Int64, PgType::Int8, i64::MIN, i64::MAX),
        int(D::UInt8, PgType::Int2, 0, 255),
        int(D::UInt16, PgType::Int4, 0, 65_535),
        int(D::UInt32, PgType::Int8, 0, i64::from(u32::MAX)),
        days(D::Date32, OPTIONS, i64::from(i32::MIN), i64::from(i32::MAX)),
        days(
            D::Int32,
            unit(Unit::Days),
            i64::from(i32::MIN),
            i64::from(i32::MAX),
        ),
        days(D::UInt16, unit(Unit::Days), 0, 65_535),
        days(D::Int8, unit(Unit::Days), -128, 127),
        days(D::UInt32, unit(Unit::Days), 0, i64::from(u32::MAX)),
        days(
            DataType::Dictionary(Box::new(D::Int8), Box::new(D::Int32)),
            unit(Unit::Days),
            i64::from(i32::MIN),
            i64::from(i32::MAX),
        ),
        time(D::Int64, unit(Unit::Seconds), 1_000_000, PgType::Timestamp),
        time(D::Int64, unit(Unit::Milliseconds), 1_000, PgType::Timestamp),
        time(D::Int64, unit(Unit::Microseconds), 1, PgType::Timestamptz),
        time(
            D::Timestamp(ArrowUnit::Second, Some(Arc::from("UTC"))),
            OPTIONS,
            1_000_000,
            PgType::Timestamptz,
        ),
        time(
            D::Timestamp(ArrowUnit::Millisecond, None),
            OPTIONS,
            1_000,
            PgType::Timestamp,
        ),
    ]
}

/// The truth of `stored op constant`, by an exact count in 128 bits of
/// the stored value, and the infinities above and below every value.
fn truth(case: &Case, op: Comparison, stored: i64, constant: Option<i64>) -> bool {
    let Some(constant) = constant else {
        return false;
    };
    let value = i128::from(stored) * case.scale - case.shift;
    let infinity = match case.pg {
        PgType::Date => [i64::from(i32::MAX), i64::from(i32::MIN)],
        PgType::Timestamp | PgType::Timestamptz => [i64::MAX, i64::MIN],
        _ => [0, 0],
    };
    let order = if infinity[0] != infinity[1] && constant == infinity[0] {
        core::cmp::Ordering::Less
    } else if infinity[0] != infinity[1] && constant == infinity[1] {
        core::cmp::Ordering::Greater
    } else {
        value.cmp(&i128::from(constant))
    };
    match op {
        Comparison::Eq => order.is_eq(),
        Comparison::Ne => order.is_ne(),
        Comparison::Lt => order.is_lt(),
        Comparison::Le => order.is_le(),
        Comparison::Gt => order.is_gt(),
        Comparison::Ge => order.is_ge(),
    }
}

/// Constants worth trying for a case: the edges of PostgreSQL's ranges,
/// the infinities, and the counts near a stored value, whole and not.
fn constants(case: &Case, stored: i64, noise: i64) -> Vec<Option<i64>> {
    let near = i128::from(stored) * case.scale - case.shift;
    let clamp =
        |v: i128| i64::try_from(v.clamp(i128::from(i64::MIN), i128::from(i64::MAX))).unwrap();
    let mut out = vec![
        None,
        Some(i64::MAX),
        Some(i64::MIN),
        Some(i64::from(i32::MAX)),
        Some(i64::from(i32::MIN)),
        Some(MIN_DATE),
        Some(MAX_DATE),
        Some(MIN_TIMESTAMP),
        Some(MAX_TIMESTAMP),
        Some(0),
        Some(-1),
        Some(256),
        Some(40_000),
        Some(-40_000),
        Some(noise),
    ];
    for delta in [-2i128, -1, 0, 1, 2] {
        out.push(Some(clamp(near + delta)));
        out.push(Some(clamp(near + delta * case.scale / 2)));
    }
    out
}

proptest! {
    /// Every case, comparison and constant: the test passes a stored
    /// value exactly when the exact comparison is true, for stored values
    /// across the stored type, those out of PostgreSQL's range among
    /// them.
    #[test]
    fn bounds_against_the_conversion(
        case in 0usize..64,
        stored in prop::collection::vec(any::<i64>(), 1..8),
        noise in any::<i64>(),
    ) {
        let cases = cases();
        let case = &cases[case % cases.len()];
        let width = i128::from(case.high) - i128::from(case.low) + 1;
        let stored: Vec<i64> = stored
            .iter()
            .map(|&s| i64::try_from(i128::from(case.low) + i128::from(s).rem_euclid(width)).unwrap())
            .chain([case.low, case.high])
            .collect();
        for &value in &stored {
            for constant in constants(case, value, noise) {
                for op in COMPARISONS {
                    let test = case.plan.test(op, constant).unwrap();
                    for &other in &stored {
                        prop_assert_eq!(
                            test.passes(other),
                            truth(case, op, other, constant),
                            "{:?} {:?} {:?} on {} as {:?}", case.plan.arrow_type(), op, constant, other, test
                        );
                    }
                }
            }
        }
    }

    /// Ranges of stored values: the answer of a test for a range equals
    /// the one the exact comparison of every value of the range gives.
    #[test]
    fn ranges_of_stored_values(
        case in 0usize..64,
        a in any::<i64>(),
        b in any::<i64>(),
        noise in any::<i64>(),
    ) {
        let cases = cases();
        let case = &cases[case % cases.len()];
        let width = i128::from(case.high) - i128::from(case.low) + 1;
        let fit = |s: i64| i64::try_from(i128::from(case.low) + i128::from(s).rem_euclid(width)).unwrap();
        let (a, b) = (fit(a), fit(b));
        let ranges = [(a.min(b), a.max(b)), (a, a), (case.low, case.high), (a.max(b), a.min(b))];
        for (least, greatest) in ranges {
            for constant in constants(case, least, noise).into_iter().chain(constants(case, greatest, noise)) {
                for op in COMPARISONS {
                    let test = case.plan.test(op, constant).unwrap();
                    let answer = test.range(least, greatest);
                    if least > greatest {
                        prop_assert_eq!(answer, RangeAnswer::SomeMay);
                        continue;
                    }
                    // The values that pass form a half-line for the
                    // orders, a point for `=` and its complement for `<>`:
                    // the ends of the range, and the point, decide.
                    let ends = [truth(case, op, least, constant), truth(case, op, greatest, constant)];
                    let finite = constant.filter(|&c| match case.pg {
                        PgType::Date => c != i64::from(i32::MAX) && c != i64::from(i32::MIN),
                        PgType::Timestamp | PgType::Timestamptz => c != i64::MAX && c != i64::MIN,
                        _ => true,
                    });
                    let point = finite.and_then(|c| {
                        let target = i128::from(c) + case.shift;
                        (target.rem_euclid(case.scale) == 0).then(|| target.div_euclid(case.scale))
                    });
                    let inside = point.filter(|p| (i128::from(least)..=i128::from(greatest)).contains(p));
                    let single = least == greatest;
                    let (all, none) = match op {
                        Comparison::Eq => (single && inside.is_some(), inside.is_none()),
                        Comparison::Ne if constant.is_none() => (false, true),
                        Comparison::Ne if finite.is_none() => (true, false),
                        Comparison::Ne => (inside.is_none(), single && inside.is_some()),
                        _ => (ends[0] && ends[1], !ends[0] && !ends[1]),
                    };
                    let expected = if all {
                        RangeAnswer::EveryValue
                    } else if none {
                        RangeAnswer::NoValue
                    } else {
                        RangeAnswer::SomeMay
                    };
                    prop_assert_eq!(answer, expected, "{:?} {:?} [{}, {}] {:?}", op, constant, least, greatest, test);
                }
            }
        }
    }
}

#[test]
fn bounds_of_clickbench_s_clauses() {
    // AdvEngineID <> 0 on a smallint: an integer constant of another type.
    let int2 = ReadPlan::new(&DataType::Int16, PgType::Int2, OPTIONS).unwrap();
    assert_eq!(
        int2.test(Comparison::Ne, Some(0)),
        Ok(Test::Compare {
            op: Comparison::Ne,
            bound: 0
        })
    );
    // UInt8 into smallint against 256 and against -1.
    let uint8 = ReadPlan::new(&DataType::UInt8, PgType::Int2, OPTIONS).unwrap();
    let below = uint8.test(Comparison::Lt, Some(256)).unwrap();
    assert!((0..=255).all(|v| below.passes(v)));
    assert_eq!(below.range(0, 255), RangeAnswer::EveryValue);
    let negative = uint8.test(Comparison::Le, Some(-1)).unwrap();
    assert_eq!(negative.range(0, 255), RangeAnswer::NoValue);
    // EventTime >= a time that is not a whole second, in seconds.
    let seconds = ReadPlan::new(
        &DataType::Int64,
        PgType::Timestamp,
        ReadOptions {
            unit: Some(Unit::Seconds),
            ..OPTIONS
        },
    )
    .unwrap();
    // 2000-01-01 00:00:00.5 is 946 684 800.5 seconds since 1970.
    assert_eq!(
        seconds.test(Comparison::Ge, Some(500_000)),
        Ok(Test::Compare {
            op: Comparison::Ge,
            bound: 946_684_801
        })
    );
    assert_eq!(
        seconds.test(Comparison::Eq, Some(500_000)),
        Ok(Test::Nothing)
    );
    assert_eq!(seconds.test(Comparison::Ne, Some(500_000)), Ok(Test::Every));
    // The infinities and a NULL constant.
    assert_eq!(
        seconds.test(Comparison::Lt, Some(i64::MAX)),
        Ok(Test::Every)
    );
    assert_eq!(
        seconds.test(Comparison::Gt, Some(i64::MAX)),
        Ok(Test::Nothing)
    );
    assert_eq!(
        seconds.test(Comparison::Ge, Some(i64::MIN)),
        Ok(Test::Every)
    );
    assert_eq!(seconds.test(Comparison::Eq, None), Ok(Test::Nothing));
    assert_eq!(seconds.test(Comparison::Ne, None), Ok(Test::Nothing));
}

#[test]
fn plans_without_bounds() {
    for (data_type, pg, options) in [
        (DataType::Float32, PgType::Float4, OPTIONS),
        (DataType::Float64, PgType::Float8, OPTIONS),
        (DataType::Utf8, PgType::Text, OPTIONS),
        (DataType::Binary, PgType::Bytea, OPTIONS),
        (
            DataType::Binary,
            PgType::Text,
            ReadOptions {
                text: Some(TextBytes::Checked),
                ..OPTIONS
            },
        ),
        (DataType::Boolean, PgType::Bool, OPTIONS),
    ] {
        let plan = ReadPlan::new(&data_type, pg, options).unwrap();
        let error = plan.test(Comparison::Eq, Some(0)).unwrap_err();
        assert_eq!(
            (error.kind(), error.sqlstate()),
            (ErrorKind::NotSupported, "0A000")
        );
    }
}

#[test]
fn a_test_applied_to_a_column() {
    let plan = ReadPlan::new(&DataType::Int32, PgType::Int4, OPTIONS).unwrap();
    let test = plan.test(Comparison::Ge, Some(10)).unwrap();
    let values: Vec<i32> = (0..70).collect();
    let mut valid = vec![0xffu8; 9];
    valid[1] = 0b1111_1011; // slot 10 is NULL
    let column = Column {
        len: 70,
        values: Values::Int32(&values),
        nulls: Some(Bitmap::new(&valid, 0)),
    };
    let mut passed = vec![u64::MAX; 2];
    test.apply(&column, &mut passed).unwrap();
    for slot in 0..70 {
        let bit = passed[slot / 64] >> (slot % 64) & 1 == 1;
        assert_eq!(bit, slot > 10, "slot {slot}: 10 is NULL");
    }
    assert_eq!(passed[1] >> 6, 0);
    // A column that is not of integers, short buffers and a short mask.
    let floats = [1.0f32];
    let mut unchanged = vec![7u64; 2];
    for column in [
        Column {
            len: 1,
            values: Values::Float32(&floats),
            nulls: None,
        },
        Column {
            len: 71,
            values: Values::Int32(&values),
            nulls: None,
        },
        Column {
            len: 70,
            values: Values::Int32(&values),
            nulls: Some(Bitmap::new(&valid[..8], 0)),
        },
    ] {
        let error = test.apply(&column, &mut unchanged).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BrokenShape);
        assert_eq!(unchanged, [7, 7]);
    }
    let error = test.apply(&column, &mut unchanged[..1]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BrokenShape);
}

/// The stored values of a case, in the width of its stored type.
enum Stored {
    Int8(Vec<i8>),
    Int16(Vec<i16>),
    Int32(Vec<i32>),
    Int64(Vec<i64>),
    UInt8(Vec<u8>),
    UInt16(Vec<u16>),
    UInt32(Vec<u32>),
}

impl Stored {
    /// `values`, each within the case's stored type, in its width.
    fn of(case: &Case, values: &[i64]) -> Stored {
        use crate::arrow::Plain;
        let fit = |v: i64| {
            let width = i128::from(case.high) - i128::from(case.low) + 1;
            i64::try_from(i128::from(case.low) + i128::from(v).rem_euclid(width)).unwrap()
        };
        let values = values.iter().map(|&v| fit(v));
        match case.plan.arrow_type().values() {
            Plain::Int8 => Stored::Int8(values.map(|v| i8::try_from(v).unwrap()).collect()),
            Plain::Int16 => Stored::Int16(values.map(|v| i16::try_from(v).unwrap()).collect()),
            Plain::Int32 | Plain::Date32 => {
                Stored::Int32(values.map(|v| i32::try_from(v).unwrap()).collect())
            }
            Plain::UInt8 => Stored::UInt8(values.map(|v| u8::try_from(v).unwrap()).collect()),
            Plain::UInt16 => Stored::UInt16(values.map(|v| u16::try_from(v).unwrap()).collect()),
            Plain::UInt32 => Stored::UInt32(values.map(|v| u32::try_from(v).unwrap()).collect()),
            _ => Stored::Int64(values.collect()),
        }
    }

    fn values(&self) -> Values<'_> {
        match self {
            Stored::Int8(v) => Values::Int8(v),
            Stored::Int16(v) => Values::Int16(v),
            Stored::Int32(v) => Values::Int32(v),
            Stored::Int64(v) => Values::Int64(v),
            Stored::UInt8(v) => Values::UInt8(v),
            Stored::UInt16(v) => Values::UInt16(v),
            Stored::UInt32(v) => Values::UInt32(v),
        }
    }

    fn get(&self, slot: usize) -> i64 {
        match self {
            Stored::Int8(v) => i64::from(v[slot]),
            Stored::Int16(v) => i64::from(v[slot]),
            Stored::Int32(v) => i64::from(v[slot]),
            Stored::Int64(v) => v[slot],
            Stored::UInt8(v) => i64::from(v[slot]),
            Stored::UInt16(v) => i64::from(v[slot]),
            Stored::UInt32(v) => i64::from(v[slot]),
        }
    }
}

proptest! {
    /// A test applied to columns of every stored type, with and without a
    /// bitmap from a random bit offset, of more than a word: the bit of a
    /// slot is set exactly where the slot is not NULL and its value passes
    /// by the exact comparison, every other bit of the words cleared.
    #[test]
    fn a_test_applied_to_columns_of_every_width(
        case in 0usize..64,
        values in prop::collection::vec(any::<i64>(), 0..150),
        valid in prop::collection::vec(any::<u8>(), 20),
        offset in 0usize..8,
        with_bitmap in any::<bool>(),
        pick in any::<usize>(),
        op in 0usize..6,
    ) {
        let cases = cases();
        let case = &cases[case % cases.len()];
        let stored = Stored::of(case, &values);
        let len = values.len();
        let constant = if len == 0 {
            Some(0)
        } else {
            let all = constants(case, stored.get(pick % len), 0);
            all[pick % all.len()]
        };
        let op = COMPARISONS[op];
        let test = case.plan.test(op, constant).unwrap();
        let bitmap = Bitmap::new(&valid, offset);
        let column = Column {
            len,
            values: stored.values(),
            nulls: with_bitmap.then_some(bitmap),
        };
        let mut passed = vec![u64::MAX; mask_words(len)];
        test.apply(&column, &mut passed).unwrap();
        for slot in 0..mask_words(len) * 64 {
            let bit = passed[slot / 64] >> (slot % 64) & 1 == 1;
            let expected = slot < len
                && (!with_bitmap || bitmap.get(slot))
                && truth(case, op, stored.get(slot), constant);
            prop_assert_eq!(bit, expected, "slot {} of {:?} {:?} {:?}", slot, op, constant, test);
        }
    }
}

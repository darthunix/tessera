use std::sync::Arc;

use arrow_schema::{DataType, TimeUnit as ArrowUnit};
use proptest::prelude::*;

use super::*;
use crate::bounds::Comparison;
use crate::calendar::{EPOCH_DAYS, EPOCH_USECS, MAX_DATE, MAX_TIMESTAMP, MIN_DATE, MIN_TIMESTAMP};
use crate::column::Bitmap;
use crate::pg::VarcharLength;
use crate::write::{WriteInput, WriteOutput, WritePlan};

const OPTIONS: ReadOptions = ReadOptions {
    unit: None,
    text: None,
    server_utf8: true,
};

fn with_unit(unit: Unit) -> ReadOptions {
    ReadOptions {
        unit: Some(unit),
        ..OPTIONS
    }
}

fn with_text(text: TextBytes) -> ReadOptions {
    ReadOptions {
        text: Some(text),
        ..OPTIONS
    }
}

fn varchar(n: u32) -> PgType {
    PgType::Varchar(Some(VarcharLength::new(n).unwrap()))
}

fn plan(data_type: &DataType, pg: PgType, options: ReadOptions) -> ReadPlan {
    ReadPlan::new(data_type, pg, options).unwrap()
}

fn refusal(data_type: &DataType, pg: PgType, options: ReadOptions) -> ErrorKind {
    ReadPlan::new(data_type, pg, options).unwrap_err().kind()
}

fn every(len: usize) -> Vec<u64> {
    let mut mask = vec![0u64; mask_words(len)];
    for slot in 0..len {
        mask[slot / 64] |= 1 << (slot % 64);
    }
    mask
}

fn timestamp(unit: ArrowUnit, zone: Option<&str>) -> DataType {
    DataType::Timestamp(unit, zone.map(Arc::from))
}

fn dictionary(key: DataType, value: DataType) -> DataType {
    DataType::Dictionary(Box::new(key), Box::new(value))
}

/// Reads the slots `asked` of `input` into `Datum` words.
fn datums(plan: &ReadPlan, input: &Input<'_>, asked: &[u64]) -> Result<(Vec<u64>, Vec<u64>)> {
    let len = input.len();
    let mut values = vec![0u64; len];
    let mut nulls = vec![0u64; mask_words(len)];
    plan.read(
        input,
        asked,
        &mut ReadOutput::Datums {
            values: &mut values,
            nulls: &mut nulls,
        },
    )?;
    Ok((values, nulls))
}

/// Reads the slots `asked` of `input` into bytes.
fn bytes<'a>(
    plan: &ReadPlan,
    input: &Input<'a>,
    asked: &[u64],
) -> Result<(Vec<&'a [u8]>, Vec<u64>)> {
    let len = input.len();
    let mut values: Vec<&'a [u8]> = vec![&[]; len];
    let mut nulls = vec![0u64; mask_words(len)];
    plan.read(
        input,
        asked,
        &mut ReadOutput::Bytes {
            values: &mut values,
            nulls: &mut nulls,
        },
    )?;
    Ok((values, nulls))
}

fn plain(values: Values<'_>, len: usize) -> Input<'_> {
    Input::Plain(Column {
        len,
        values,
        nulls: None,
    })
}

/// The offsets of `strings`, one after another.
fn offsets(strings: &[&[u8]]) -> (Vec<i32>, Vec<u8>) {
    let mut offsets = vec![0i32];
    let mut data = Vec::new();
    for string in strings {
        data.extend_from_slice(string);
        offsets.push(i32::try_from(data.len()).unwrap());
    }
    (offsets, data)
}

fn date_datum(days_since_2000: i64) -> u64 {
    i64::from(i32::try_from(days_since_2000).unwrap()) as u64
}

#[test]
fn clickbench_columns() {
    let cases = [
        (DataType::Int16, PgType::Int2, OPTIONS, Failures::NONE),
        (
            DataType::UInt16,
            PgType::Date,
            with_unit(Unit::Days),
            Failures::NONE,
        ),
        (
            DataType::Int64,
            PgType::Timestamp,
            with_unit(Unit::Seconds),
            Failures::OUT_OF_RANGE,
        ),
        (DataType::Int64, PgType::Int8, OPTIONS, Failures::NONE),
        (
            DataType::Binary,
            PgType::Text,
            with_text(TextBytes::Checked),
            Failures::NOT_UTF8 | Failures::ZERO_BYTE | Failures::TOO_LARGE,
        ),
    ];
    for (data_type, pg, options, failures) in cases {
        let plan = plan(&data_type, pg, options);
        assert_eq!(plan.failures(), failures, "{data_type:?} into {pg:?}");
        assert_eq!(plan.pg_type(), pg);
        assert_eq!(plan.arrow_type(), ArrowType::new(&data_type).unwrap());
    }
}

#[test]
fn every_pair_accepted() {
    use DataType as D;
    let none = Failures::NONE;
    let range = Failures::OUT_OF_RANGE;
    let utf8 = Failures::ZERO_BYTE | Failures::TOO_LARGE;
    let checked = Failures::NOT_UTF8 | utf8;
    let long = Failures::TOO_LONG;
    let mut cases: Vec<(DataType, PgType, ReadOptions, Failures)> = vec![
        (D::Boolean, PgType::Bool, OPTIONS, none),
        (D::Float32, PgType::Float4, OPTIONS, none),
        (D::Float32, PgType::Float8, OPTIONS, none),
        (D::Float64, PgType::Float8, OPTIONS, none),
        (D::Date32, PgType::Date, OPTIONS, range),
        (D::Int8, PgType::Date, with_unit(Unit::Days), none),
        (D::Int16, PgType::Date, with_unit(Unit::Days), none),
        (D::UInt8, PgType::Date, with_unit(Unit::Days), none),
        (D::UInt16, PgType::Date, with_unit(Unit::Days), none),
        (D::Int32, PgType::Date, with_unit(Unit::Days), range),
        (D::UInt32, PgType::Date, with_unit(Unit::Days), range),
        (D::Utf8, PgType::Text, OPTIONS, utf8),
        (D::LargeUtf8, PgType::Text, OPTIONS, utf8),
        (D::Utf8, PgType::Varchar(None), OPTIONS, utf8),
        (D::LargeUtf8, varchar(10), OPTIONS, utf8 | long),
        (D::Utf8, varchar(10), OPTIONS, utf8 | long),
        (D::Binary, PgType::Bytea, OPTIONS, Failures::TOO_LARGE),
        (D::LargeBinary, PgType::Bytea, OPTIONS, Failures::TOO_LARGE),
        (
            D::Binary,
            PgType::Text,
            with_text(TextBytes::Checked),
            checked,
        ),
        (
            D::LargeBinary,
            PgType::Text,
            with_text(TextBytes::Checked),
            checked,
        ),
        (
            D::Binary,
            PgType::Varchar(None),
            with_text(TextBytes::Checked),
            checked,
        ),
        (
            D::Binary,
            varchar(10),
            with_text(TextBytes::Checked),
            checked | long,
        ),
        (
            D::Binary,
            PgType::Text,
            with_text(TextBytes::Trusted),
            Failures::TOO_LARGE,
        ),
        (
            D::LargeBinary,
            PgType::Varchar(None),
            with_text(TextBytes::Trusted),
            Failures::TOO_LARGE,
        ),
        (D::FixedSizeBinary(16), PgType::Bytea, OPTIONS, none),
        (dictionary(D::Int8, D::Utf8), PgType::Text, OPTIONS, utf8),
        (
            dictionary(D::UInt64, D::Binary),
            PgType::Text,
            with_text(TextBytes::Checked),
            checked,
        ),
        (
            dictionary(D::Int32, D::Int32),
            PgType::Date,
            with_unit(Unit::Days),
            range,
        ),
    ];
    // Every integer into every integer type that holds it.
    for (data_type, targets) in [
        (D::Int8, &[PgType::Int2, PgType::Int4, PgType::Int8][..]),
        (D::Int16, &[PgType::Int2, PgType::Int4, PgType::Int8]),
        (D::Int32, &[PgType::Int4, PgType::Int8]),
        (D::Int64, &[PgType::Int8]),
        (D::UInt8, &[PgType::Int2, PgType::Int4, PgType::Int8]),
        (D::UInt16, &[PgType::Int4, PgType::Int8]),
        (D::UInt32, &[PgType::Int8]),
    ] {
        for &pg in targets {
            cases.push((data_type.clone(), pg, OPTIONS, none));
        }
    }
    // Timestamps in each unit, without a time zone, with an empty one and
    // with others; Int64 with each unit of time.
    for unit in [
        ArrowUnit::Second,
        ArrowUnit::Millisecond,
        ArrowUnit::Microsecond,
    ] {
        cases.push((timestamp(unit, None), PgType::Timestamp, OPTIONS, range));
        cases.push((timestamp(unit, Some("")), PgType::Timestamp, OPTIONS, range));
        for zone in ["UTC", "Europe/Paris", "+03:00"] {
            cases.push((
                timestamp(unit, Some(zone)),
                PgType::Timestamptz,
                OPTIONS,
                range,
            ));
        }
    }
    for unit in [Unit::Seconds, Unit::Milliseconds, Unit::Microseconds] {
        cases.push((D::Int64, PgType::Timestamp, with_unit(unit), range));
        cases.push((D::Int64, PgType::Timestamptz, with_unit(unit), range));
    }
    for (data_type, pg, options, failures) in cases {
        let plan = ReadPlan::new(&data_type, pg, options)
            .unwrap_or_else(|e| panic!("{data_type:?} into {pg:?}: {e}"));
        assert_eq!(plan.failures(), failures, "{data_type:?} into {pg:?}");
        let bytes = matches!(pg, PgType::Text | PgType::Varchar(_) | PgType::Bytea);
        assert_eq!(plan.gives_datums(), !bytes);
    }
}

#[test]
fn pairs_refused_in_order() {
    use DataType as D;
    use ErrorKind::*;
    let latin1 = ReadOptions {
        server_utf8: false,
        ..OPTIONS
    };
    let cases = [
        // The type before the unit.
        (
            D::Decimal128(10, 2),
            PgType::Date,
            with_unit(Unit::Days),
            NotSupported,
        ),
        // The units and the treatments, before the pairs.
        (D::Utf8, PgType::Date, with_unit(Unit::Days), InvalidOption),
        (
            D::Date32,
            PgType::Date,
            with_unit(Unit::Days),
            InvalidOption,
        ),
        (D::Utf8, PgType::Int4, with_unit(Unit::Days), InvalidOption),
        (D::Int64, PgType::Date, with_unit(Unit::Days), InvalidOption),
        (
            D::Int32,
            PgType::Timestamp,
            with_unit(Unit::Seconds),
            InvalidOption,
        ),
        (
            timestamp(ArrowUnit::Second, None),
            PgType::Timestamp,
            with_unit(Unit::Seconds),
            InvalidOption,
        ),
        (
            D::Utf8,
            PgType::Text,
            with_text(TextBytes::Checked),
            InvalidOption,
        ),
        (
            D::Binary,
            PgType::Bytea,
            with_text(TextBytes::Checked),
            InvalidOption,
        ),
        // The pairs.
        (D::Int64, PgType::Int2, OPTIONS, TypeMismatch),
        (D::UInt32, PgType::Int4, OPTIONS, TypeMismatch),
        (D::UInt16, PgType::Int2, OPTIONS, TypeMismatch),
        (D::Int32, PgType::Int2, OPTIONS, TypeMismatch),
        (D::Float64, PgType::Float4, OPTIONS, TypeMismatch),
        (D::Utf8, PgType::Date, OPTIONS, TypeMismatch),
        (D::Utf8, PgType::Bytea, OPTIONS, TypeMismatch),
        (
            timestamp(ArrowUnit::Second, Some("UTC")),
            PgType::Timestamp,
            OPTIONS,
            TypeMismatch,
        ),
        (
            timestamp(ArrowUnit::Second, None),
            PgType::Timestamptz,
            OPTIONS,
            TypeMismatch,
        ),
        (D::Int32, PgType::Int4, with_unit(Unit::Days), TypeMismatch),
        (
            D::Int64,
            PgType::Int8,
            with_unit(Unit::Seconds),
            TypeMismatch,
        ),
        (D::Binary, PgType::Text, OPTIONS, TypeMismatch),
        (
            D::Binary,
            varchar(10),
            with_text(TextBytes::Trusted),
            TypeMismatch,
        ),
        (D::Boolean, PgType::Int2, OPTIONS, TypeMismatch),
        (D::FixedSizeBinary(4), PgType::Text, OPTIONS, TypeMismatch),
        // The size after the pair.
        (
            D::FixedSizeBinary(1_073_741_820),
            PgType::Bytea,
            OPTIONS,
            TooLarge,
        ),
        (
            D::FixedSizeBinary(1_073_741_820),
            PgType::Text,
            OPTIONS,
            TypeMismatch,
        ),
        // The encoding last.
        (D::Utf8, PgType::Text, latin1, NotSupported),
        (D::Binary, PgType::Text, latin1, TypeMismatch),
        (
            D::Utf8,
            PgType::Text,
            ReadOptions {
                unit: Some(Unit::Days),
                ..latin1
            },
            InvalidOption,
        ),
    ];
    for (data_type, pg, options, kind) in cases {
        assert_eq!(
            refusal(&data_type, pg, options),
            kind,
            "{data_type:?} into {pg:?} with {options:?}"
        );
    }
    assert!(ReadPlan::new(&D::FixedSizeBinary(1_073_741_819), PgType::Bytea, OPTIONS).is_ok());
    assert!(ReadPlan::new(&D::Binary, PgType::Bytea, latin1).is_ok());
}

#[test]
fn days_and_times_at_the_edges_of_each_unit() {
    let days_plan = plan(&DataType::Date32, PgType::Date, OPTIONS);
    let first = MIN_DATE + EPOCH_DAYS;
    let last = MAX_DATE + EPOCH_DAYS;
    let days: Vec<i32> = [first, last, first - 1, last + 1]
        .iter()
        .map(|&d| i32::try_from(d).unwrap())
        .collect();
    let input = plain(Values::Int32(&days), days.len());
    for (slot, expected) in [
        (0, Some(MIN_DATE)),
        (1, Some(MAX_DATE)),
        (2, None),
        (3, None),
    ] {
        match (datums(&days_plan, &input, &[1 << slot]), expected) {
            (Ok((values, _)), Some(days)) => assert_eq!(values[slot], date_datum(days)),
            (Err(e), None) => {
                assert_eq!((e.kind(), e.slot()), (ErrorKind::OutOfRange, Some(slot)));
                assert_eq!((e.sqlstate(), e.message()), ("22008", "date out of range"));
            }
            (result, _) => panic!("slot {slot}: {result:?}"),
        }
    }
    // Days in an Int32 with the unit of days, the same edges.
    let int_days = plan(&DataType::Int32, PgType::Date, with_unit(Unit::Days));
    assert!(datums(&int_days, &input, &[0b11]).is_ok());
    assert_eq!(
        datums(&int_days, &input, &[0b100]).unwrap_err().slot(),
        Some(2)
    );
    // Days in a UInt32 past what a date holds, and in a UInt16, which a
    // date always holds.
    let unsigned = [2_147_483_648u32, u32::MAX];
    let u32_days = plan(&DataType::UInt32, PgType::Date, with_unit(Unit::Days));
    let input = plain(Values::UInt32(&unsigned), 2);
    for slot in 0..2 {
        let error = datums(&u32_days, &input, &[1 << slot]).unwrap_err();
        assert_eq!(
            (error.kind(), error.slot()),
            (ErrorKind::OutOfRange, Some(slot))
        );
    }
    let u16_days = plan(&DataType::UInt16, PgType::Date, with_unit(Unit::Days));
    let input = plain(Values::UInt16(&[u16::MAX]), 1);
    assert_eq!(
        datums(&u16_days, &input, &[1]).unwrap().0[0],
        date_datum(65_535 - EPOCH_DAYS)
    );

    // Times: the first and the last whole unit PostgreSQL holds and one
    // past each, by seconds and milliseconds; by microseconds, the first,
    // one before it, and the largest i64.
    for (unit, per_second) in [(Unit::Seconds, 1i128), (Unit::Milliseconds, 1_000)] {
        let time = plan(&DataType::Int64, PgType::Timestamp, with_unit(unit));
        let scale = 1_000_000 / per_second;
        let shift = i128::from(EPOCH_USECS);
        let low = i128::from(MIN_TIMESTAMP) + shift;
        let first = low.div_euclid(scale) + i128::from(low.rem_euclid(scale) != 0);
        let last = (i128::from(MAX_TIMESTAMP) + shift).div_euclid(scale);
        let stored: Vec<i64> = [first, last, first - 1, last + 1]
            .iter()
            .map(|&v| i64::try_from(v).unwrap())
            .collect();
        let input = plain(Values::Int64(&stored), 4);
        let (values, _) = datums(&time, &input, &[0b11]).unwrap();
        for slot in 0..2 {
            let exact = i128::from(stored[slot]) * scale - shift;
            assert_eq!(
                values[slot],
                i64::try_from(exact).unwrap() as u64,
                "{unit:?}"
            );
        }
        for slot in 2..4 {
            let error = datums(&time, &input, &[1 << slot]).unwrap_err();
            assert_eq!(
                (error.kind(), error.slot()),
                (ErrorKind::OutOfRange, Some(slot))
            );
            assert_eq!(error.message(), "timestamp out of range");
        }
    }
    let micros = plan(
        &timestamp(ArrowUnit::Microsecond, Some("UTC")),
        PgType::Timestamptz,
        OPTIONS,
    );
    let first = MIN_TIMESTAMP + EPOCH_USECS;
    let stored = [first, first - 1, i64::MAX];
    let input = plain(Values::Int64(&stored), 3);
    let (values, _) = datums(&micros, &input, &[0b101]).unwrap();
    assert_eq!(values[0], MIN_TIMESTAMP as u64);
    assert_eq!(values[2], (i64::MAX - EPOCH_USECS) as u64);
    assert_eq!(
        datums(&micros, &input, &[0b10]).unwrap_err().slot(),
        Some(1)
    );
}

#[test]
fn strings_checked() {
    let three = "абв".as_bytes(); // three characters, six bytes
    let four = "abcd".as_bytes();
    let spaced = "abc ".as_bytes();
    let not_utf8: &[u8] = b"abc\xffd";
    let zero: &[u8] = b"abc\0d";
    let strings: [&[u8]; 5] = [not_utf8, zero, three, four, spaced];
    let (offs, data) = offsets(&strings);
    let input = plain(
        Values::Bytes {
            offsets: &offs,
            data: &data,
        },
        5,
    );
    let run = |plan: &ReadPlan| -> Vec<std::result::Result<&[u8], ErrorKind>> {
        (0..5)
            .map(|slot| {
                bytes(plan, &input, &[1 << slot])
                    .map(|(values, _)| values[slot])
                    .map_err(|e| {
                        assert_eq!(e.slot(), Some(slot));
                        e.kind()
                    })
            })
            .collect()
    };
    use ErrorKind::{InvalidText, TooLong};
    // The bytes that are not UTF-8 and the zero byte, each longer than
    // three characters, fail for their bytes, not their length.
    let checked = plan(&DataType::Binary, varchar(3), with_text(TextBytes::Checked));
    assert_eq!(
        run(&checked),
        [
            Err(InvalidText),
            Err(InvalidText),
            Ok(three),
            Err(TooLong),
            Err(TooLong)
        ]
    );
    // From Utf8, which the crate takes as UTF-8: the zero byte and the
    // length are checked.
    let utf8 = plan(&DataType::Utf8, varchar(3), OPTIONS);
    assert_eq!(
        run(&utf8)[1..],
        [Err(InvalidText), Ok(three), Err(TooLong), Err(TooLong)]
    );
    // From trusted bytes into text, all pass.
    let trusted = plan(
        &DataType::Binary,
        PgType::Text,
        with_text(TextBytes::Trusted),
    );
    assert_eq!(
        run(&trusted),
        strings.iter().map(|s| Ok(*s)).collect::<Vec<_>>()
    );
    let error = bytes(&checked, &input, &[1]).unwrap_err();
    assert_eq!(
        (error.sqlstate(), error.message()),
        ("22021", "invalid byte sequence for encoding \"UTF8\"")
    );
    let error = bytes(&checked, &input, &[1 << 3]).unwrap_err();
    assert_eq!(error.sqlstate(), "22001");
}

#[test]
fn a_string_too_large_fails_before_its_bytes_are_checked() {
    // MAX_VALUE_BYTES + 1 zero bytes: the size is checked before the zero
    // bytes, and before UTF-8.
    let data = &crate::zeros()[..=MAX_VALUE_BYTES];
    let offs = [0i64, i64::try_from(data.len()).unwrap()];
    let input = plain(
        Values::LargeBytes {
            offsets: &offs,
            data,
        },
        1,
    );
    for plan in [
        plan(
            &DataType::LargeBinary,
            PgType::Text,
            with_text(TextBytes::Checked),
        ),
        plan(&DataType::LargeBinary, PgType::Bytea, OPTIONS),
        plan(
            &DataType::LargeBinary,
            PgType::Text,
            with_text(TextBytes::Trusted),
        ),
    ] {
        let error = bytes(&plan, &input, &[1]).unwrap_err();
        assert_eq!(
            (error.kind(), error.sqlstate()),
            (ErrorKind::TooLarge, "54000")
        );
    }
    // The largest value PostgreSQL holds is read.
    let offs = [0i64, i64::try_from(MAX_VALUE_BYTES).unwrap()];
    let largest = plain(
        Values::LargeBytes {
            offsets: &offs,
            data,
        },
        1,
    );
    let plan = plan(&DataType::LargeBinary, PgType::Bytea, OPTIONS);
    assert_eq!(
        bytes(&plan, &largest, &[1]).unwrap().0[0].len(),
        MAX_VALUE_BYTES
    );
}

#[test]
fn numbers_widened() {
    let check = |data_type: DataType, pg: PgType, input: Input<'_>, expected: Vec<i64>| {
        let plan = plan(&data_type, pg, OPTIONS);
        let (values, _) = datums(&plan, &input, &every(input.len())).unwrap();
        let expected: Vec<u64> = expected.into_iter().map(|v| v as u64).collect();
        assert_eq!(values, expected, "{data_type:?} into {pg:?}");
    };
    for pg in [PgType::Int2, PgType::Int4, PgType::Int8] {
        let int8 = plain(Values::Int8(&[i8::MIN, i8::MAX]), 2);
        check(DataType::Int8, pg, int8, vec![-128, 127]);
        let int16 = plain(Values::Int16(&[i16::MIN, i16::MAX]), 2);
        check(DataType::Int16, pg, int16, vec![-32768, 32767]);
        check(
            DataType::UInt8,
            pg,
            plain(Values::UInt8(&[0, 255]), 2),
            vec![0, 255],
        );
    }
    for pg in [PgType::Int4, PgType::Int8] {
        check(
            DataType::Int32,
            pg,
            plain(Values::Int32(&[i32::MIN, i32::MAX]), 2),
            vec![i64::from(i32::MIN), i64::from(i32::MAX)],
        );
        let uint16 = plain(Values::UInt16(&[0, 65_535]), 2);
        check(DataType::UInt16, pg, uint16, vec![0, 65_535]);
    }
    check(
        DataType::Int64,
        PgType::Int8,
        plain(Values::Int64(&[i64::MIN, i64::MAX]), 2),
        vec![i64::MIN, i64::MAX],
    );
    check(
        DataType::UInt32,
        PgType::Int8,
        plain(Values::UInt32(&[0, u32::MAX]), 2),
        vec![0, 4_294_967_295],
    );
    // Float32 widened to the same value, its sign kept, NaN a NaN.
    let floats = [
        f32::from_bits(1),
        f32::MAX,
        0.0,
        -0.0,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
    ];
    let wide = plan(&DataType::Float32, PgType::Float8, OPTIONS);
    let input = plain(Values::Float32(&floats), 7);
    let (values, _) = datums(&wide, &input, &every(7)).unwrap();
    for (value, datum) in floats.iter().zip(&values) {
        let back = f64::from_bits(*datum);
        if value.is_nan() {
            assert!(back.is_nan());
        } else {
            assert_eq!(back, f64::from(*value));
            assert_eq!(back.is_sign_negative(), value.is_sign_negative());
        }
    }
    // A real as PostgreSQL's Float4GetDatum makes it: its bits widened
    // with their sign.
    let real = plan(&DataType::Float32, PgType::Float4, OPTIONS);
    let input = plain(Values::Float32(&[-1.5, 2.0]), 2);
    let (values, _) = datums(&real, &input, &every(2)).unwrap();
    assert_eq!(values[0], i64::from((-1.5f32).to_bits() as i32) as u64);
    assert_eq!(values[0] >> 32, 0xffff_ffff);
    assert_eq!(values[1], u64::from(2.0f32.to_bits()));
    let double = plan(&DataType::Float64, PgType::Float8, OPTIONS);
    let input = plain(Values::Float64(&[-0.25]), 1);
    let (values, _) = datums(&double, &input, &[1]).unwrap();
    assert_eq!(values[0], (-0.25f64).to_bits());
}

/// Strings, bytes and dictionaries of either output read for a sparse
/// selection: the place of every slot not asked keeps what the caller
/// put there.
#[test]
fn places_of_slots_not_asked_kept() {
    let (offs, data) = offsets(&[b"a", b"bb", b"ccc", b"dddd", b"e"]);
    let asked = [0b1_0010u64];
    let kept: &[u8] = b"kept";
    let text = plan(&DataType::Utf8, PgType::Text, OPTIONS);
    let strings = plain(
        Values::Bytes {
            offsets: &offs,
            data: &data,
        },
        5,
    );
    let entries = Column {
        len: 5,
        values: Values::Bytes {
            offsets: &offs,
            data: &data,
        },
        nulls: None,
    };
    let keys = [4u8, 3, 2, 1, 0];
    let text_dictionary = Input::Dictionary {
        len: 5,
        keys: Keys::UInt8(&keys),
        nulls: None,
        values: entries,
    };
    let dictionary_plan = plan(
        &dictionary(DataType::UInt8, DataType::Utf8),
        PgType::Text,
        OPTIONS,
    );
    for (plan, input, expected) in [
        (&text, &strings, [&b"bb"[..], b"e"]),
        (&dictionary_plan, &text_dictionary, [&b"dddd"[..], b"a"]),
    ] {
        let mut values: Vec<&[u8]> = vec![kept; 5];
        let mut nulls = vec![0u64; 1];
        plan.read(
            input,
            &asked,
            &mut ReadOutput::Bytes {
                values: &mut values,
                nulls: &mut nulls,
            },
        )
        .unwrap();
        assert_eq!([values[1], values[4]], expected);
        assert!([0, 2, 3].iter().all(|&slot| values[slot] == kept));
    }
    let days = [1i32, 2, 3, 4, 5];
    let day_dictionary = Input::Dictionary {
        len: 5,
        keys: Keys::UInt8(&keys),
        nulls: None,
        values: Column {
            len: 5,
            values: Values::Int32(&days),
            nulls: None,
        },
    };
    let day_plan = plan(
        &dictionary(DataType::UInt8, DataType::Int32),
        PgType::Date,
        with_unit(Unit::Days),
    );
    let mut values = vec![7u64; 5];
    let mut nulls = vec![0u64; 1];
    day_plan
        .read(
            &day_dictionary,
            &asked,
            &mut ReadOutput::Datums {
                values: &mut values,
                nulls: &mut nulls,
            },
        )
        .unwrap();
    let day = |stored: i32| (i64::from(stored) - EPOCH_DAYS) as u64;
    assert_eq!([values[1], values[4]], [day(4), day(1)]);
    assert!([0, 2, 3].iter().all(|&slot| values[slot] == 7));
}

#[test]
fn slots_asked_null_slots_and_dictionaries() {
    let time = plan(
        &DataType::Int64,
        PgType::Timestamp,
        with_unit(Unit::Seconds),
    );
    let stored = [0i64, i64::MAX, 5, i64::MIN, 7];
    let seconds = |s: i64| ((s * 1_000_000) - EPOCH_USECS) as u64;
    // NULL slots 1 and 3 hold out-of-range times: they give NULL and do
    // not fail.
    let valid = [0b1_0101u8];
    let input = Input::Plain(Column {
        len: 5,
        values: Values::Int64(&stored),
        nulls: Some(Bitmap::new(&valid, 0)),
    });
    let (values, nulls) = datums(&time, &input, &every(5)).unwrap();
    assert_eq!(nulls, [0b0_1010]);
    assert_eq!(
        [values[0], values[2], values[4]],
        [seconds(0), seconds(5), seconds(7)]
    );
    // A column without a bitmap: slot 1's value is out of range, not
    // asked, and raises no failure.
    let no_bitmap = plain(Values::Int64(&stored), 5);
    let (_, nulls) = datums(&time, &no_bitmap, &[0b1_0101]).unwrap();
    assert_eq!(nulls, [0]);
    // A bitmap from a bit offset of 5: slot i is bit 5 + i.
    let shifted = [0b1010_0000u8, 0b0000_0010];
    let input = Input::Plain(Column {
        len: 5,
        values: Values::Int64(&stored),
        nulls: Some(Bitmap::new(&shifted, 5)),
    });
    let (_, nulls) = datums(&time, &input, &every(5)).unwrap();
    assert_eq!(nulls, [0b0_1010]);
    // A Boolean column read from a bit offset of 3.
    let bools = [0b1010_1000u8, 0b1];
    let boolean = plan(&DataType::Boolean, PgType::Bool, OPTIONS);
    let input = plain(Values::Boolean(Bitmap::new(&bools, 3)), 6);
    let (values, _) = datums(&boolean, &input, &every(6)).unwrap();
    assert_eq!(values, [1, 0, 1, 0, 1, 1]);

    // A dictionary of strings: its value 3, which no slot refers to, is
    // not UTF-8; its value 1, referred to, is NULL by its bitmap over
    // bytes that are not UTF-8; its NULL slot 2 and its slot 4, not
    // asked, hold indexes past its end.
    let entries: [&[u8]; 4] = [b"ok", b"\xff", b"fine", b"\xfe"];
    let (offs, data) = offsets(&entries);
    let entry_valid = [0b1101u8];
    let keys = [0i32, 1, 99, 2, -5];
    let key_valid = [0b1_1011u8];
    let input = Input::Dictionary {
        len: 5,
        keys: Keys::Int32(&keys),
        nulls: Some(Bitmap::new(&key_valid, 0)),
        values: Column {
            len: 4,
            values: Values::Bytes {
                offsets: &offs,
                data: &data,
            },
            nulls: Some(Bitmap::new(&entry_valid, 0)),
        },
    };
    let text = plan(
        &dictionary(DataType::Int32, DataType::Binary),
        PgType::Text,
        with_text(TextBytes::Checked),
    );
    let (values, nulls) = bytes(&text, &input, &[0b0_1111]).unwrap();
    assert_eq!(nulls, [0b0110]);
    assert_eq!((values[0], values[3]), (&b"ok"[..], &b"fine"[..]));
    // An index outside the dictionary in a slot asked that is not NULL.
    let error = bytes(&text, &input, &[0b1_0000]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BrokenShape);

    // A dictionary of days.
    let days = [0i32, i32::MAX];
    let day_keys = [0u8, 0, 1];
    let input = Input::Dictionary {
        len: 3,
        keys: Keys::UInt8(&day_keys),
        nulls: None,
        values: Column {
            len: 2,
            values: Values::Int32(&days),
            nulls: None,
        },
    };
    let date = plan(
        &dictionary(DataType::UInt8, DataType::Int32),
        PgType::Date,
        with_unit(Unit::Days),
    );
    let (values, nulls) = datums(&date, &input, &[0b011]).unwrap();
    assert_eq!(nulls, [0]);
    assert_eq!(
        values[..2],
        [date_datum(-EPOCH_DAYS), date_datum(-EPOCH_DAYS)]
    );
    let error = datums(&date, &input, &[0b111]).unwrap_err();
    assert_eq!(
        (error.kind(), error.slot()),
        (ErrorKind::OutOfRange, Some(2))
    );

    // The fifth slot asked fails, the four before it converted.
    let stored = [1i64, 2, 3, 4, i64::MAX, 6];
    let input = plain(Values::Int64(&stored), 6);
    let mut values = vec![u64::MAX; 6];
    let mut nulls = vec![u64::MAX; 1];
    let error = time
        .read(
            &input,
            &[0b11_1111],
            &mut ReadOutput::Datums {
                values: &mut values,
                nulls: &mut nulls,
            },
        )
        .unwrap_err();
    assert_eq!(error.slot(), Some(4));
    for (slot, value) in values.iter().enumerate().take(4) {
        assert_eq!(*value, seconds(stored[slot]));
    }
}

#[test]
fn a_failure_found_within_a_word() {
    let time = plan(
        &DataType::Int64,
        PgType::Timestamp,
        with_unit(Unit::Seconds),
    );
    let mut stored = vec![0i64; 100];
    // Slot 70 fails; before it, in the same word, slot 66 is NULL and
    // slot 68 is not asked, holding the least and greatest i64.
    stored[66] = i64::MIN;
    stored[68] = i64::MAX;
    stored[70] = i64::MAX;
    let mut valid = vec![0xffu8; 13];
    valid[66 / 8] &= !(1 << (66 % 8));
    let mut asked = every(100);
    asked[1] &= !(1 << (68 - 64));
    let input = Input::Plain(Column {
        len: 100,
        values: Values::Int64(&stored),
        nulls: Some(Bitmap::new(&valid, 0)),
    });
    let mut values = vec![0u64; 100];
    let mut nulls = vec![0u64; 2];
    let error = time
        .read(
            &input,
            &asked,
            &mut ReadOutput::Datums {
                values: &mut values,
                nulls: &mut nulls,
            },
        )
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.slot()),
        (ErrorKind::OutOfRange, Some(70))
    );
    assert!(values[..64].iter().all(|&v| v == (-EPOCH_USECS) as u64));
    // The slots asked before it in its own word, and the NULL slot's bit.
    for slot in [64, 65, 67, 69] {
        assert_eq!(values[slot], (-EPOCH_USECS) as u64, "slot {slot}");
    }
    assert_eq!(nulls[1] >> (66 - 64) & 1, 1);
}

#[test]
fn every_broken_shape_refused() {
    let time = plan(
        &DataType::Int64,
        PgType::Timestamp,
        with_unit(Unit::Seconds),
    );
    let stored = [1i64, 2, 3];
    let short_bitmap: [u8; 0] = [];
    let string = plan(&DataType::Utf8, PgType::Text, OPTIONS);
    let large_string = plan(&DataType::LargeUtf8, PgType::Text, OPTIONS);
    let backward = [0i32, 3, 1];
    let past = [0i32, 2, 9];
    let fixed = plan(&DataType::FixedSizeBinary(4), PgType::Bytea, OPTIONS);
    let boolean = plan(&DataType::Boolean, PgType::Bool, OPTIONS);
    let bits = [0u8];
    let dictionary_plan = plan(
        &dictionary(DataType::Int32, DataType::Utf8),
        PgType::Text,
        OPTIONS,
    );
    let day_dictionary = plan(
        &dictionary(DataType::Int8, DataType::Int32),
        PgType::Date,
        with_unit(Unit::Days),
    );
    let entry_offsets = [0i32, 2];
    let entries = Column {
        len: 1,
        values: Values::Bytes {
            offsets: &entry_offsets,
            data: b"ab",
        },
        nulls: None,
    };
    let strings = |offsets| {
        plain(
            Values::Bytes {
                offsets,
                data: b"abc",
            },
            2,
        )
    };
    let cases: Vec<(&ReadPlan, Input<'_>, Vec<u64>)> = vec![
        // Values shorter than the column.
        (&time, plain(Values::Int64(&stored), 4), every(4)),
        // A bitmap shorter than the column.
        (
            &time,
            Input::Plain(Column {
                len: 3,
                values: Values::Int64(&stored),
                nulls: Some(Bitmap::new(&short_bitmap, 0)),
            }),
            every(3),
        ),
        // A slot asked past the column, and a mask shorter than it.
        (&time, plain(Values::Int64(&stored), 3), vec![0b1000]),
        (&time, plain(Values::Int64(&stored), 3), vec![]),
        // An input of another type than the plan's.
        (&time, plain(Values::Int32(&[1, 2, 3]), 3), every(3)),
        // Offsets that go back, past their bytes, and too few.
        (&string, strings(&backward), every(2)),
        (&string, strings(&past), every(2)),
        (&string, strings(&past[..2]), every(2)),
        // Too few offsets, though the slots asked have theirs, and none.
        (&string, strings(&past[..2]), vec![0b1]),
        (&string, strings(&[]), vec![0]),
        (
            &large_string,
            plain(
                Values::LargeBytes {
                    offsets: &[0, 2],
                    data: b"abc",
                },
                2,
            ),
            vec![0b1],
        ),
        (
            &large_string,
            plain(
                Values::LargeBytes {
                    offsets: &[],
                    data: b"abc",
                },
                2,
            ),
            vec![0],
        ),
        // Fixed-size bytes shorter than the size times the slots, and of
        // another size than the plan's.
        (
            &fixed,
            plain(
                Values::FixedBytes {
                    size: 4,
                    data: &[0; 7],
                },
                2,
            ),
            every(2),
        ),
        (
            &fixed,
            plain(
                Values::FixedBytes {
                    size: 3,
                    data: &[0; 9],
                },
                2,
            ),
            every(2),
        ),
        // A Boolean buffer shorter than its bit offset and the slots.
        (
            &boolean,
            plain(Values::Boolean(Bitmap::new(&bits, 4)), 5),
            every(5),
        ),
        // A dictionary's index outside it, and indexes of another type.
        (
            &dictionary_plan,
            Input::Dictionary {
                len: 1,
                keys: Keys::Int32(&[1]),
                nulls: None,
                values: entries,
            },
            every(1),
        ),
        (
            &dictionary_plan,
            Input::Dictionary {
                len: 1,
                keys: Keys::Int64(&[0]),
                nulls: None,
                values: entries,
            },
            every(1),
        ),
        // A plain column for a plan of a dictionary.
        (&dictionary_plan, Input::Plain(entries), every(1)),
        // An index of a dictionary of days one past it.
        (
            &day_dictionary,
            Input::Dictionary {
                len: 1,
                keys: Keys::Int8(&[1]),
                nulls: None,
                values: Column {
                    len: 1,
                    values: Values::Int32(&[5]),
                    nulls: None,
                },
            },
            every(1),
        ),
    ];
    for (index, (plan, input, asked)) in cases.into_iter().enumerate() {
        let len = input.len();
        let mut nulls = vec![7u64; mask_words(len) + 1];
        if plan.gives_datums() {
            let mut values = vec![7u64; len + 1];
            let mut output = ReadOutput::Datums {
                values: &mut values,
                nulls: &mut nulls,
            };
            let error = plan.read(&input, &asked, &mut output).unwrap_err();
            assert_eq!(
                (error.kind(), error.sqlstate()),
                (ErrorKind::BrokenShape, "XX000"),
                "case {index}"
            );
            assert!(
                values.iter().all(|&v| v == 7),
                "case {index}: output changed"
            );
        } else {
            let mut values: Vec<&[u8]> = vec![b"x"; len + 1];
            let mut output = ReadOutput::Bytes {
                values: &mut values,
                nulls: &mut nulls,
            };
            let error = plan.read(&input, &asked, &mut output).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::BrokenShape, "case {index}");
            assert!(
                values.iter().all(|v| *v == b"x"),
                "case {index}: output changed"
            );
        }
        assert!(nulls.iter().all(|&v| v == 7), "case {index}: nulls changed");
    }
    // Outputs shorter than the column, and of the other form.
    let input = plain(Values::Int64(&stored), 3);
    let mut values = vec![0u64; 2];
    let mut nulls = vec![0u64; 1];
    let mut output = ReadOutput::Datums {
        values: &mut values,
        nulls: &mut nulls,
    };
    let short = time.read(&input, &every(3), &mut output);
    assert_eq!(short.unwrap_err().kind(), ErrorKind::BrokenShape);
    let mut strings: Vec<&[u8]> = vec![&[]; 3];
    let mut output = ReadOutput::Bytes {
        values: &mut strings,
        nulls: &mut nulls,
    };
    let other = time.read(&input, &every(3), &mut output);
    assert_eq!(other.unwrap_err().kind(), ErrorKind::BrokenShape);
}

#[test]
fn an_empty_column() {
    let time = plan(
        &DataType::Int64,
        PgType::Timestamp,
        with_unit(Unit::Seconds),
    );
    assert!(datums(&time, &plain(Values::Int64(&[]), 0), &[]).is_ok());
    let text = plan(&DataType::Utf8, PgType::Text, OPTIONS);
    let input = plain(
        Values::Bytes {
            offsets: &[],
            data: &[],
        },
        0,
    );
    assert!(input.is_empty());
    assert!(bytes(&text, &input, &[]).is_ok());
}

/// The exact count in 128 bits of PostgreSQL's microseconds of `stored`
/// seconds, `None` outside its range.
fn exact_seconds(stored: i64) -> Option<i64> {
    let micros = i128::from(stored) * 1_000_000 - i128::from(EPOCH_USECS);
    (i128::from(MIN_TIMESTAMP)..=i128::from(MAX_TIMESTAMP))
        .contains(&micros)
        .then(|| i64::try_from(micros).unwrap())
}

proptest! {
    /// Columns of seconds of every bit pattern, with random NULL slots
    /// and slots asked: the read gives each slot asked its exact value,
    /// or fails at the first slot asked that is not NULL and out of
    /// range, and never panics.
    #[test]
    fn seconds_of_every_bit_pattern(
        stored in prop::collection::vec(
            prop_oneof![any::<i64>(), -100_000_000_000i64..100_000_000_000],
            0..200,
        ),
        valid in prop::collection::vec(any::<u8>(), 26),
        asked_bits in prop::collection::vec(any::<u64>(), 4),
        offset in 0usize..8,
    ) {
        let len = stored.len();
        let plan = plan(&DataType::Int64, PgType::Timestamp, with_unit(Unit::Seconds));
        let mut asked = asked_bits[..mask_words(len)].to_vec();
        if !len.is_multiple_of(64) && let Some(last) = asked.last_mut() {
            *last &= (1u64 << (len % 64)) - 1;
        }
        let bitmap = Bitmap::new(&valid, offset);
        let input = Input::Plain(Column {
            len,
            values: Values::Int64(&stored),
            nulls: Some(bitmap),
        });
        let result = datums(&plan, &input, &asked);
        let is_asked = |slot: usize| asked[slot / 64] >> (slot % 64) & 1 == 1;
        let first_failure = (0..len)
            .find(|&s| is_asked(s) && bitmap.get(s) && exact_seconds(stored[s]).is_none());
        match (result, first_failure) {
            (Ok((values, nulls)), None) => {
                for slot in 0..len {
                    let null = nulls[slot / 64] >> (slot % 64) & 1 == 1;
                    prop_assert_eq!(null, is_asked(slot) && !bitmap.get(slot));
                    if is_asked(slot) && bitmap.get(slot) {
                        prop_assert_eq!(Some(values[slot] as i64), exact_seconds(stored[slot]));
                    }
                }
            }
            (Err(error), Some(slot)) => {
                prop_assert_eq!((error.kind(), error.slot()), (ErrorKind::OutOfRange, Some(slot)));
            }
            (result, expected) => {
                prop_assert!(false, "{:?} against {:?}", result.map(|_| ()), expected);
            }
        }
    }

    /// Bytes of every pattern into checked text: each slot asked passes
    /// exactly when it is UTF-8 without a zero byte.
    #[test]
    fn bytes_of_every_pattern(
        strings in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..8), 1..20),
    ) {
        let refs: Vec<&[u8]> = strings.iter().map(Vec::as_slice).collect();
        let (offs, data) = offsets(&refs);
        let input = plain(Values::Bytes { offsets: &offs, data: &data }, refs.len());
        let plan = plan(&DataType::Binary, PgType::Text, with_text(TextBytes::Checked));
        for (slot, string) in refs.iter().enumerate() {
            let valid = std::str::from_utf8(string).is_ok() && !string.contains(&0);
            let mut asked = vec![0u64; mask_words(refs.len())];
            asked[slot / 64] |= 1 << (slot % 64);
            let result = bytes(&plan, &input, &asked);
            prop_assert_eq!(result.is_ok(), valid);
            if let Ok((values, _)) = result {
                prop_assert_eq!(values[slot], *string);
            }
        }
    }
}

/// The values of a column of one width, owned, from random bits.
#[derive(Debug, Clone)]
enum Owned {
    Int8(Vec<i8>),
    Int16(Vec<i16>),
    Int32(Vec<i32>),
    Int64(Vec<i64>),
    UInt8(Vec<u8>),
    UInt16(Vec<u16>),
    UInt32(Vec<u32>),
    Float32(Vec<f32>),
    Float64(Vec<f64>),
    Boolean(Vec<u8>, usize, usize),
}

impl Owned {
    fn of(width: usize, bits: &[u64]) -> Owned {
        match width {
            0 => Owned::Int8(bits.iter().map(|&b| b as i8).collect()),
            1 => Owned::Int16(bits.iter().map(|&b| b as i16).collect()),
            2 => Owned::Int32(bits.iter().map(|&b| b as i32).collect()),
            3 => Owned::Int64(bits.iter().map(|&b| b as i64).collect()),
            4 => Owned::UInt8(bits.iter().map(|&b| b as u8).collect()),
            5 => Owned::UInt16(bits.iter().map(|&b| b as u16).collect()),
            6 => Owned::UInt32(bits.iter().map(|&b| b as u32).collect()),
            7 => Owned::Float32(bits.iter().map(|&b| f32::from_bits(b as u32)).collect()),
            8 => Owned::Float64(bits.iter().map(|&b| f64::from_bits(b)).collect()),
            _ => {
                let offset = (bits.first().copied().unwrap_or(0) % 8) as usize;
                let bytes = bits.iter().map(|&b| (b >> 8) as u8).chain([0, 0]).collect();
                Owned::Boolean(bytes, offset, bits.len())
            }
        }
    }

    fn values(&self) -> Values<'_> {
        match self {
            Owned::Int8(v) => Values::Int8(v),
            Owned::Int16(v) => Values::Int16(v),
            Owned::Int32(v) => Values::Int32(v),
            Owned::Int64(v) => Values::Int64(v),
            Owned::UInt8(v) => Values::UInt8(v),
            Owned::UInt16(v) => Values::UInt16(v),
            Owned::UInt32(v) => Values::UInt32(v),
            Owned::Float32(v) => Values::Float32(v),
            Owned::Float64(v) => Values::Float64(v),
            Owned::Boolean(bytes, offset, _) => Values::Boolean(Bitmap::new(bytes, *offset)),
        }
    }

    fn len(&self) -> usize {
        match self {
            Owned::Int8(v) => v.len(),
            Owned::Int16(v) => v.len(),
            Owned::Int32(v) => v.len(),
            Owned::Int64(v) => v.len(),
            Owned::UInt8(v) => v.len(),
            Owned::UInt16(v) => v.len(),
            Owned::UInt32(v) => v.len(),
            Owned::Float32(v) => v.len(),
            Owned::Float64(v) => v.len(),
            Owned::Boolean(_, _, len) => *len,
        }
    }

    /// The value of `slot` as an integer, for the integer widths.
    fn int(&self, slot: usize) -> i128 {
        match self {
            Owned::Int8(v) => i128::from(v[slot]),
            Owned::Int16(v) => i128::from(v[slot]),
            Owned::Int32(v) => i128::from(v[slot]),
            Owned::Int64(v) => i128::from(v[slot]),
            Owned::UInt8(v) => i128::from(v[slot]),
            Owned::UInt16(v) => i128::from(v[slot]),
            Owned::UInt32(v) => i128::from(v[slot]),
            _ => 0,
        }
    }
}

/// A family of read plans of one width, and the code of one value that
/// a read of it must agree with: the representation, or `None` out of
/// range.
#[derive(Debug, Clone, Copy)]
enum Family {
    Int,
    Days,
    Time(i128),
    Float4,
    Float8FromFloat4,
    Float8,
    Bool,
}

fn families() -> Vec<(usize, DataType, PgType, ReadOptions, Family)> {
    use DataType as D;
    let days = with_unit(Unit::Days);
    let mut cases = vec![
        (0, D::Int8, PgType::Int2, OPTIONS, Family::Int),
        (1, D::Int16, PgType::Int4, OPTIONS, Family::Int),
        (2, D::Int32, PgType::Int8, OPTIONS, Family::Int),
        (3, D::Int64, PgType::Int8, OPTIONS, Family::Int),
        (4, D::UInt8, PgType::Int2, OPTIONS, Family::Int),
        (5, D::UInt16, PgType::Int4, OPTIONS, Family::Int),
        (6, D::UInt32, PgType::Int8, OPTIONS, Family::Int),
        (0, D::Int8, PgType::Date, days, Family::Days),
        (1, D::Int16, PgType::Date, days, Family::Days),
        (2, D::Int32, PgType::Date, days, Family::Days),
        (2, D::Date32, PgType::Date, OPTIONS, Family::Days),
        (4, D::UInt8, PgType::Date, days, Family::Days),
        (5, D::UInt16, PgType::Date, days, Family::Days),
        (6, D::UInt32, PgType::Date, days, Family::Days),
        (7, D::Float32, PgType::Float4, OPTIONS, Family::Float4),
        (
            7,
            D::Float32,
            PgType::Float8,
            OPTIONS,
            Family::Float8FromFloat4,
        ),
        (8, D::Float64, PgType::Float8, OPTIONS, Family::Float8),
        (9, D::Boolean, PgType::Bool, OPTIONS, Family::Bool),
    ];
    for (unit, scale) in [
        (Unit::Seconds, 1_000_000),
        (Unit::Milliseconds, 1_000),
        (Unit::Microseconds, 1),
    ] {
        cases.push((
            3,
            D::Int64,
            PgType::Timestamp,
            with_unit(unit),
            Family::Time(scale),
        ));
    }
    for (unit, scale) in [
        (ArrowUnit::Second, 1_000_000),
        (ArrowUnit::Millisecond, 1_000),
        (ArrowUnit::Microsecond, 1),
    ] {
        cases.push((
            3,
            timestamp(unit, Some("UTC")),
            PgType::Timestamptz,
            OPTIONS,
            Family::Time(scale),
        ));
    }
    cases
}

/// The representation of slot `slot` of `owned` by the code of one
/// value, `None` where it is out of range.
fn one_value(family: Family, owned: &Owned, slot: usize) -> Option<u64> {
    let in_range = |v: i128, low: i64, high: i64| (i128::from(low)..=i128::from(high)).contains(&v);
    match (family, owned) {
        (Family::Int, owned) => Some(owned.int(slot) as i64 as u64),
        (Family::Days, owned) => {
            let days = owned.int(slot) - i128::from(EPOCH_DAYS);
            in_range(days, MIN_DATE, MAX_DATE).then_some(i64::from(days as i32) as u64)
        }
        (Family::Time(scale), owned) => {
            let micros = owned.int(slot) * scale - i128::from(EPOCH_USECS);
            in_range(micros, MIN_TIMESTAMP, MAX_TIMESTAMP).then_some(micros as i64 as u64)
        }
        (Family::Float4, Owned::Float32(v)) => Some(i64::from(v[slot].to_bits() as i32) as u64),
        (Family::Float8FromFloat4, Owned::Float32(v)) => Some(f64::from(v[slot]).to_bits()),
        (Family::Float8, Owned::Float64(v)) => Some(v[slot].to_bits()),
        (Family::Bool, Owned::Boolean(bytes, offset, _)) => {
            Some(u64::from(Bitmap::new(bytes, *offset).get(slot)))
        }
        _ => None,
    }
}

proptest! {
    /// Every dense family, over columns of every bit pattern, random NULL
    /// slots from a random bit offset, random slots asked and a filled
    /// output: each slot asked that is not NULL gets the representation
    /// of the code of one value, or the first that fails is reported; a
    /// slot not asked keeps the value it had or gets its true
    /// representation; and the mask of NULL slots is exact.
    #[test]
    fn dense_families_against_the_code_of_one_value(
        family in 0usize..24,
        bits in prop::collection::vec(
            prop_oneof![any::<u64>(), (0u64..30_000), Just(0), Just(u64::MAX)],
            0..160,
        ),
        valid in prop::collection::vec(any::<u8>(), 22),
        offset in 0usize..8,
        asked_bits in prop::collection::vec(any::<u64>(), 3),
        before in any::<u64>(),
    ) {
        let cases = families();
        let (width, data_type, pg, options, family) = cases[family % cases.len()].clone();
        let owned = Owned::of(width, &bits);
        let len = owned.len();
        let plan = plan(&data_type, pg, options);
        let mut asked = asked_bits[..mask_words(len)].to_vec();
        if !len.is_multiple_of(64) && let Some(last) = asked.last_mut() {
            *last &= (1u64 << (len % 64)) - 1;
        }
        let bitmap = Bitmap::new(&valid, offset);
        let input = Input::Plain(Column { len, values: owned.values(), nulls: Some(bitmap) });
        let filled: Vec<u64> = (0..len as u64).map(|i| before ^ i.wrapping_mul(0x9e37_79b9)).collect();
        let mut values = filled.clone();
        let mut nulls = vec![before; mask_words(len)];
        let result = plan.read(&input, &asked, &mut ReadOutput::Datums { values: &mut values, nulls: &mut nulls });
        let is_asked = |slot: usize| asked[slot / 64] >> (slot % 64) & 1 == 1;
        let failure = (0..len).find(|&s| is_asked(s) && bitmap.get(s) && one_value(family, &owned, s).is_none());
        let checked = match (&result, failure) {
            (Ok(()), None) => len,
            (Err(error), Some(slot)) => {
                prop_assert_eq!((error.kind(), error.slot()), (ErrorKind::OutOfRange, Some(slot)));
                slot
            }
            _ => {
                prop_assert!(false, "{:?} against {:?} for {:?}", result, failure, data_type);
                0
            }
        };
        for slot in 0..checked {
            let expected = one_value(family, &owned, slot);
            if is_asked(slot) && bitmap.get(slot) {
                prop_assert_eq!(Some(values[slot]), expected, "slot {}", slot);
            } else if bitmap.get(slot) && expected.is_some() {
                prop_assert!(values[slot] == filled[slot] || Some(values[slot]) == expected, "slot {} not asked", slot);
            }
            if result.is_ok() {
                let null = nulls[slot / 64] >> (slot % 64) & 1 == 1;
                prop_assert_eq!(null, is_asked(slot) && !bitmap.get(slot));
            }
        }
    }

    /// A dictionary of every family against the code of one value of its
    /// values: each slot asked that is not NULL, by its own bitmap or the
    /// dictionary's, gets its value's representation.
    #[test]
    fn dictionaries_against_the_code_of_one_value(
        family in 0usize..24,
        entries in prop::collection::vec(prop_oneof![any::<u64>(), (0u64..30_000)], 1..12),
        entry_valid in any::<u16>(),
        keys in prop::collection::vec(any::<u8>(), 0..100),
        key_valid in prop::collection::vec(any::<u8>(), 13),
        asked_bits in prop::collection::vec(any::<u64>(), 2),
    ) {
        let cases = families();
        let (width, data_type, pg, options, family) = cases[family % cases.len()].clone();
        let owned = Owned::of(width, &entries);
        let entry_count = owned.len();
        let keys: Vec<u16> = keys.iter().map(|&k| u16::from(k) % entry_count as u16).collect();
        let len = keys.len();
        let dictionary_type = dictionary(DataType::UInt16, data_type.clone());
        let plan = plan(&dictionary_type, pg, options);
        let entry_bits = entry_valid.to_le_bytes();
        let entry_bitmap = Bitmap::new(&entry_bits, 0);
        let key_bitmap = Bitmap::new(&key_valid, 0);
        let input = Input::Dictionary {
            len,
            keys: Keys::UInt16(&keys),
            nulls: Some(key_bitmap),
            values: Column { len: entry_count, values: owned.values(), nulls: Some(entry_bitmap) },
        };
        let mut asked = asked_bits[..mask_words(len)].to_vec();
        if !len.is_multiple_of(64) && let Some(last) = asked.last_mut() {
            *last &= (1u64 << (len % 64)) - 1;
        }
        let is_asked = |slot: usize| asked[slot / 64] >> (slot % 64) & 1 == 1;
        let null = |slot: usize| !key_bitmap.get(slot) || !entry_bitmap.get(usize::from(keys[slot]));
        let result = datums(&plan, &input, &asked);
        let failure = (0..len).find(|&s| is_asked(s) && !null(s) && one_value(family, &owned, usize::from(keys[s])).is_none());
        match (result, failure) {
            (Ok((values, nulls)), None) => {
                for slot in (0..len).filter(|&s| is_asked(s)) {
                    let bit = nulls[slot / 64] >> (slot % 64) & 1 == 1;
                    prop_assert_eq!(bit, null(slot));
                    if !null(slot) {
                        prop_assert_eq!(Some(values[slot]), one_value(family, &owned, usize::from(keys[slot])));
                    }
                }
            }
            (Err(error), Some(slot)) => {
                prop_assert_eq!((error.kind(), error.slot()), (ErrorKind::OutOfRange, Some(slot)));
            }
            (result, failure) => prop_assert!(false, "{:?} against {:?}", result.map(|_| ()), failure),
        }
    }
}

#[test]
fn bytes_of_every_layout() {
    // FixedSizeBinary into bytea.
    let fixed = plan(&DataType::FixedSizeBinary(3), PgType::Bytea, OPTIONS);
    let data = *b"abcdefghi";
    let input = plain(
        Values::FixedBytes {
            size: 3,
            data: &data,
        },
        3,
    );
    let (values, _) = bytes(&fixed, &input, &[0b101]).unwrap();
    assert_eq!((values[0], values[2]), (&b"abc"[..], &b"ghi"[..]));
    // LargeUtf8 into text.
    let large = plan(&DataType::LargeUtf8, PgType::Text, OPTIONS);
    let large_offsets = [0i64, 2, 2, 5];
    let input = plain(
        Values::LargeBytes {
            offsets: &large_offsets,
            data: b"abcde",
        },
        3,
    );
    let (values, _) = bytes(&large, &input, &every(3)).unwrap();
    assert_eq!(values, [&b"ab"[..], b"", b"cde"]);
    // A plain Utf8 column whose NULL slot holds bytes that are not UTF-8
    // and a zero byte: the NULL slot gives NULL and does not fail.
    let utf8 = plan(
        &DataType::Binary,
        PgType::Text,
        with_text(TextBytes::Checked),
    );
    let strings: [&[u8]; 3] = [b"ok", b"\xff\0", b"fine"];
    let (offs, data) = offsets(&strings);
    let valid = [0b101u8];
    let input = Input::Plain(Column {
        len: 3,
        values: Values::Bytes {
            offsets: &offs,
            data: &data,
        },
        nulls: Some(Bitmap::new(&valid, 0)),
    });
    let (values, nulls) = bytes(&utf8, &input, &every(3)).unwrap();
    assert_eq!(nulls, [0b010]);
    assert_eq!((values[0], values[2]), (&b"ok"[..], &b"fine"[..]));
    // A dictionary's value NULL by its bitmap gives NULL; its offsets
    // hold, as Arrow's format wants of every slot.
    let entry_offsets = [0i32, 2, 2];
    let entry_valid = [0b01u8];
    let input = Input::Dictionary {
        len: 2,
        keys: Keys::Int8(&[0, 1]),
        nulls: None,
        values: Column {
            len: 2,
            values: Values::Bytes {
                offsets: &entry_offsets,
                data: b"ab",
            },
            nulls: Some(Bitmap::new(&entry_valid, 0)),
        },
    };
    let text = plan(
        &dictionary(DataType::Int8, DataType::Utf8),
        PgType::Text,
        OPTIONS,
    );
    let (values, nulls) = bytes(&text, &input, &every(2)).unwrap();
    assert_eq!((values[0], nulls[0]), (&b"ab"[..], 0b10));
}

#[test]
fn broken_shapes_of_a_dictionary_and_of_the_output() {
    let text = plan(
        &dictionary(DataType::Int8, DataType::Utf8),
        PgType::Text,
        OPTIONS,
    );
    let dictionary_of =
        |len, keys: &'static [i8], nulls: Option<Bitmap<'static>>, offsets: &'static [i32]| {
            Input::Dictionary {
                len,
                keys: Keys::Int8(keys),
                nulls,
                values: Column {
                    len: 2,
                    values: Values::Bytes {
                        offsets,
                        data: b"ab",
                    },
                    nulls: None,
                },
            }
        };
    static OFFSETS: [i32; 3] = [0, 2, 1];
    static KEYS: [i8; 2] = [0, 1];
    let cases = [
        // A bitmap of the dictionary's slots shorter than the column.
        dictionary_of(2, &KEYS, Some(Bitmap::new(&[], 0)), &[0, 1, 2]),
        // Fewer indexes than slots.
        dictionary_of(3, &KEYS, None, &[0, 1, 2]),
        // A referenced value's offsets that go back.
        dictionary_of(2, &KEYS, None, &OFFSETS),
    ];
    for (index, input) in cases.iter().enumerate() {
        let error = bytes(&text, input, &every(input.len())).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BrokenShape, "case {index}");
    }
    // The offsets of a NULL slot asked that go back, plain and in a
    // dictionary's value NULL by its bitmap: the shape is broken, the
    // output unchanged.
    let utf8 = plan(&DataType::Utf8, PgType::Text, OPTIONS);
    let backward = [1i32, 0];
    let valid = [0u8];
    let input = Input::Plain(Column {
        len: 1,
        values: Values::Bytes {
            offsets: &backward,
            data: b"x",
        },
        nulls: Some(Bitmap::new(&valid, 0)),
    });
    let mut values: Vec<&[u8]> = vec![b"x"];
    let mut nulls = vec![7u64];
    let mut output = ReadOutput::Bytes {
        values: &mut values,
        nulls: &mut nulls,
    };
    let error = utf8.read(&input, &[1], &mut output).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BrokenShape);
    assert_eq!((values[0], nulls[0]), (&b"x"[..], 7));
    let entry_valid = [0b01u8];
    let input = Input::Dictionary {
        len: 2,
        keys: Keys::Int8(&KEYS),
        nulls: None,
        values: Column {
            len: 2,
            values: Values::Bytes {
                offsets: &OFFSETS,
                data: b"ab",
            },
            nulls: Some(Bitmap::new(&entry_valid, 0)),
        },
    };
    let error = bytes(&text, &input, &every(2)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BrokenShape);
    // A mask of NULL slots shorter than the column.
    let time = plan(
        &DataType::Int64,
        PgType::Timestamp,
        with_unit(Unit::Seconds),
    );
    let stored = [0i64; 65];
    let input = plain(Values::Int64(&stored), 65);
    let mut values = vec![0u64; 65];
    let mut nulls = vec![0u64; 1];
    let mut output = ReadOutput::Datums {
        values: &mut values,
        nulls: &mut nulls,
    };
    let error = time.read(&input, &every(65), &mut output).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BrokenShape);
}

/// Every Arrow type the crate takes, plain and as a dictionary's values.
fn every_arrow_type() -> Vec<DataType> {
    use DataType as D;
    let mut plain = vec![
        D::Boolean,
        D::Int8,
        D::Int16,
        D::Int32,
        D::Int64,
        D::UInt8,
        D::UInt16,
        D::UInt32,
        D::Float32,
        D::Float64,
        D::Date32,
        D::Utf8,
        D::LargeUtf8,
        D::Binary,
        D::LargeBinary,
        D::FixedSizeBinary(3),
    ];
    for unit in [
        ArrowUnit::Second,
        ArrowUnit::Millisecond,
        ArrowUnit::Microsecond,
    ] {
        plain.push(timestamp(unit, None));
        plain.push(timestamp(unit, Some("UTC")));
    }
    let dictionaries: Vec<DataType> = plain
        .iter()
        .map(|value| dictionary(D::UInt8, value.clone()))
        .collect();
    plain.extend(dictionaries);
    plain
}

/// Every PostgreSQL type the crate knows, `varchar` with and without a
/// length.
fn every_pg_type() -> Vec<PgType> {
    vec![
        PgType::Bool,
        PgType::Int2,
        PgType::Int4,
        PgType::Int8,
        PgType::Float4,
        PgType::Float8,
        PgType::Date,
        PgType::Timestamp,
        PgType::Timestamptz,
        PgType::Text,
        PgType::Varchar(None),
        varchar(3),
        PgType::Bytea,
    ]
}

/// The column of `arrow`'s plain type `plain` made of `bits`, owned.
struct AnyColumn {
    dense: Option<Owned>,
    offsets: Vec<i32>,
    large_offsets: Vec<i64>,
    data: Vec<u8>,
    plain: Plain,
}

impl AnyColumn {
    fn of(plain: Plain, bits: &[u64]) -> AnyColumn {
        let width = match plain {
            Plain::Int8 => Some(0),
            Plain::Int16 => Some(1),
            Plain::Int32 | Plain::Date32 => Some(2),
            Plain::Int64 | Plain::Timestamp { .. } => Some(3),
            Plain::UInt8 => Some(4),
            Plain::UInt16 => Some(5),
            Plain::UInt32 => Some(6),
            Plain::Float32 => Some(7),
            Plain::Float64 => Some(8),
            Plain::Boolean => Some(9),
            _ => None,
        };
        // Strings of up to seven bytes of any value, a word each.
        let mut offsets = vec![0i32];
        let mut data = Vec::new();
        for &word in bits {
            let len = (word % 8) as usize;
            data.extend_from_slice(&word.to_le_bytes()[..len]);
            offsets.push(i32::try_from(data.len()).unwrap());
        }
        if let Plain::FixedSizeBinary(size) = plain {
            data = bits
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .take(size * bits.len())
                .collect();
        }
        AnyColumn {
            dense: width.map(|width| Owned::of(width, bits)),
            large_offsets: offsets.iter().map(|&o| i64::from(o)).collect(),
            offsets,
            data,
            plain,
        }
    }

    fn values(&self) -> Values<'_> {
        match (&self.dense, self.plain) {
            (Some(owned), _) => owned.values(),
            (None, Plain::Utf8 | Plain::Binary) => Values::Bytes {
                offsets: &self.offsets,
                data: &self.data,
            },
            (None, Plain::LargeUtf8 | Plain::LargeBinary) => Values::LargeBytes {
                offsets: &self.large_offsets,
                data: &self.data,
            },
            (None, Plain::FixedSizeBinary(size)) => Values::FixedBytes {
                size,
                data: &self.data,
            },
            (None, plain) => unreachable!("{plain:?}"),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Plans made from every pair of known types with every option, and
    /// reads, bounds and writes through each over slots of every bit
    /// pattern: each call returns its result or an error its plan names,
    /// a plan's refusal one of the kinds of a plan, and none panics.
    #[test]
    fn no_panic_on_any_input(
        bits in prop::collection::vec(
            prop_oneof![any::<u64>(), Just(0), Just(u64::MAX), Just(1 << 63)],
            1..70,
        ),
        valid in prop::collection::vec(any::<u8>(), 10),
        offset in 0usize..8,
        constant in prop_oneof![any::<i64>(), Just(i64::MAX), Just(i64::MIN)],
    ) {
        let len = bits.len();
        let bitmap = Bitmap::new(&valid, offset);
        let units = [None, Some(Unit::Days), Some(Unit::Seconds), Some(Unit::Milliseconds), Some(Unit::Microseconds)];
        let texts = [None, Some(TextBytes::Checked), Some(TextBytes::Trusted)];
        let keys: Vec<u8> = bits.iter().map(|&b| (b % len as u64) as u8).collect();
        let mut accepted = 0;
        for data_type in every_arrow_type() {
            let arrow = ArrowType::new(&data_type).unwrap();
            let column = AnyColumn::of(arrow.values(), &bits);
            let input = match arrow {
                ArrowType::Plain(_) => Input::Plain(Column { len, values: column.values(), nulls: Some(bitmap) }),
                ArrowType::Dictionary { .. } => Input::Dictionary {
                    len,
                    keys: Keys::UInt8(&keys),
                    nulls: Some(bitmap),
                    values: Column { len, values: column.values(), nulls: Some(bitmap) },
                },
            };
            for pg in every_pg_type() {
                for unit in units {
                    for text in texts {
                        for server_utf8 in [true, false] {
                            let options = ReadOptions { unit, text, server_utf8 };
                            let plan = match ReadPlan::new(&data_type, pg, options) {
                                Ok(plan) => plan,
                                Err(error) => {
                                    prop_assert!(matches!(
                                        error.kind(),
                                        ErrorKind::NotSupported | ErrorKind::InvalidOption
                                            | ErrorKind::TypeMismatch | ErrorKind::TooLarge
                                    ), "{:?}", error);
                                    continue;
                                }
                            };
                            accepted += 1;
                            let mut nulls = vec![0u64; mask_words(len)];
                            let result = if plan.gives_datums() {
                                let mut values = vec![0u64; len];
                                plan.read(&input, &every(len), &mut ReadOutput::Datums { values: &mut values, nulls: &mut nulls })
                            } else {
                                let mut values: Vec<&[u8]> = vec![&[]; len];
                                plan.read(&input, &every(len), &mut ReadOutput::Bytes { values: &mut values, nulls: &mut nulls })
                            };
                            if let Err(error) = result {
                                prop_assert!(plan.failures().kinds().any(|kind| kind == error.kind()), "{:?} of {:?} into {:?}", error, data_type, pg);
                            }
                            for op in [Comparison::Eq, Comparison::Ne, Comparison::Lt, Comparison::Le, Comparison::Gt, Comparison::Ge] {
                                // Bounds of integers, days and times alone, never refused.
                                let bounded = matches!(
                                    plan.pg_type(),
                                    PgType::Int2 | PgType::Int4 | PgType::Int8 | PgType::Date
                                        | PgType::Timestamp | PgType::Timestamptz
                                );
                                let test = match plan.test(op, Some(constant)) {
                                    Ok(test) => test,
                                    Err(error) => {
                                        prop_assert!(!bounded && error.kind() == ErrorKind::NotSupported, "{:?}", error);
                                        continue;
                                    }
                                };
                                prop_assert!(bounded);
                                let _ = test.range(constant, constant.wrapping_mul(3));
                                if let Input::Plain(column) = &input {
                                    let mut passed = vec![0u64; mask_words(len)];
                                    prop_assert!(test.apply(column, &mut passed).is_ok());
                                }
                            }
                        }
                    }
                }
            }
        }
        // Every pair the plan's table lists, by every option that applies.
        prop_assert_eq!(accepted, 196);
        for pg in every_pg_type() {
            for server_utf8 in [true, false] {
                let text = matches!(pg, PgType::Text | PgType::Varchar(_));
                let plan = match WritePlan::new(pg, server_utf8) {
                    Ok(plan) => plan,
                    Err(error) => {
                        // Text on a server of another encoding alone is refused.
                        prop_assert!(text && !server_utf8 && error.kind() == ErrorKind::NotSupported, "{:?}", error);
                        continue;
                    }
                };
                let owned: Vec<Vec<u8>> = bits.iter().map(|w| w.to_le_bytes()[..(w % 8) as usize].to_vec()).collect();
                let strings: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
                let nulls: Vec<u64> = (0..mask_words(len)).map(|w| bitmap.word(w * 64, len)).collect();
                let input = match plan.data_type() {
                    DataType::Utf8 | DataType::Binary => WriteInput::Bytes { len, values: &strings, nulls: &nulls },
                    _ => WriteInput::Datums { len, values: &bits, nulls: &nulls },
                };
                let mut validity = vec![0u8; len.div_ceil(8)];
                let result = match plan.data_type() {
                    DataType::Boolean => plan.write(&input, &every(len), &mut WriteOutput::Boolean { values: &mut vec![0u8; len.div_ceil(8)], validity: &mut validity }),
                    DataType::Int16 => plan.write(&input, &every(len), &mut WriteOutput::Int16 { values: &mut vec![0; len], validity: &mut validity }),
                    DataType::Int32 | DataType::Date32 => plan.write(&input, &every(len), &mut WriteOutput::Int32 { values: &mut vec![0; len], validity: &mut validity }),
                    DataType::Int64 | DataType::Timestamp(..) => plan.write(&input, &every(len), &mut WriteOutput::Int64 { values: &mut vec![0; len], validity: &mut validity }),
                    DataType::Float32 => plan.write(&input, &every(len), &mut WriteOutput::Float32 { values: &mut vec![0.0; len], validity: &mut validity }),
                    DataType::Float64 => plan.write(&input, &every(len), &mut WriteOutput::Float64 { values: &mut vec![0.0; len], validity: &mut validity }),
                    _ => plan.write(&input, &every(len), &mut WriteOutput::Bytes { offsets: &mut vec![0; len + 1], data: &mut vec![0; 8 * len], validity: &mut validity }),
                };
                if let Err(error) = result {
                    prop_assert!(plan.failures().kinds().any(|kind| kind == error.kind()), "{:?} of {:?}", error, pg);
                }
            }
        }
    }
}

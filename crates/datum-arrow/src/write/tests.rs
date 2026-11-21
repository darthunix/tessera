use std::sync::Arc;

use arrow_array::types::Int32Type;
use arrow_array::{
    Array, BinaryArray, BooleanArray, Date32Array, DictionaryArray, FixedSizeBinaryArray,
    Float32Array, Float64Array, Int8Array, Int16Array, Int32Array, Int64Array, LargeBinaryArray,
    LargeStringArray, StringArray, TimestampMicrosecondArray, TimestampSecondArray, UInt8Array,
    UInt16Array, UInt32Array,
};
use arrow_buffer::{BooleanBuffer, Buffer, NullBuffer, OffsetBuffer, ScalarBuffer};
use arrow_schema::{DataType, TimeUnit as ArrowUnit};
use proptest::prelude::*;

use super::*;
use crate::column::{Bitmap, Column, Input, Keys, Values};
use crate::pg::VarcharLength;
use crate::read::{ReadOptions, ReadOutput, ReadPlan, Unit};

const OPTIONS: ReadOptions = ReadOptions {
    unit: None,
    text: None,
    server_utf8: true,
};

fn every(len: usize) -> Vec<u64> {
    mask_of(&vec![true; len])
}

fn mask_of(bits: &[bool]) -> Vec<u64> {
    let mut mask = vec![0u64; mask_words(bits.len())];
    for (slot, &bit) in bits.iter().enumerate() {
        if bit {
            mask[slot / 64] |= 1 << (slot % 64);
        }
    }
    mask
}

fn bit(bytes: &[u8], index: usize) -> bool {
    bytes[index / 8] >> (index % 8) & 1 == 1
}

const ALL: [PgType; 12] = [
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
    PgType::Bytea,
];

#[test]
fn types_of_the_columns_written() {
    let expected = [
        (DataType::Boolean, Failures::NONE),
        (DataType::Int16, Failures::NONE),
        (DataType::Int32, Failures::NONE),
        (DataType::Int64, Failures::NONE),
        (DataType::Float32, Failures::NONE),
        (DataType::Float64, Failures::NONE),
        (DataType::Date32, Failures::OUT_OF_RANGE),
        (
            DataType::Timestamp(ArrowUnit::Microsecond, None),
            Failures::OUT_OF_RANGE,
        ),
        (
            DataType::Timestamp(ArrowUnit::Microsecond, Some(Arc::from("UTC"))),
            Failures::OUT_OF_RANGE,
        ),
        (DataType::Utf8, Failures::TOO_LARGE),
        (DataType::Utf8, Failures::TOO_LARGE),
        (DataType::Binary, Failures::TOO_LARGE),
    ];
    for (pg, (data_type, failures)) in ALL.iter().zip(expected) {
        let plan = WritePlan::new(*pg, true).unwrap();
        assert_eq!(
            (plan.data_type(), plan.failures()),
            (data_type, failures),
            "{pg:?}"
        );
        assert_eq!(plan.pg_type(), *pg);
    }
    let varchar = PgType::Varchar(Some(VarcharLength::new(10).unwrap()));
    assert_eq!(
        WritePlan::new(varchar, true).unwrap().data_type(),
        DataType::Utf8
    );
    for pg in [PgType::Text, varchar] {
        let error = WritePlan::new(pg, false).unwrap_err();
        assert_eq!(error.sqlstate(), "0A000");
    }
    assert!(WritePlan::new(PgType::Bytea, false).is_ok());
}

#[test]
fn values_infinities_and_the_last_times_written() {
    // Dates: the first and the last day, one before and one past, the
    // infinities.
    let date = WritePlan::new(PgType::Date, true).unwrap();
    let datum = |d: i64| i64::from(i32::try_from(d).unwrap()) as u64;
    let values: Vec<u64> = [MIN_DATE, MAX_DATE, MIN_DATE - 1, MAX_DATE + 1]
        .iter()
        .map(|&d| datum(d))
        .chain([i32::MAX, i32::MIN].map(|d| i64::from(d) as u64))
        .collect();
    let input = WriteInput::Datums {
        len: 6,
        values: &values,
        nulls: &[0],
    };
    let mut out = vec![9i32; 6];
    let mut validity = vec![0u8; 1];
    let output = &mut WriteOutput::Int32 {
        values: &mut out,
        validity: &mut validity,
    };
    assert_eq!(date.write(&input, &[0b11], output).map(|w| w.len), Ok(2));
    assert_eq!(
        (i64::from(out[0]), i64::from(out[1]), validity[0]),
        (MIN_DATE + EPOCH_DAYS, MAX_DATE + EPOCH_DAYS, 0b11)
    );
    for slot in 2..6 {
        let mut out = vec![9i32; 1];
        let mut validity = vec![0u8; 1];
        let output = &mut WriteOutput::Int32 {
            values: &mut out,
            validity: &mut validity,
        };
        let error = date.write(&input, &[1 << slot], output).unwrap_err();
        assert_eq!(
            (error.kind(), error.slot(), error.message()),
            (ErrorKind::OutOfRange, Some(slot), "date out of range")
        );
    }

    // Times: the first microsecond, one before, the last one an i64 since
    // 1970 holds and the one after it, a time past PostgreSQL's last, the
    // infinities, and a slot not asked that holds infinity.
    for pg in [PgType::Timestamp, PgType::Timestamptz] {
        let time = WritePlan::new(pg, true).unwrap();
        let last = i64::MAX - EPOCH_USECS;
        let times = [
            MIN_TIMESTAMP,
            MIN_TIMESTAMP - 1,
            last,
            last + 1,
            MAX_TIMESTAMP + 1,
            i64::MAX,
            i64::MIN,
            i64::MAX,
        ];
        let values: Vec<u64> = times.iter().map(|&t| t as u64).collect();
        let input = WriteInput::Datums {
            len: 8,
            values: &values,
            nulls: &[0],
        };
        let mut out = vec![9i64; 8];
        let mut validity = vec![0xffu8; 1];
        let written = time
            .write(
                &input,
                &[0b0000_0101],
                &mut WriteOutput::Int64 {
                    values: &mut out,
                    validity: &mut validity,
                },
            )
            .unwrap();
        assert_eq!(written, Written { len: 2, bytes: 0 });
        assert_eq!(out[..2], [MIN_TIMESTAMP + EPOCH_USECS, i64::MAX]);
        assert_eq!(validity[0], 0b11);
        for slot in [1, 3, 4, 5, 6] {
            let mut out = vec![9i64; 1];
            let mut validity = vec![0u8; 1];
            let output = &mut WriteOutput::Int64 {
                values: &mut out,
                validity: &mut validity,
            };
            let error = time.write(&input, &[1 << slot], output).unwrap_err();
            assert_eq!(
                (error.kind(), error.slot()),
                (ErrorKind::OutOfRange, Some(slot))
            );
        }
    }

    // Booleans of 0, 1 and 2, packed from the first bit, with a slot not
    // asked between them.
    let boolean = WritePlan::new(PgType::Bool, true).unwrap();
    let values = [0u64, 7, 1, 2];
    let input = WriteInput::Datums {
        len: 4,
        values: &values,
        nulls: &[0],
    };
    let mut bits = vec![0xffu8; 1];
    let mut validity = vec![0u8; 1];
    let written = boolean
        .write(
            &input,
            &[0b1101],
            &mut WriteOutput::Boolean {
                values: &mut bits,
                validity: &mut validity,
            },
        )
        .unwrap();
    assert_eq!(written.len, 3);
    assert_eq!((bits[0], validity[0]), (0b110, 0b111));
}

#[test]
fn the_values_before_a_failure_are_written() {
    // A slot not asked and a NULL slot asked before the failing time: the
    // two values asked before it written, packed, the NULL's bit clear.
    let time = WritePlan::new(PgType::Timestamp, true).unwrap();
    let values = [0u64, 1, i64::MIN as u64, 2, i64::MAX as u64, 3];
    let nulls = [0b00_0100u64];
    let input = WriteInput::Datums {
        len: 6,
        values: &values,
        nulls: &nulls,
    };
    let mut out = vec![9i64; 6];
    let mut validity = vec![0xffu8; 1];
    let error = time
        .write(
            &input,
            &[0b11_1101],
            &mut WriteOutput::Int64 {
                values: &mut out,
                validity: &mut validity,
            },
        )
        .unwrap_err();
    assert_eq!(error.slot(), Some(4));
    assert_eq!(out[0], EPOCH_USECS);
    assert_eq!((out[1], out[2]), (0, 2 + EPOCH_USECS));
    assert_eq!(validity[0] & 0b111, 0b101);
    // The same of a date.
    let date = WritePlan::new(PgType::Date, true).unwrap();
    let values = [0u64, 1, 0, 2, i64::from(i32::MAX) as u64];
    let input = WriteInput::Datums {
        len: 5,
        values: &values,
        nulls: &nulls,
    };
    let mut out = vec![9i32; 5];
    let mut validity = vec![0xffu8; 1];
    let error = date
        .write(
            &input,
            &[0b1_1101],
            &mut WriteOutput::Int32 {
                values: &mut out,
                validity: &mut validity,
            },
        )
        .unwrap_err();
    assert_eq!(error.slot(), Some(4));
    assert_eq!(out[..3], [10_957, 0, 10_959]);
    assert_eq!(validity[0] & 0b111, 0b101);
}

#[test]
fn bytes_past_the_offsets_fail_before_anything_is_written() {
    // Three values of 1 073 741 819 bytes: the first two fit the 32-bit
    // offsets, the third passes them, into a buffer of bytes too short.
    let big = vec![0u8; 1_073_741_819];
    let values: [&[u8]; 3] = [&big, &big, &big];
    let bytea = WritePlan::new(PgType::Bytea, true).unwrap();
    let input = WriteInput::Bytes {
        len: 3,
        values: &values,
        nulls: &[0],
    };
    let mut offsets = vec![7i32; 4];
    let mut data = vec![7u8; 10];
    let mut validity = vec![7u8; 1];
    let mut output = WriteOutput::Bytes {
        offsets: &mut offsets,
        data: &mut data,
        validity: &mut validity,
    };
    let error = bytea.write(&input, &[0b111], &mut output).unwrap_err();
    assert_eq!(
        (error.kind(), error.slot(), error.sqlstate()),
        (ErrorKind::TooLarge, Some(2), "54000")
    );
    // The shape of the input is checked first: a slot asked past the
    // column gives XX000, though the values pass the size too.
    let error = bytea.write(&input, &[0b1111], &mut output).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BrokenShape);
    assert!(offsets.iter().all(|&o| o == 7) && data.iter().all(|&b| b == 7));
    assert_eq!(validity, [7]);
}

/// A write of each broken shape refused, every output buffer unchanged.
#[test]
fn broken_shapes_of_a_write() {
    let int = WritePlan::new(PgType::Int4, true).unwrap();
    let boolean = WritePlan::new(PgType::Bool, true).unwrap();
    let text = WritePlan::new(PgType::Text, true).unwrap();
    let values = [1u64, 2, 3];
    let strings: [&[u8]; 2] = [b"ab", b"c"];
    let datums = |values: &'static [u64], nulls: &'static [u64]| WriteInput::Datums {
        len: 3,
        values,
        nulls,
    };
    let fine = WriteInput::Datums {
        len: 3,
        values: &values,
        nulls: &[0],
    };
    let bytes = WriteInput::Bytes {
        len: 2,
        values: &strings,
        nulls: &[0],
    };
    // Inputs of a broken shape.
    let inputs: Vec<(&WritePlan, WriteInput<'_>, Vec<u64>)> = vec![
        (&int, datums(&[1, 2], &[0]), every(3)),
        (&int, datums(&[1, 2, 3], &[]), every(3)),
        (&int, fine, vec![0b1000]),
        (&int, fine, vec![]),
        (&int, bytes, every(2)),
        (&text, fine, every(3)),
        (
            &text,
            WriteInput::Bytes {
                len: 3,
                values: &strings,
                nulls: &[0],
            },
            every(3),
        ),
    ];
    for (index, (plan, input, asked)) in inputs.into_iter().enumerate() {
        let mut out = vec![7i32; 4];
        let mut validity = vec![7u8; 1];
        let mut output = WriteOutput::Int32 {
            values: &mut out,
            validity: &mut validity,
        };
        let error = plan.write(&input, &asked, &mut output).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BrokenShape, "input case {index}");
        assert!(
            out.iter().all(|&v| v == 7) && validity == [7],
            "input case {index}"
        );
    }
    // Outputs too short in each buffer, or of another type.
    let check = |plan: &WritePlan, input: &WriteInput<'_>, output: &mut WriteOutput<'_>| {
        let error = plan.write(input, &every(input.len()), output).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BrokenShape);
    };
    let mut short_values = vec![7i32; 2];
    let mut wide_values = vec![7i32; 3];
    let mut short_validity: Vec<u8> = vec![];
    let mut validity = vec![7u8; 1];
    check(
        &int,
        &fine,
        &mut WriteOutput::Int32 {
            values: &mut short_values,
            validity: &mut validity,
        },
    );
    check(
        &int,
        &fine,
        &mut WriteOutput::Int32 {
            values: &mut wide_values,
            validity: &mut short_validity,
        },
    );
    let mut other = vec![7i64; 3];
    check(
        &int,
        &fine,
        &mut WriteOutput::Int64 {
            values: &mut other,
            validity: &mut validity,
        },
    );
    let mut no_bits: Vec<u8> = vec![];
    check(
        &boolean,
        &fine,
        &mut WriteOutput::Boolean {
            values: &mut no_bits,
            validity: &mut validity,
        },
    );
    let mut bits = vec![7u8; 1];
    check(
        &boolean,
        &fine,
        &mut WriteOutput::Boolean {
            values: &mut bits,
            validity: &mut short_validity,
        },
    );
    let mut offsets = vec![7i32; 3];
    let mut short_offsets = vec![7i32; 2];
    let mut data = vec![7u8; 3];
    let mut short_data = vec![7u8; 2];
    check(
        &text,
        &bytes,
        &mut WriteOutput::Bytes {
            offsets: &mut short_offsets,
            data: &mut data,
            validity: &mut validity,
        },
    );
    check(
        &text,
        &bytes,
        &mut WriteOutput::Bytes {
            offsets: &mut offsets,
            data: &mut short_data,
            validity: &mut validity,
        },
    );
    check(
        &text,
        &bytes,
        &mut WriteOutput::Bytes {
            offsets: &mut offsets,
            data: &mut data,
            validity: &mut short_validity,
        },
    );
    assert!(short_values.iter().chain(&wide_values).all(|&v| v == 7));
    assert!(other.iter().all(|&v| v == 7) && validity == [7] && bits == [7]);
    assert!(offsets.iter().chain(&short_offsets).all(|&o| o == 7));
    assert!(data.iter().chain(&short_data).all(|&b| b == 7));
}

/// The Arrow value the write gives a representation of `pg`, as Arrow's
/// array of the type written shows it: an integer, the bits of a float,
/// or a boolean as 0 and 1.
fn arrow_value(pg: PgType, datum: u64) -> i128 {
    match pg {
        PgType::Bool => i128::from(datum != 0),
        PgType::Int2 => i128::from(datum as i16),
        PgType::Int4 => i128::from(datum as i32),
        PgType::Int8 => i128::from(datum as i64),
        PgType::Float4 => i128::from((datum as u32) as i32),
        PgType::Float8 => i128::from(datum as i64),
        PgType::Date => i128::from(datum as i32) + i128::from(EPOCH_DAYS),
        _ => i128::from(datum as i64) + i128::from(EPOCH_USECS),
    }
}

/// A column of `pg` written for the slots `asked`, packed, into buffers
/// that start filled; checked against Arrow's own arrays of the type
/// written; and read back through the read plan of that type.
fn round_trip(pg: PgType, values: &[u64], strings: &[Vec<u8>], nulls: &[bool], asked: &[bool]) {
    let len = nulls.len();
    let plan = WritePlan::new(pg, true).unwrap();
    let null_mask = mask_of(nulls);
    let asked_mask = mask_of(asked);
    let order: Vec<usize> = (0..len).filter(|&s| asked[s]).collect();
    let count = order.len();
    let mut validity = vec![0xa5u8; count.div_ceil(8) + 1];
    let reader = ReadPlan::new(&plan.data_type(), pg, OPTIONS).unwrap();
    let mut read_nulls = vec![0u64; mask_words(count)];
    let validity_of = |validity: &[u8]| {
        NullBuffer::new(BooleanBuffer::new(
            Buffer::from(validity.to_vec()),
            0,
            count,
        ))
    };
    if matches!(plan.data_type(), DataType::Utf8 | DataType::Binary) {
        let refs: Vec<&[u8]> = strings.iter().map(Vec::as_slice).collect();
        let input = WriteInput::Bytes {
            len,
            values: &refs,
            nulls: &null_mask,
        };
        let total: usize = order
            .iter()
            .filter(|&&s| !nulls[s])
            .map(|&s| refs[s].len())
            .sum();
        let mut offsets = vec![-1i32; count + 2];
        let mut data = vec![0xa5u8; total + 3];
        let written = plan
            .write(
                &input,
                &asked_mask,
                &mut WriteOutput::Bytes {
                    offsets: &mut offsets,
                    data: &mut data,
                    validity: &mut validity,
                },
            )
            .unwrap();
        assert_eq!(
            written,
            Written {
                len: count,
                bytes: total
            }
        );
        // The places past what was written are left as they were.
        assert_eq!(offsets[count + 1], -1);
        assert_eq!(data[total..], [0xa5; 3]);
        offsets.truncate(count + 1);
        data.truncate(total);
        // Arrow takes the arrays as valid, UTF-8 checked for Utf8, and
        // shows the values and NULLs written.
        let offset_buffer = OffsetBuffer::new(ScalarBuffer::from(offsets.clone()));
        let array: Arc<dyn Array> = if plan.data_type() == DataType::Utf8 {
            Arc::new(
                StringArray::try_new(
                    offset_buffer,
                    Buffer::from(data.clone()),
                    Some(validity_of(&validity)),
                )
                .unwrap(),
            )
        } else {
            Arc::new(
                BinaryArray::try_new(
                    offset_buffer,
                    Buffer::from(data.clone()),
                    Some(validity_of(&validity)),
                )
                .unwrap(),
            )
        };
        for (place, &slot) in order.iter().enumerate() {
            assert_eq!(array.is_null(place), nulls[slot], "{pg:?} place {place}");
            if !nulls[slot] {
                let shown: &[u8] = match array.as_any().downcast_ref::<StringArray>() {
                    Some(strings) => strings.value(place).as_bytes(),
                    None => array
                        .as_any()
                        .downcast_ref::<BinaryArray>()
                        .unwrap()
                        .value(place),
                };
                assert_eq!(shown, refs[slot], "{pg:?} place {place}");
            }
        }
        let input = Input::Plain(Column {
            len: count,
            values: Values::Bytes {
                offsets: &offsets,
                data: &data,
            },
            nulls: Some(Bitmap::new(&validity, 0)),
        });
        let mut back: Vec<&[u8]> = vec![&[]; count];
        reader
            .read(
                &input,
                &every(count),
                &mut ReadOutput::Bytes {
                    values: &mut back,
                    nulls: &mut read_nulls,
                },
            )
            .unwrap();
        for (place, &slot) in order.iter().enumerate() {
            assert_eq!(read_nulls[place / 64] >> (place % 64) & 1 == 1, nulls[slot]);
            if !nulls[slot] {
                assert_eq!(back[place], refs[slot], "{pg:?} slot {slot}");
            }
        }
        return;
    }
    let input = WriteInput::Datums {
        len,
        values,
        nulls: &null_mask,
    };
    let mut back = vec![0u64; count];
    let shown: Vec<Option<i128>>;
    // Each type's Arrow buffer, Arrow's array of it, then the read back.
    match plan.data_type() {
        DataType::Boolean => {
            let mut bits = vec![0xa5u8; count.div_ceil(8) + 1];
            let output = &mut WriteOutput::Boolean {
                values: &mut bits,
                validity: &mut validity,
            };
            assert_eq!(plan.write(&input, &asked_mask, output).unwrap().len, count);
            let array = BooleanArray::new(
                BooleanBuffer::new(Buffer::from(bits.clone()), 0, count),
                Some(validity_of(&validity)),
            );
            shown = (0..count)
                .map(|p| array.is_valid(p).then(|| i128::from(array.value(p))))
                .collect();
            let input = Input::Plain(Column {
                len: count,
                values: Values::Boolean(Bitmap::new(&bits, 0)),
                nulls: Some(Bitmap::new(&validity, 0)),
            });
            let output = &mut ReadOutput::Datums {
                values: &mut back,
                nulls: &mut read_nulls,
            };
            reader.read(&input, &every(count), output).unwrap();
        }
        DataType::Int16 => {
            let mut out = vec![-7i16; count + 1];
            let output = &mut WriteOutput::Int16 {
                values: &mut out,
                validity: &mut validity,
            };
            plan.write(&input, &asked_mask, output).unwrap();
            assert_eq!(out[count], -7);
            let array = Int16Array::try_new(
                ScalarBuffer::from(out[..count].to_vec()),
                Some(validity_of(&validity)),
            )
            .unwrap();
            shown = (0..count)
                .map(|p| array.is_valid(p).then(|| i128::from(array.value(p))))
                .collect();
            let input = Input::Plain(Column {
                len: count,
                values: Values::Int16(&out),
                nulls: Some(Bitmap::new(&validity, 0)),
            });
            let output = &mut ReadOutput::Datums {
                values: &mut back,
                nulls: &mut read_nulls,
            };
            reader.read(&input, &every(count), output).unwrap();
        }
        DataType::Int32 | DataType::Date32 => {
            let mut out = vec![-7i32; count + 1];
            let output = &mut WriteOutput::Int32 {
                values: &mut out,
                validity: &mut validity,
            };
            plan.write(&input, &asked_mask, output).unwrap();
            assert_eq!(out[count], -7);
            let values = ScalarBuffer::from(out[..count].to_vec());
            shown = if plan.data_type() == DataType::Date32 {
                let array = Date32Array::try_new(values, Some(validity_of(&validity))).unwrap();
                (0..count)
                    .map(|p| array.is_valid(p).then(|| i128::from(array.value(p))))
                    .collect()
            } else {
                let array = Int32Array::try_new(values, Some(validity_of(&validity))).unwrap();
                (0..count)
                    .map(|p| array.is_valid(p).then(|| i128::from(array.value(p))))
                    .collect()
            };
            let input = Input::Plain(Column {
                len: count,
                values: Values::Int32(&out),
                nulls: Some(Bitmap::new(&validity, 0)),
            });
            let output = &mut ReadOutput::Datums {
                values: &mut back,
                nulls: &mut read_nulls,
            };
            reader.read(&input, &every(count), output).unwrap();
        }
        DataType::Int64 | DataType::Timestamp(..) => {
            let mut out = vec![-7i64; count + 1];
            let output = &mut WriteOutput::Int64 {
                values: &mut out,
                validity: &mut validity,
            };
            plan.write(&input, &asked_mask, output).unwrap();
            assert_eq!(out[count], -7);
            let values = ScalarBuffer::from(out[..count].to_vec());
            shown = if plan.data_type() == DataType::Int64 {
                let array = Int64Array::try_new(values, Some(validity_of(&validity))).unwrap();
                (0..count)
                    .map(|p| array.is_valid(p).then(|| i128::from(array.value(p))))
                    .collect()
            } else {
                let array =
                    TimestampMicrosecondArray::try_new(values, Some(validity_of(&validity)))
                        .unwrap()
                        .with_data_type(plan.data_type());
                assert_eq!(array.data_type(), &plan.data_type());
                (0..count)
                    .map(|p| array.is_valid(p).then(|| i128::from(array.value(p))))
                    .collect()
            };
            let input = Input::Plain(Column {
                len: count,
                values: Values::Int64(&out),
                nulls: Some(Bitmap::new(&validity, 0)),
            });
            let output = &mut ReadOutput::Datums {
                values: &mut back,
                nulls: &mut read_nulls,
            };
            reader.read(&input, &every(count), output).unwrap();
        }
        DataType::Float32 => {
            let mut out = vec![-7f32; count + 1];
            let output = &mut WriteOutput::Float32 {
                values: &mut out,
                validity: &mut validity,
            };
            plan.write(&input, &asked_mask, output).unwrap();
            let array = Float32Array::try_new(
                ScalarBuffer::from(out[..count].to_vec()),
                Some(validity_of(&validity)),
            )
            .unwrap();
            shown = (0..count)
                .map(|p| {
                    array
                        .is_valid(p)
                        .then(|| i128::from(array.value(p).to_bits() as i32))
                })
                .collect();
            let input = Input::Plain(Column {
                len: count,
                values: Values::Float32(&out),
                nulls: Some(Bitmap::new(&validity, 0)),
            });
            let output = &mut ReadOutput::Datums {
                values: &mut back,
                nulls: &mut read_nulls,
            };
            reader.read(&input, &every(count), output).unwrap();
        }
        DataType::Float64 => {
            let mut out = vec![-7f64; count + 1];
            let output = &mut WriteOutput::Float64 {
                values: &mut out,
                validity: &mut validity,
            };
            plan.write(&input, &asked_mask, output).unwrap();
            let array = Float64Array::try_new(
                ScalarBuffer::from(out[..count].to_vec()),
                Some(validity_of(&validity)),
            )
            .unwrap();
            shown = (0..count)
                .map(|p| {
                    array
                        .is_valid(p)
                        .then(|| i128::from(array.value(p).to_bits() as i64))
                })
                .collect();
            let input = Input::Plain(Column {
                len: count,
                values: Values::Float64(&out),
                nulls: Some(Bitmap::new(&validity, 0)),
            });
            let output = &mut ReadOutput::Datums {
                values: &mut back,
                nulls: &mut read_nulls,
            };
            reader.read(&input, &every(count), output).unwrap();
        }
        other => panic!("{other:?}"),
    }
    // The bits past the count in the last byte of validity are cleared.
    if !count.is_multiple_of(8) {
        assert_eq!(validity[count / 8] >> (count % 8), 0);
    }
    for (place, &slot) in order.iter().enumerate() {
        let expected = (!nulls[slot]).then(|| arrow_value(pg, values[slot]));
        assert_eq!(shown[place], expected, "{pg:?} slot {slot}: Arrow's value");
        assert_eq!(bit(&validity, place), !nulls[slot]);
        assert_eq!(read_nulls[place / 64] >> (place % 64) & 1 == 1, nulls[slot]);
        if !nulls[slot] {
            assert_eq!(back[place], values[slot], "{pg:?} slot {slot}: read back");
        }
    }
}

/// A representation of `pg`, valid for its type, from random bits.
fn representation(pg: PgType, bits: u64) -> u64 {
    let span = |low: i64, high: i64| {
        let width = i128::from(high) - i128::from(low) + 1;
        i64::try_from(i128::from(low) + i128::from(bits) % width).unwrap()
    };
    match pg {
        PgType::Bool => bits & 1,
        PgType::Int2 => i64::from(bits as i16) as u64,
        PgType::Int4 | PgType::Float4 => i64::from(bits as i32) as u64,
        PgType::Date => span(MIN_DATE, MAX_DATE) as u64,
        PgType::Timestamp | PgType::Timestamptz => span(MIN_TIMESTAMP, LAST_WRITTEN_TIME) as u64,
        _ => bits,
    }
}

proptest! {
    /// Every type written for slots asked at random, NULL slots among
    /// them and others not asked, across a word's edge: packed in the
    /// order of the slots asked, Arrow takes each array as valid and
    /// shows the values written, and the read plan of the type written
    /// gives back each representation bit for bit, NaN and the edges of
    /// the ranges among them.
    #[test]
    fn every_type_written_and_read_back(
        bits in prop::collection::vec(any::<u64>(), 0..140),
        nulls in prop::collection::vec(any::<bool>(), 140),
        asked in prop::collection::vec(prop::bool::weighted(0.7), 140),
        strings in prop::collection::vec("[^\0]{0,6}", 140),
    ) {
        let len = bits.len();
        let (nulls, asked) = (&nulls[..len], &asked[..len]);
        for pg in ALL {
            let mut values: Vec<u64> = bits.iter().map(|&b| representation(pg, b)).collect();
            match pg {
                PgType::Date if len >= 2 => {
                    values[0] = MIN_DATE as u64;
                    values[1] = MAX_DATE as u64;
                }
                PgType::Timestamp | PgType::Timestamptz if len >= 2 => {
                    values[0] = MIN_TIMESTAMP as u64;
                    values[1] = LAST_WRITTEN_TIME as u64;
                }
                PgType::Float8 if len >= 1 => values[0] = f64::NAN.to_bits(),
                PgType::Float4 if len >= 1 => {
                    values[0] = i64::from(f32::NAN.to_bits() as i32) as u64;
                }
                _ => {}
            }
            let strings: Vec<Vec<u8>> = strings[..len].iter().map(|s| s.as_bytes().to_vec()).collect();
            round_trip(pg, &values, &strings, nulls, asked);
        }
    }
}

fn nulls_of(array: &dyn Array) -> Option<Bitmap<'_>> {
    array
        .nulls()
        .map(|n| Bitmap::new(n.buffer().as_slice(), n.offset()))
}

/// Reads every slot of `input` through a plan of `data_type` into `pg`,
/// and checks each against Arrow's own view: NULL where Arrow's array is,
/// the expected representation elsewhere.
fn read_against(
    data_type: &DataType,
    pg: PgType,
    options: ReadOptions,
    input: &Input<'_>,
    array: &dyn Array,
    expected: impl Fn(usize) -> u64,
) {
    let plan = ReadPlan::new(data_type, pg, options).unwrap();
    let len = input.len();
    let mut values = vec![0u64; len];
    let mut nulls = vec![0u64; mask_words(len)];
    let output = &mut ReadOutput::Datums {
        values: &mut values,
        nulls: &mut nulls,
    };
    plan.read(input, &every(len), output).unwrap();
    for (slot, value) in values.iter().enumerate() {
        assert_eq!(
            nulls[slot / 64] >> (slot % 64) & 1 == 1,
            array.is_null(slot),
            "{data_type:?} {slot}"
        );
        if array.is_valid(slot) {
            assert_eq!(*value, expected(slot), "{data_type:?} slot {slot}");
        }
    }
}

/// Arrays Arrow builds of every type the crate takes, sliced from past a
/// byte of their bitmaps and not, with and without a bitmap, read
/// through the crate's views of their buffers.
#[test]
fn arrow_s_own_arrays() {
    // Slices that start in the second byte of a bitmap, and whole arrays.
    let cuts = [(0, 20), (9, 7), (13, 0)];
    macro_rules! primitive {
        ($array:ty, $variant:ident, $data_type:expr, $pg:expr, $options:expr, $datum:expr) => {
            let with = <$array>::from_iter((0..20).map(|i| (i % 3 != 1).then(|| (i * 37) as _)));
            let without = <$array>::from_iter_values((0..20).map(|i| (i * 53) as _));
            for whole in [&with, &without] {
                for (offset, length) in cuts {
                    let array = whole.slice(offset, length);
                    let input = Input::Plain(Column {
                        len: length,
                        values: Values::$variant(array.values()),
                        nulls: nulls_of(&array),
                    });
                    read_against(&$data_type, $pg, $options, &input, &array, |s| {
                        $datum(array.value(s))
                    });
                }
            }
        };
    }
    let days = ReadOptions {
        unit: Some(Unit::Days),
        ..OPTIONS
    };
    primitive!(
        Int8Array,
        Int8,
        DataType::Int8,
        PgType::Int2,
        OPTIONS,
        |v: i8| i64::from(v) as u64
    );
    primitive!(
        Int16Array,
        Int16,
        DataType::Int16,
        PgType::Int2,
        OPTIONS,
        |v: i16| i64::from(v) as u64
    );
    primitive!(
        Int32Array,
        Int32,
        DataType::Int32,
        PgType::Int4,
        OPTIONS,
        |v: i32| i64::from(v) as u64
    );
    primitive!(
        Int64Array,
        Int64,
        DataType::Int64,
        PgType::Int8,
        OPTIONS,
        |v: i64| v as u64
    );
    primitive!(
        UInt8Array,
        UInt8,
        DataType::UInt8,
        PgType::Int2,
        OPTIONS,
        |v: u8| u64::from(v)
    );
    primitive!(
        UInt16Array,
        UInt16,
        DataType::UInt16,
        PgType::Date,
        days,
        |v: u16| { i64::from(i32::from(v) - EPOCH_DAYS as i32) as u64 }
    );
    primitive!(
        UInt32Array,
        UInt32,
        DataType::UInt32,
        PgType::Int8,
        OPTIONS,
        |v: u32| u64::from(v)
    );
    primitive!(
        Date32Array,
        Int32,
        DataType::Date32,
        PgType::Date,
        OPTIONS,
        |v: i32| { i64::from(v - EPOCH_DAYS as i32) as u64 }
    );
    primitive!(
        TimestampSecondArray,
        Int64,
        DataType::Timestamp(ArrowUnit::Second, None),
        PgType::Timestamp,
        OPTIONS,
        |v: i64| (v * 1_000_000 - EPOCH_USECS) as u64
    );
    primitive!(
        Float32Array,
        Float32,
        DataType::Float32,
        PgType::Float4,
        OPTIONS,
        |v: f32| { i64::from(v.to_bits() as i32) as u64 }
    );
    primitive!(
        Float64Array,
        Float64,
        DataType::Float64,
        PgType::Float8,
        OPTIONS,
        |v: f64| v.to_bits()
    );

    // Booleans, sliced from past a byte, with and without a bitmap.
    let with = BooleanArray::from_iter((0..20).map(|i| (i % 3 != 1).then_some(i % 2 == 0)));
    let without = BooleanArray::from_iter((0..20).map(|i| Some(i % 5 == 0)));
    for whole in [&with, &without] {
        for (offset, length) in cuts {
            let array = whole.slice(offset, length);
            let bits = array.values();
            let input = Input::Plain(Column {
                len: length,
                values: Values::Boolean(Bitmap::new(bits.inner().as_slice(), bits.offset())),
                nulls: nulls_of(&array),
            });
            read_against(
                &DataType::Boolean,
                PgType::Bool,
                OPTIONS,
                &input,
                &array,
                |s| u64::from(array.value(s)),
            );
        }
    }

    // Strings and bytes of every layout, sliced and not, with and without
    // a bitmap.
    let words: Vec<String> = (0..20).map(|i| "дом".repeat(i % 4)).collect();
    let binary_plan = |data_type: DataType, pg| ReadPlan::new(&data_type, pg, OPTIONS).unwrap();
    let read_bytes = |plan: &ReadPlan,
                      input: &Input<'_>,
                      array: &dyn Array,
                      expected: &dyn Fn(usize) -> Vec<u8>| {
        let len = input.len();
        let mut values: Vec<&[u8]> = vec![&[]; len];
        let mut nulls = vec![0u64; mask_words(len)];
        let output = &mut ReadOutput::Bytes {
            values: &mut values,
            nulls: &mut nulls,
        };
        plan.read(input, &every(len), output).unwrap();
        for slot in 0..len {
            assert_eq!(
                nulls[slot / 64] >> (slot % 64) & 1 == 1,
                array.is_null(slot)
            );
            if array.is_valid(slot) {
                assert_eq!(values[slot], expected(slot).as_slice());
            }
        }
    };
    let maybe = |i: usize| (i % 3 != 1).then(|| words[i].clone());
    let small = [
        StringArray::from_iter((0..20).map(maybe)),
        StringArray::from_iter_values(words.iter()),
    ];
    let large = [
        LargeStringArray::from_iter((0..20).map(maybe)),
        LargeStringArray::from_iter_values(words.iter()),
    ];
    let binary = [
        BinaryArray::from_iter((0..20).map(|i| maybe(i).map(String::into_bytes))),
        BinaryArray::from_iter_values(words.iter().map(String::as_bytes)),
    ];
    let large_binary = [
        LargeBinaryArray::from_iter((0..20).map(|i| maybe(i).map(String::into_bytes))),
        LargeBinaryArray::from_iter_values(words.iter().map(String::as_bytes)),
    ];
    let fixed = [
        FixedSizeBinaryArray::try_from_sparse_iter_with_size(
            (0..20).map(|i: u8| (i % 3 != 1).then_some([i, i ^ 0xff])),
            2,
        )
        .unwrap(),
        FixedSizeBinaryArray::try_from_iter((0..20u8).map(|i| [i, i])).unwrap(),
    ];
    for (offset, length) in cuts {
        for whole in &small {
            let array = whole.slice(offset, length);
            let input = Input::Plain(Column {
                len: length,
                values: Values::Bytes {
                    offsets: array.value_offsets(),
                    data: array.values(),
                },
                nulls: nulls_of(&array),
            });
            read_bytes(
                &binary_plan(DataType::Utf8, PgType::Text),
                &input,
                &array,
                &|s| array.value(s).as_bytes().to_vec(),
            );
        }
        for whole in &large {
            let array = whole.slice(offset, length);
            let input = Input::Plain(Column {
                len: length,
                values: Values::LargeBytes {
                    offsets: array.value_offsets(),
                    data: array.values(),
                },
                nulls: nulls_of(&array),
            });
            read_bytes(
                &binary_plan(DataType::LargeUtf8, PgType::Text),
                &input,
                &array,
                &|s| array.value(s).as_bytes().to_vec(),
            );
        }
        for whole in &binary {
            let array = whole.slice(offset, length);
            let input = Input::Plain(Column {
                len: length,
                values: Values::Bytes {
                    offsets: array.value_offsets(),
                    data: array.values(),
                },
                nulls: nulls_of(&array),
            });
            read_bytes(
                &binary_plan(DataType::Binary, PgType::Bytea),
                &input,
                &array,
                &|s| array.value(s).to_vec(),
            );
        }
        for whole in &large_binary {
            let array = whole.slice(offset, length);
            let input = Input::Plain(Column {
                len: length,
                values: Values::LargeBytes {
                    offsets: array.value_offsets(),
                    data: array.values(),
                },
                nulls: nulls_of(&array),
            });
            read_bytes(
                &binary_plan(DataType::LargeBinary, PgType::Bytea),
                &input,
                &array,
                &|s| array.value(s).to_vec(),
            );
        }
        for whole in &fixed {
            let array = whole.slice(offset, length);
            // A sliced FixedSizeBinaryArray's buffer starts at its first slot.
            let data = &array.value_data()[..2 * length];
            let input = Input::Plain(Column {
                len: length,
                values: Values::FixedBytes { size: 2, data },
                nulls: nulls_of(&array),
            });
            read_bytes(
                &binary_plan(DataType::FixedSizeBinary(2), PgType::Bytea),
                &input,
                &array,
                &|s| array.value(s).to_vec(),
            );
        }
    }

    // A dictionary of strings, its keys with a bitmap.
    let keys = Int32Array::from(vec![Some(1), None, Some(0), Some(2)]);
    let dictionary: DictionaryArray<Int32Type> =
        DictionaryArray::try_new(keys, Arc::new(StringArray::from(vec!["x", "yy", "zzz"])))
            .unwrap();
    let plan = binary_plan(dictionary.data_type().clone(), PgType::Text);
    let entries = dictionary
        .values()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let input = Input::Dictionary {
        len: 4,
        keys: Keys::Int32(dictionary.keys().values()),
        nulls: nulls_of(dictionary.keys()),
        values: Column {
            len: 3,
            values: Values::Bytes {
                offsets: entries.value_offsets(),
                data: entries.values(),
            },
            nulls: None,
        },
    };
    let mut values: Vec<&[u8]> = vec![&[]; 4];
    let mut nulls = vec![0u64; 1];
    let output = &mut ReadOutput::Bytes {
        values: &mut values,
        nulls: &mut nulls,
    };
    plan.read(&input, &every(4), output).unwrap();
    assert_eq!(nulls, [0b0010]);
    assert_eq!(
        (values[0], values[2], values[3]),
        (&b"yy"[..], &b"x"[..], &b"zzz"[..])
    );
}

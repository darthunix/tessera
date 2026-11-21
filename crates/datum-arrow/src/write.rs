//! Write plans: the Arrow type a PostgreSQL column becomes, and the
//! conversion of a column's representations into Arrow's buffers.

use std::sync::Arc;

use arrow_schema::{DataType, TimeUnit as ArrowUnit};

use crate::calendar::{EPOCH_DAYS, EPOCH_USECS, MAX_DATE, MAX_TIMESTAMP, MIN_DATE, MIN_TIMESTAMP};
use crate::column::{mask_fits, mask_words};
use crate::error::{Error, ErrorKind, Result};
use crate::failures::Failures;
use crate::pg::PgType;

/// The most bytes the strings of one call write: what the 32-bit offsets
/// of `Utf8` and `Binary` hold.
pub const MAX_WRITE_BYTES: usize = i32::MAX as usize;

/// How a PostgreSQL value becomes an Arrow value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Conv {
    Bool,
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Date,
    Time,
    Bytes,
}

/// A write plan: the Arrow type a PostgreSQL column becomes, made once
/// for a column, before any value is written.
///
/// | PostgreSQL type    | Arrow type                         | Input, output |
/// |--------------------|------------------------------------|---------------|
/// | `boolean`          | `Boolean`                          | `Datums`, `Boolean` |
/// | `smallint`         | `Int16`                            | `Datums`, `Int16` |
/// | `integer`          | `Int32`                            | `Datums`, `Int32` |
/// | `bigint`           | `Int64`                            | `Datums`, `Int64` |
/// | `real`             | `Float32`                          | `Datums`, `Float32` |
/// | `double precision` | `Float64`                          | `Datums`, `Float64` |
/// | `date`             | `Date32`                           | `Datums`, `Int32` |
/// | `timestamp`        | `Timestamp` in microseconds, no time zone | `Datums`, `Int64` |
/// | `timestamptz`      | `Timestamp` in microseconds, `UTC` | `Datums`, `Int64` |
/// | `text`, `varchar`  | `Utf8`                             | `Bytes`, `Bytes` |
/// | `bytea`            | `Binary`                           | `Bytes`, `Bytes` |
///
/// The failures it names: [`Failures::OUT_OF_RANGE`] for `date`,
/// `timestamp` and `timestamptz` (an infinity, which Arrow cannot hold;
/// a representation outside PostgreSQL's range; and, for the times, a
/// time past the last microsecond since 1970 that an `i64` holds, in
/// PostgreSQL's last thirty years); [`Failures::TOO_LARGE`] for `text`,
/// `varchar` and `bytea`, the bytes of one call past
/// [`MAX_WRITE_BYTES`]; none for the others. The caller's
/// representations of `boolean`, `text`, `varchar` and `bytea` are taken
/// as values of their types: a `boolean` word other than 0 is true, and
/// a text is written as its bytes, unchecked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WritePlan {
    pg: PgType,
    conv: Conv,
}

impl WritePlan {
    /// The plan of a column of `pg`, on a server whose encoding is UTF-8
    /// where `server_utf8` is true.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotSupported`] for `text` or `varchar` on a server
    /// whose encoding is not UTF-8.
    pub fn new(pg: PgType, server_utf8: bool) -> Result<Self> {
        let conv = match pg {
            PgType::Bool => Conv::Bool,
            PgType::Int2 => Conv::Int2,
            PgType::Int4 => Conv::Int4,
            PgType::Int8 => Conv::Int8,
            PgType::Float4 => Conv::Float4,
            PgType::Float8 => Conv::Float8,
            PgType::Date => Conv::Date,
            PgType::Timestamp | PgType::Timestamptz => Conv::Time,
            PgType::Text | PgType::Varchar(_) => {
                if !server_utf8 {
                    return Err(Error::new(
                        ErrorKind::NotSupported,
                        "text written from a server whose encoding is not UTF-8",
                    ));
                }
                Conv::Bytes
            }
            PgType::Bytea => Conv::Bytes,
        };
        Ok(WritePlan { pg, conv })
    }

    /// The plan's PostgreSQL type.
    pub const fn pg_type(&self) -> PgType {
        self.pg
    }

    /// The Arrow type the column becomes; the caller gives its field a
    /// name and says whether it holds `NULL`.
    pub fn data_type(&self) -> DataType {
        match self.pg {
            PgType::Bool => DataType::Boolean,
            PgType::Int2 => DataType::Int16,
            PgType::Int4 => DataType::Int32,
            PgType::Int8 => DataType::Int64,
            PgType::Float4 => DataType::Float32,
            PgType::Float8 => DataType::Float64,
            PgType::Date => DataType::Date32,
            PgType::Timestamp => DataType::Timestamp(ArrowUnit::Microsecond, None),
            PgType::Timestamptz => {
                DataType::Timestamp(ArrowUnit::Microsecond, Some(Arc::from("UTC")))
            }
            PgType::Text | PgType::Varchar(_) => DataType::Utf8,
            PgType::Bytea => DataType::Binary,
        }
    }

    /// The failures a value of the plan may meet.
    pub const fn failures(&self) -> Failures {
        match self.conv {
            Conv::Date | Conv::Time => Failures::OUT_OF_RANGE,
            Conv::Bytes => Failures::TOO_LARGE,
            _ => Failures::NONE,
        }
    }

    /// Writes the slots `asked` of `input` into `output`, one Arrow value
    /// a slot asked, in their order: the values packed from the first
    /// place of the output's buffers, booleans and the bitmap of validity
    /// from their first bit. A `NULL` slot gets a clear bit of validity,
    /// a zero value and an empty string; it never fails. Returns the count
    /// of values written and, for strings and bytes, of their bytes.
    ///
    /// `asked`, like the input's mask of `NULL` slots, is a mask of 64-bit
    /// words, bit `i % 64` of word `i / 64` for slot `i`, of at least
    /// [`mask_words`](crate::mask_words) words of the column's slots, with
    /// no bit set past the column. For `count` slots asked and `bytes`
    /// bytes of them that are not `NULL`, the output needs at least
    /// `count` values, `count + 1` offsets, `bytes` bytes of data, and
    /// `count.div_ceil(8)` bytes of booleans and of validity; a longer
    /// buffer is taken, its places past what is written left as they were,
    /// save the bits past `count` in the last byte of booleans and of
    /// validity, which are cleared.
    ///
    /// # Errors
    ///
    /// In three steps, each before the next:
    ///
    /// 1. [`ErrorKind::BrokenShape`] for an input of another form than
    ///    the plan's, buffers shorter than the column, or a mask that asks
    ///    past it;
    /// 2. [`ErrorKind::TooLarge`] where the bytes of the slots asked pass
    ///    [`MAX_WRITE_BYTES`], at the first slot whose bytes pass them;
    /// 3. [`ErrorKind::BrokenShape`] for output buffers shorter than the
    ///    values written: these three steps change no output.
    ///
    /// Then [`ErrorKind::OutOfRange`] for a date or a time the plan names,
    /// at the first slot asked that fails: the values of the slots asked
    /// before it are written, and the rest of the output has no meaning.
    pub fn write(
        &self,
        input: &WriteInput<'_>,
        asked: &[u64],
        output: &mut WriteOutput<'_>,
    ) -> Result<Written> {
        let broken = |message| Err(Error::new(ErrorKind::BrokenShape, message));
        let len = input.len();
        let nulls = input.nulls();
        if !mask_fits(asked, len) || nulls.len() < mask_words(len) {
            return broken("a mask or a mask of NULL slots shorter than the column");
        }
        let count: usize = asked.iter().map(|w| w.count_ones() as usize).sum();
        let bytes = match (input, self.conv) {
            (WriteInput::Datums { values, .. }, conv) if conv != Conv::Bytes => {
                if values.len() < len {
                    return broken("representations shorter than the column");
                }
                0
            }
            (WriteInput::Bytes { values, nulls, .. }, Conv::Bytes) => {
                if values.len() < len {
                    return broken("representations shorter than the column");
                }
                let mut total = 0usize;
                for slot in slots(asked, len) {
                    if is_null(nulls, slot) {
                        continue;
                    }
                    total += values[slot].len();
                    if total > MAX_WRITE_BYTES {
                        return Err(Error::new(
                            ErrorKind::TooLarge,
                            "values of one call past what Arrow's 32-bit offsets hold",
                        )
                        .at(slot));
                    }
                }
                total
            }
            _ => return broken("an input of another form than the plan's"),
        };
        let validity_bytes = count.div_ceil(8);
        let fits = match (&*output, self.conv) {
            (WriteOutput::Boolean { values, validity }, Conv::Bool) => {
                values.len() >= validity_bytes && validity.len() >= validity_bytes
            }
            (WriteOutput::Int16 { values, validity }, Conv::Int2) => {
                values.len() >= count && validity.len() >= validity_bytes
            }
            (WriteOutput::Int32 { values, validity }, Conv::Int4 | Conv::Date) => {
                values.len() >= count && validity.len() >= validity_bytes
            }
            (WriteOutput::Int64 { values, validity }, Conv::Int8 | Conv::Time) => {
                values.len() >= count && validity.len() >= validity_bytes
            }
            (WriteOutput::Float32 { values, validity }, Conv::Float4) => {
                values.len() >= count && validity.len() >= validity_bytes
            }
            (WriteOutput::Float64 { values, validity }, Conv::Float8) => {
                values.len() >= count && validity.len() >= validity_bytes
            }
            (
                WriteOutput::Bytes {
                    offsets,
                    data,
                    validity,
                },
                Conv::Bytes,
            ) => offsets.len() > count && data.len() >= bytes && validity.len() >= validity_bytes,
            _ => return broken("an output of another type than the plan's"),
        };
        if !fits {
            return broken("an output shorter than the values written");
        }
        self.convert(input, asked, output, count)?;
        Ok(Written { len: count, bytes })
    }

    fn convert(
        &self,
        input: &WriteInput<'_>,
        asked: &[u64],
        output: &mut WriteOutput<'_>,
        count: usize,
    ) -> Result<()> {
        let len = input.len();
        let nulls = input.nulls();
        match (input, output) {
            (
                WriteInput::Bytes { values, .. },
                WriteOutput::Bytes {
                    offsets,
                    data,
                    validity,
                },
            ) => {
                let mut end = 0usize;
                offsets[0] = 0;
                for (place, slot) in slots(asked, len).enumerate() {
                    let null = is_null(nulls, slot);
                    set_bit(validity, place, !null);
                    if !null {
                        let bytes = values[slot];
                        data[end..end + bytes.len()].copy_from_slice(bytes);
                        end += bytes.len();
                    }
                    // The size's check bounds the bytes by i32::MAX.
                    offsets[place + 1] = end as i32;
                }
                clear_tail(validity, count);
                Ok(())
            }
            (WriteInput::Datums { values, .. }, output) => {
                let input = *values;
                for (place, slot) in slots(asked, len).enumerate() {
                    let null = is_null(nulls, slot);
                    let datum = if null { 0 } else { input[slot] };
                    let written = match output {
                        WriteOutput::Boolean { values, validity } => {
                            set_bit(values, place, datum != 0);
                            set_bit(validity, place, !null);
                            Ok(())
                        }
                        WriteOutput::Int16 { values, validity } => {
                            values[place] = datum as i16;
                            set_bit(validity, place, !null);
                            Ok(())
                        }
                        WriteOutput::Int32 { values, validity } => {
                            let value = if self.conv == Conv::Date && !null {
                                date_of(datum as i32)
                            } else {
                                Ok(datum as i32)
                            };
                            value.map(|value| {
                                values[place] = value;
                                set_bit(validity, place, !null);
                            })
                        }
                        WriteOutput::Int64 { values, validity } => {
                            let value = if self.conv == Conv::Time && !null {
                                time_of(datum as i64)
                            } else {
                                Ok(datum as i64)
                            };
                            value.map(|value| {
                                values[place] = value;
                                set_bit(validity, place, !null);
                            })
                        }
                        WriteOutput::Float32 { values, validity } => {
                            values[place] = f32::from_bits(datum as u32);
                            set_bit(validity, place, !null);
                            Ok(())
                        }
                        WriteOutput::Float64 { values, validity } => {
                            values[place] = f64::from_bits(datum);
                            set_bit(validity, place, !null);
                            Ok(())
                        }
                        WriteOutput::Bytes { .. } => Ok(()),
                    };
                    written.map_err(|e| e.at(slot))?;
                }
                match output {
                    WriteOutput::Boolean { values, validity } => {
                        clear_tail(values, count);
                        clear_tail(validity, count);
                    }
                    WriteOutput::Int16 { validity, .. }
                    | WriteOutput::Int32 { validity, .. }
                    | WriteOutput::Int64 { validity, .. }
                    | WriteOutput::Float32 { validity, .. }
                    | WriteOutput::Float64 { validity, .. }
                    | WriteOutput::Bytes { validity, .. } => clear_tail(validity, count),
                }
                Ok(())
            }
            _ => Err(Error::new(
                ErrorKind::BrokenShape,
                "an input of another form than the plan's",
            )),
        }
    }
}

/// What a write wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Written {
    /// The Arrow values written, one a slot asked.
    pub len: usize,
    /// The bytes of the strings or bytes written; 0 for other types.
    pub bytes: usize,
}

/// The representations a write takes: a PostgreSQL column's values, a
/// mask of its `NULL` slots, a bit a slot set where the slot is `NULL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteInput<'a> {
    /// The `Datum` words of a type passed by value, as PostgreSQL's
    /// functions make them: a `smallint`, an `integer`, a `date` and the
    /// 32 bits of a `real` taken from the word's low bits, a `boolean` true
    /// where the word is not 0, the others as the whole word; of a `NULL`
    /// slot, any word.
    Datums {
        /// The count of slots.
        len: usize,
        /// A word a slot.
        values: &'a [u64],
        /// The `NULL` slots.
        nulls: &'a [u64],
    },
    /// The bytes of `text`, `varchar` or `bytea` values, without their
    /// header of length; of a `NULL` slot, any slice.
    Bytes {
        /// The count of slots.
        len: usize,
        /// A slice of bytes a slot.
        values: &'a [&'a [u8]],
        /// The `NULL` slots.
        nulls: &'a [u64],
    },
}

impl WriteInput<'_> {
    /// The count of slots.
    pub const fn len(&self) -> usize {
        match self {
            WriteInput::Datums { len, .. } | WriteInput::Bytes { len, .. } => *len,
        }
    }

    /// Whether the column has no slot.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    const fn nulls(&self) -> &[u64] {
        match self {
            WriteInput::Datums { nulls, .. } | WriteInput::Bytes { nulls, .. } => nulls,
        }
    }
}

/// The Arrow buffers a write fills, the caller's: the values and the
/// bitmap of validity, a set bit where a value is not `NULL`, packed from
/// their first place and bit.
#[derive(Debug)]
pub enum WriteOutput<'o> {
    /// `Boolean`: a bit a value, from the first bit.
    Boolean {
        /// The bits of the values.
        values: &'o mut [u8],
        /// The bitmap of validity.
        validity: &'o mut [u8],
    },
    /// `Int16`, of `smallint`.
    Int16 {
        /// The values.
        values: &'o mut [i16],
        /// The bitmap of validity.
        validity: &'o mut [u8],
    },
    /// `Int32`, of `integer`, and `Date32`, of `date`.
    Int32 {
        /// The values.
        values: &'o mut [i32],
        /// The bitmap of validity.
        validity: &'o mut [u8],
    },
    /// `Int64`, of `bigint`, and `Timestamp`, of the times.
    Int64 {
        /// The values.
        values: &'o mut [i64],
        /// The bitmap of validity.
        validity: &'o mut [u8],
    },
    /// `Float32`, of `real`.
    Float32 {
        /// The values.
        values: &'o mut [f32],
        /// The bitmap of validity.
        validity: &'o mut [u8],
    },
    /// `Float64`, of `double precision`.
    Float64 {
        /// The values.
        values: &'o mut [f64],
        /// The bitmap of validity.
        validity: &'o mut [u8],
    },
    /// `Utf8` and `Binary`: the offsets, one more than the values, and
    /// the bytes.
    Bytes {
        /// The offsets.
        offsets: &'o mut [i32],
        /// The bytes.
        data: &'o mut [u8],
        /// The bitmap of validity.
        validity: &'o mut [u8],
    },
}

/// The slots asked, in order.
fn slots(asked: &[u64], len: usize) -> impl Iterator<Item = usize> + '_ {
    asked
        .iter()
        .take(mask_words(len))
        .enumerate()
        .flat_map(|(word, &bits)| {
            let mut bits = bits;
            core::iter::from_fn(move || {
                (bits != 0).then(|| {
                    let slot = word * 64 + bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    slot
                })
            })
        })
}

fn is_null(nulls: &[u64], slot: usize) -> bool {
    nulls[slot / 64] >> (slot % 64) & 1 == 1
}

fn set_bit(bytes: &mut [u8], bit: usize, value: bool) {
    let mask = 1 << (bit % 8);
    if value {
        bytes[bit / 8] |= mask;
    } else {
        bytes[bit / 8] &= !mask;
    }
}

/// Clears the bits past `count` in the last byte a write used.
fn clear_tail(bytes: &mut [u8], count: usize) {
    if !count.is_multiple_of(8) {
        bytes[count / 8] &= (1u8 << (count % 8)) - 1;
    }
}

/// A PostgreSQL date as days since 1970, if Arrow holds it: no infinity
/// and nothing outside PostgreSQL's range.
fn date_of(days: i32) -> Result<i32> {
    let days = i64::from(days);
    if (MIN_DATE..=MAX_DATE).contains(&days) {
        // Within PostgreSQL's range, days since 1970 fit an i32.
        Ok((days + EPOCH_DAYS) as i32)
    } else {
        Err(Error::new(ErrorKind::OutOfRange, "date out of range"))
    }
}

/// The last microsecond since 2000 whose count since 1970 an `i64`
/// holds.
const LAST_WRITTEN_TIME: i64 = {
    let last = i64::MAX - EPOCH_USECS;
    if last < MAX_TIMESTAMP {
        last
    } else {
        MAX_TIMESTAMP
    }
};

/// A PostgreSQL time as microseconds since 1970, if Arrow holds it.
fn time_of(micros: i64) -> Result<i64> {
    if (MIN_TIMESTAMP..=LAST_WRITTEN_TIME).contains(&micros) {
        Ok(micros + EPOCH_USECS)
    } else {
        Err(Error::new(ErrorKind::OutOfRange, "timestamp out of range"))
    }
}

#[cfg(test)]
mod tests;

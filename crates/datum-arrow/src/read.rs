//! Read plans: how the values of an Arrow column become values of a
//! PostgreSQL type, and the conversion of a column's slots through one.

use arrow_schema::DataType;

use crate::arrow::{ArrowType, Key, Plain, TimeUnit};
use crate::calendar::Count;
use crate::column::{Bitmap, Column, Input, Keys, Values, mask_fits, mask_words};
use crate::error::{Error, ErrorKind, Result};
use crate::failures::Failures;
use crate::pg::PgType;

/// The largest value PostgreSQL holds, in bytes, without the header of
/// length of 4 bytes a caller adds: `MaxAllocSize` less the header.
pub const MAX_VALUE_BYTES: usize = 0x3fff_ffff - 4;

/// What a bare integer counts, the option `unit` of a read plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Unit {
    /// Days since 1970-01-01, for an integer of at most 32 bits read
    /// into `date`.
    Days,
    /// Seconds since 1970-01-01 00:00:00, for an `Int64` read into
    /// `timestamp` or `timestamptz`.
    Seconds,
    /// Milliseconds since 1970-01-01 00:00:00, as [`Unit::Seconds`].
    Milliseconds,
    /// Microseconds since 1970-01-01 00:00:00, as [`Unit::Seconds`].
    Microseconds,
}

/// How bytes of `Binary`, `LargeBinary` or `BinaryView` are read as
/// text, into `text` or `varchar`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextBytes {
    /// Each value made is checked as UTF-8 without a zero byte.
    Checked,
    /// No value is checked for UTF-8 or a zero byte: whoever made the
    /// column promises it. Taken into `text` and `varchar` without a
    /// length only, since a length is counted in characters.
    Trusted,
}

/// The options of a read plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReadOptions {
    /// What a bare integer counts; `None` for an integer read as one, and
    /// for every type that is not a bare integer.
    pub unit: Option<Unit>,
    /// How bytes are read as text; `None` for bytes read into `bytea`,
    /// and for every type that is not bytes.
    pub text: Option<TextBytes>,
    /// Whether the server's encoding is UTF-8, which a plan into `text`
    /// or `varchar` needs.
    pub server_utf8: bool,
}

/// How one value becomes a PostgreSQL value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Conv {
    /// A bit into a `boolean`.
    Bool,
    /// An integer widened.
    Int,
    /// A `Float32` into a `real`.
    Float4,
    /// A `Float32` widened into a `double precision`.
    Float8FromFloat4,
    /// A `Float64` into a `double precision`.
    Float8,
    /// Days or a unit of time, shifted and scaled, in PostgreSQL's range.
    Count(Count),
    /// Bytes or a string into `text` or `varchar`.
    Text {
        utf8: bool,
        zero: bool,
        chars: Option<u32>,
    },
    /// Bytes into `bytea`.
    Bytea,
}

impl Conv {
    const fn fixed(self) -> bool {
        !matches!(self, Conv::Text { .. } | Conv::Bytea)
    }
}

/// A read plan: how the values of an Arrow column become values of a
/// PostgreSQL type, made once for a column, before any value is read.
///
/// [`ReadPlan::new`] refuses a pair of types it cannot convert;
/// [`ReadPlan::failures`] names every way one of its values may still
/// fail, and [`ReadPlan::read`] fails in no other way, save for a call of
/// a broken shape.
///
/// It accepts these pairs and no other:
///
/// | Arrow type                                   | PostgreSQL types |
/// |----------------------------------------------|------------------|
/// | `Boolean`                                    | `boolean` |
/// | `Int8`, `Int16`, `UInt8`                     | `smallint`, `integer`, `bigint` |
/// | `Int32`, `UInt16`                            | `integer`, `bigint` |
/// | `Int64`, `UInt32`                            | `bigint` |
/// | `Float32`                                    | `real`, `double precision` |
/// | `Float64`                                    | `double precision` |
/// | `Date32`; an integer of at most 32 bits with [`Unit::Days`] | `date` |
/// | `Timestamp` without a time zone              | `timestamp` |
/// | `Timestamp` with a time zone                 | `timestamptz` |
/// | `Int64` with a unit of time                  | `timestamp`, `timestamptz` |
/// | `Utf8`, `LargeUtf8`, `Utf8View`              | `text`, `varchar` |
/// | `Binary`, `LargeBinary`, `BinaryView`        | `bytea` |
/// | the same, [`TextBytes::Checked`]             | `text`, `varchar` |
/// | the same, [`TextBytes::Trusted`]             | `text`, `varchar` without a length |
/// | `FixedSizeBinary`                            | `bytea` |
/// | a `Dictionary` of one of these               | as its values |
///
/// The failures it names, only where a value of its Arrow type can
/// meet them:
///
/// - [`Failures::OUT_OF_RANGE`]: days in 32 bits, and every 64-bit count
///   of time; days in 8 or 16 bits always fit;
/// - [`Failures::ZERO_BYTE`]: strings and checked bytes into text;
///   Arrow's format requires `Utf8` to be UTF-8, which the crate takes
///   so, unchecked: a caller that cannot vouch for a column's bytes
///   reads them as `Binary`, checked, and a `Utf8` value that is not
///   UTF-8 gives a result of no promise, but no panic;
/// - [`Failures::NOT_UTF8`]: checked bytes;
/// - [`Failures::TOO_LONG`]: strings and checked bytes into a `varchar`
///   with a length, counted in characters; a string longer by spaces
///   alone fails too, which PostgreSQL's input would cut;
/// - [`Failures::TOO_LARGE`]: strings and bytes of variable length, past
///   [`MAX_VALUE_BYTES`].
///
/// The ranges are PostgreSQL's: a `date` from -2 451 545 to
/// 2 145 031 948 days since 2000-01-01, from 4714-11-24 BC to
/// 5874897-12-31 of the Gregorian calendar; a `timestamp` and a
/// `timestamptz` from -211 813 488 000 000 000 to 9 223 371 331 199 999 999
/// microseconds since 2000-01-01 00:00:00, from 4714-11-24 BC to the end
/// of 294276 AD. A count of seconds or milliseconds is exact over every
/// value an `i64` holds: no rounding decides its range.
///
/// Where a value of strings or bytes meets several failures, its size is
/// checked first, then its UTF-8 and its zero byte, then a `varchar`'s
/// length, as PostgreSQL checks a string's encoding before a length;
/// only the first is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReadPlan {
    arrow: ArrowType,
    pg: PgType,
    conv: Conv,
    failures: Failures,
}

fn refuse(kind: ErrorKind, message: &'static str) -> Error {
    Error::new(kind, message)
}

impl ReadPlan {
    /// The plan of an Arrow column of `arrow` read into `pg`, with
    /// `options`.
    ///
    /// # Errors
    ///
    /// In this order:
    ///
    /// 1. [`ErrorKind::NotSupported`] for an Arrow type outside the
    ///    crate's set ([`ArrowType::new`]);
    /// 2. [`ErrorKind::InvalidOption`] for an option where it does not
    ///    apply, on the Arrow type or a dictionary's values: the unit of
    ///    days on anything but an integer of at most 32 bits, a unit of
    ///    time on anything but an `Int64`, a treatment of text on
    ///    anything but `Binary`, `LargeBinary` or `BinaryView` read into
    ///    `text` or `varchar`;
    /// 3. [`ErrorKind::TypeMismatch`] for a pair the table of
    ///    [`ReadPlan`] does not list, as the unit reads it;
    /// 4. [`ErrorKind::TooLarge`] for a `FixedSizeBinary` larger than
    ///    [`MAX_VALUE_BYTES`];
    /// 5. [`ErrorKind::NotSupported`] for a plan into `text` or `varchar`
    ///    on a server whose encoding is not UTF-8.
    pub fn new(arrow: &DataType, pg: PgType, options: ReadOptions) -> Result<Self> {
        let arrow = ArrowType::new(arrow)?;
        let value = arrow.values();
        let is_text = matches!(pg, PgType::Text | PgType::Varchar(_));
        let is_bytes = matches!(
            value,
            Plain::Binary | Plain::LargeBinary | Plain::BinaryView
        );
        let unit = match options.unit {
            None => None,
            Some(Unit::Days) => {
                if !matches!(
                    value,
                    Plain::Int8
                        | Plain::Int16
                        | Plain::Int32
                        | Plain::UInt8
                        | Plain::UInt16
                        | Plain::UInt32
                ) {
                    return Err(refuse(
                        ErrorKind::InvalidOption,
                        "a unit of days on a type that is not an integer of at most 32 bits",
                    ));
                }
                Some(Unit::Days)
            }
            Some(unit) => {
                if value != Plain::Int64 {
                    return Err(refuse(
                        ErrorKind::InvalidOption,
                        "a unit of time on a type that is not a 64-bit integer",
                    ));
                }
                Some(unit)
            }
        };
        if options.text.is_some() && !(is_bytes && is_text) {
            return Err(refuse(
                ErrorKind::InvalidOption,
                "a treatment of text on a type that is not bytes read as text",
            ));
        }
        let mismatch = || {
            Err(refuse(
                ErrorKind::TypeMismatch,
                "a pair of types the crate does not convert",
            ))
        };
        let (conv, failures) = match (value, unit, pg) {
            (Plain::Boolean, None, PgType::Bool) => (Conv::Bool, Failures::NONE),
            (plain, None, pg @ (PgType::Int2 | PgType::Int4 | PgType::Int8)) => {
                // The bits a target holds, against the bits a source needs
                // signed.
                let needed = match plain {
                    Plain::Int8 | Plain::UInt8 => 16,
                    Plain::Int16 => 16,
                    Plain::Int32 | Plain::UInt16 => 32,
                    Plain::Int64 | Plain::UInt32 => 64,
                    _ => return mismatch(),
                };
                let held = match pg {
                    PgType::Int2 => 16,
                    PgType::Int4 => 32,
                    _ => 64,
                };
                if needed > held {
                    return mismatch();
                }
                (Conv::Int, Failures::NONE)
            }
            (Plain::Float32, None, PgType::Float4) => (Conv::Float4, Failures::NONE),
            (Plain::Float32, None, PgType::Float8) => (Conv::Float8FromFloat4, Failures::NONE),
            (Plain::Float64, None, PgType::Float8) => (Conv::Float8, Failures::NONE),
            (Plain::Date32, None, PgType::Date)
            | (Plain::Int32 | Plain::UInt32, Some(Unit::Days), PgType::Date) => {
                (Conv::Count(Count::DAYS), Failures::OUT_OF_RANGE)
            }
            (
                Plain::Int8 | Plain::Int16 | Plain::UInt8 | Plain::UInt16,
                Some(Unit::Days),
                PgType::Date,
            ) => (Conv::Count(Count::DAYS), Failures::NONE),
            (Plain::Timestamp { unit, utc }, None, PgType::Timestamp | PgType::Timestamptz) => {
                if utc != (pg == PgType::Timestamptz) {
                    return mismatch();
                }
                let per_second = match unit {
                    TimeUnit::Second => 1,
                    TimeUnit::Millisecond => 1_000,
                    TimeUnit::Microsecond => 1_000_000,
                };
                (Conv::Count(Count::time(per_second)), Failures::OUT_OF_RANGE)
            }
            (
                Plain::Int64,
                Some(unit @ (Unit::Seconds | Unit::Milliseconds | Unit::Microseconds)),
                PgType::Timestamp | PgType::Timestamptz,
            ) => {
                let per_second = match unit {
                    Unit::Seconds => 1,
                    Unit::Milliseconds => 1_000,
                    _ => 1_000_000,
                };
                (Conv::Count(Count::time(per_second)), Failures::OUT_OF_RANGE)
            }
            (
                Plain::Utf8 | Plain::LargeUtf8 | Plain::Utf8View,
                None,
                PgType::Text | PgType::Varchar(_),
            ) => {
                let chars = varchar_chars(pg);
                let mut failures = Failures::ZERO_BYTE | Failures::TOO_LARGE;
                if chars.is_some() {
                    failures = failures | Failures::TOO_LONG;
                }
                (
                    Conv::Text {
                        utf8: false,
                        zero: true,
                        chars,
                    },
                    failures,
                )
            }
            (Plain::Binary | Plain::LargeBinary | Plain::BinaryView, None, PgType::Bytea) => {
                (Conv::Bytea, Failures::TOO_LARGE)
            }
            (
                Plain::Binary | Plain::LargeBinary | Plain::BinaryView,
                None,
                PgType::Text | PgType::Varchar(_),
            ) => {
                let chars = varchar_chars(pg);
                match options.text {
                    None => {
                        return Err(refuse(
                            ErrorKind::TypeMismatch,
                            "bytes read as text without a treatment of text",
                        ));
                    }
                    Some(TextBytes::Checked) => {
                        let mut failures =
                            Failures::NOT_UTF8 | Failures::ZERO_BYTE | Failures::TOO_LARGE;
                        if chars.is_some() {
                            failures = failures | Failures::TOO_LONG;
                        }
                        (
                            Conv::Text {
                                utf8: true,
                                zero: true,
                                chars,
                            },
                            failures,
                        )
                    }
                    Some(TextBytes::Trusted) => {
                        if chars.is_some() {
                            return Err(refuse(
                                ErrorKind::TypeMismatch,
                                "trusted bytes into a varchar with a length, which counts characters",
                            ));
                        }
                        (
                            Conv::Text {
                                utf8: false,
                                zero: false,
                                chars: None,
                            },
                            Failures::TOO_LARGE,
                        )
                    }
                }
            }
            (Plain::FixedSizeBinary(_), None, PgType::Bytea) => (Conv::Bytea, Failures::NONE),
            _ => return mismatch(),
        };
        Self::finish(arrow, pg, conv, failures, options)
    }

    fn finish(
        arrow: ArrowType,
        pg: PgType,
        conv: Conv,
        failures: Failures,
        options: ReadOptions,
    ) -> Result<Self> {
        if let Plain::FixedSizeBinary(size) = arrow.values()
            && size > MAX_VALUE_BYTES
        {
            return Err(refuse(
                ErrorKind::TooLarge,
                "fixed-size bytes larger than PostgreSQL's largest value",
            ));
        }
        if matches!(pg, PgType::Text | PgType::Varchar(_)) && !options.server_utf8 {
            return Err(refuse(
                ErrorKind::NotSupported,
                "text read into a server whose encoding is not UTF-8",
            ));
        }
        Ok(ReadPlan {
            arrow,
            pg,
            conv,
            failures,
        })
    }

    /// The failures a value of the plan may still meet.
    pub const fn failures(&self) -> Failures {
        self.failures
    }

    /// How the plan's stored integers count in PostgreSQL's terms: an
    /// integer as itself, days and times shifted and scaled; `None` for a
    /// plan of other values.
    pub(crate) const fn stored_count(&self) -> Option<Count> {
        match self.conv {
            Conv::Int => Some(Count {
                scale: 1,
                shift: 0,
                low: i64::MIN,
                high: i64::MAX,
            }),
            Conv::Count(count) => Some(count),
            _ => None,
        }
    }

    /// The plan's Arrow type.
    pub const fn arrow_type(&self) -> ArrowType {
        self.arrow
    }

    /// The plan's PostgreSQL type.
    pub const fn pg_type(&self) -> PgType {
        self.pg
    }

    /// Whether the plan gives `Datum` words, [`ReadOutput::Datums`], or
    /// bytes, [`ReadOutput::Bytes`].
    pub const fn gives_datums(&self) -> bool {
        self.conv.fixed()
    }

    /// Reads the slots `asked` of `input` into `output`.
    ///
    /// `asked` is a mask of 64-bit words, bit `i % 64` of word `i / 64`
    /// for slot `i`. Each slot asked gets its representation in the
    /// output's place of its slot, or a set bit in the output's mask of
    /// `NULL` slots, where its bitmap, or a dictionary's values' bitmap,
    /// says it is `NULL`. A `NULL` slot never fails, nor does a slot not
    /// asked, and a dictionary's index is read only for a slot asked that
    /// is not `NULL`.
    ///
    /// The representations: a value of a type PostgreSQL passes by value
    /// as the 64-bit word of a `Datum`, as the server makes it (a
    /// `boolean` as 0 or 1; a `smallint`, an `integer`, a `date` and the
    /// bits of a `real` widened with their sign; the others as their 64
    /// bits); a `text`, a `varchar` or a `bytea` as its bytes, a slice of
    /// the input's, without PostgreSQL's header.
    ///
    /// For a plain column of integers, floating-point numbers, days,
    /// times or booleans, the call may write the output's place of a slot
    /// not asked or `NULL`: with the slot's representation where the
    /// slot is not `NULL` and its value converts, and otherwise with a
    /// value of no meaning. The output's place of a `NULL` slot has no
    /// meaning after the call; its bit in the mask of `NULL` slots is
    /// set, and the bit of a slot not asked is clear. For strings and
    /// bytes, and for any dictionary, the output's place of a slot not
    /// asked is left as it was; a dictionary's value that no slot asked
    /// and not `NULL` refers to is neither checked nor fails.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::BrokenShape`], before any value is converted and with
    /// the output unchanged, for an input of another type than the
    /// plan's; an output of the other form than
    /// [`ReadPlan::gives_datums`] says; buffers, a bitmap, an output or its
    /// mask of `NULL` slots shorter than the column; a mask of the slots
    /// asked of fewer than [`mask_words`](crate::mask_words) words, or one
    /// that asks for a slot past the column; offsets that go back or past
    /// their bytes, of a slot asked, `NULL` or not, or of a dictionary's
    /// value that a slot asked and not `NULL` refers to, as Arrow's format
    /// wants the offsets of every slot to hold; views fewer than the
    /// column's slots, and a view whose buffer is past the buffers or
    /// whose bytes run past its buffer, of a slot asked that is not
    /// `NULL`, since a `NULL` slot may hold any view; and an index of a
    /// dictionary outside it in a slot asked that is not `NULL`.
    ///
    /// A failure the plan names, with the slot of the first slot asked
    /// that fails; the slots asked before it have their representations
    /// and their bits in the mask of `NULL` slots, and the rest of the
    /// output has no meaning.
    pub fn read<'a>(
        &self,
        input: &Input<'a>,
        asked: &[u64],
        output: &mut ReadOutput<'_, 'a>,
    ) -> Result<()> {
        self.check_shape(input, asked, output)?;
        match input {
            Input::Plain(column) => self.read_plain(column, asked, output),
            Input::Dictionary {
                len,
                keys,
                nulls,
                values,
            } => self.read_dictionary(*len, keys, *nulls, values, asked, output),
        }
    }

    fn check_shape(
        &self,
        input: &Input<'_>,
        asked: &[u64],
        output: &ReadOutput<'_, '_>,
    ) -> Result<()> {
        let broken = |message| Err(Error::new(ErrorKind::BrokenShape, message));
        let len = input.len();
        if !mask_fits(asked, len) {
            return broken("a mask shorter than the column, or one that asks past it");
        }
        let (out_len, out_nulls) = match output {
            ReadOutput::Datums { values, nulls } => (values.len(), nulls.len()),
            ReadOutput::Bytes { values, nulls } => (values.len(), nulls.len()),
        };
        if matches!(output, ReadOutput::Datums { .. }) != self.conv.fixed() {
            return broken("an output of another form than the plan gives");
        }
        if out_len < len || out_nulls < mask_words(len) {
            return broken("an output shorter than the column");
        }
        match (input, self.arrow) {
            (Input::Plain(column), ArrowType::Plain(plain)) => {
                check_column(column, plain)?;
                if let Some(range) = bytes_of(column) {
                    // Arrow's format wants the offsets of every slot,
                    // NULL or not, to hold; a NULL slot may hold any view.
                    let (nulls, message) = match range {
                        ByteRange::Views(..) => (
                            column.nulls,
                            "a view of a slot asked whose buffer or bytes do not hold",
                        ),
                        _ => (
                            None,
                            "offsets of a slot asked that go back or past their bytes",
                        ),
                    };
                    each_slot(asked, nulls, len, |slot| {
                        if range.slot(slot).is_some() {
                            Ok(())
                        } else {
                            broken(message)
                        }
                    })?;
                }
                Ok(())
            }
            (
                Input::Dictionary {
                    len,
                    keys,
                    nulls,
                    values,
                },
                ArrowType::Dictionary { key, value },
            ) => {
                if !keys_match(keys, key) || keys.len() < *len {
                    return broken("indexes of another type than the plan's, or too few");
                }
                if nulls.is_some_and(|nulls| !nulls.covers(*len)) {
                    return broken("a bitmap of NULL values shorter than the column");
                }
                check_column(values, value)?;
                let range = bytes_of(values);
                each_slot(asked, *nulls, *len, |slot| match keys.get(slot) {
                    Some(index) if index < values.len => match &range {
                        Some(range) if range.slot(index).is_none() => broken(
                            "offsets of a dictionary's value that go back or past their bytes",
                        ),
                        _ => Ok(()),
                    },
                    _ => broken("an index of a dictionary outside it"),
                })
            }
            _ => broken("a column of another type than the plan's"),
        }
    }

    fn read_plain<'a>(
        &self,
        column: &Column<'a>,
        asked: &[u64],
        output: &mut ReadOutput<'_, 'a>,
    ) -> Result<()> {
        let len = column.len;
        match output {
            ReadOutput::Datums { values, nulls } => {
                let out = &mut values[..len];
                let valid = column.nulls;
                match (self.conv, column.values) {
                    (Conv::Bool, Values::Boolean(bits)) => {
                        dense_bits(bits, len, asked, valid, out, nulls);
                        Ok(())
                    }
                    (Conv::Int, values) => {
                        int_values(values, len, asked, valid, out, nulls);
                        Ok(())
                    }
                    (Conv::Float4, Values::Float32(v)) => dense(
                        &v[..len],
                        asked,
                        valid,
                        out,
                        nulls,
                        |x| float4_datum(*x),
                        |_| true,
                        None,
                    ),
                    (Conv::Float8FromFloat4, Values::Float32(v)) => dense(
                        &v[..len],
                        asked,
                        valid,
                        out,
                        nulls,
                        |x| f64::from(*x).to_bits(),
                        |_| true,
                        None,
                    ),
                    (Conv::Float8, Values::Float64(v)) => dense(
                        &v[..len],
                        asked,
                        valid,
                        out,
                        nulls,
                        |x| x.to_bits(),
                        |_| true,
                        None,
                    ),
                    (Conv::Count(count), values) => count_values(
                        count,
                        self.pg,
                        self.failures.contains(Failures::OUT_OF_RANGE),
                        values,
                        len,
                        asked,
                        valid,
                        out,
                        nulls,
                    ),
                    _ => Err(Error::new(
                        ErrorKind::BrokenShape,
                        "a column of another type than the plan's",
                    )),
                }
            }
            ReadOutput::Bytes { values: out, nulls } => {
                let range = bytes_of(column).ok_or(Error::new(
                    ErrorKind::BrokenShape,
                    "a column of another type than the plan's",
                ))?;
                for (word, null) in nulls.iter_mut().enumerate().take(mask_words(len)) {
                    *null = asked[word] & !valid_word(column.nulls, word * 64, len);
                }
                each_slot(asked, column.nulls, len, |slot| {
                    let bytes = range.slot(slot).unwrap_or(&[]);
                    self.check_bytes(bytes).map_err(|e| e.at(slot))?;
                    out[slot] = bytes;
                    Ok(())
                })
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn read_dictionary<'a>(
        &self,
        len: usize,
        keys: &Keys<'a>,
        key_nulls: Option<Bitmap<'a>>,
        values: &Column<'a>,
        asked: &[u64],
        output: &mut ReadOutput<'_, 'a>,
    ) -> Result<()> {
        let range = bytes_of(values);
        for word in 0..mask_words(len) {
            let first = word * 64;
            let mut null = asked[word] & !valid_word(key_nulls, first, len);
            let mut slots = asked[word] & !null;
            let result = (|| {
                while slots != 0 {
                    let slot = first + slots.trailing_zeros() as usize;
                    slots &= slots - 1;
                    // The shape's check found every index of a slot asked
                    // that is not NULL within the dictionary.
                    let index = keys.get(slot).unwrap_or(0);
                    if values.nulls.is_some_and(|nulls| !nulls.get(index)) {
                        null |= 1 << (slot - first);
                        continue;
                    }
                    match output {
                        ReadOutput::Datums { values: out, .. } => {
                            out[slot] = self
                                .datum_of(values.values, index)
                                .map_err(|e| e.at(slot))?;
                        }
                        ReadOutput::Bytes { values: out, .. } => {
                            let bytes = range.as_ref().and_then(|r| r.slot(index)).unwrap_or(&[]);
                            self.check_bytes(bytes).map_err(|e| e.at(slot))?;
                            out[slot] = bytes;
                        }
                    }
                }
                Ok(())
            })();
            let nulls = match output {
                ReadOutput::Datums { nulls, .. } | ReadOutput::Bytes { nulls, .. } => nulls,
            };
            nulls[word] = null;
            result?;
        }
        Ok(())
    }

    /// The `Datum` of a dictionary's value at `index`.
    fn datum_of(&self, values: Values<'_>, index: usize) -> Result<u64> {
        let int = |value: i64| -> Result<u64> {
            match self.conv {
                Conv::Count(count) => {
                    if !self.failures.contains(Failures::OUT_OF_RANGE) || count.holds(value) {
                        Ok(count_datum(count, self.pg, value))
                    } else {
                        Err(out_of_range(self.pg))
                    }
                }
                _ => Ok(value as u64),
            }
        };
        match values {
            Values::Boolean(bits) => Ok(u64::from(bits.get(index))),
            Values::Int8(v) => int(i64::from(v[index])),
            Values::Int16(v) => int(i64::from(v[index])),
            Values::Int32(v) => int(i64::from(v[index])),
            Values::Int64(v) => int(v[index]),
            Values::UInt8(v) => int(i64::from(v[index])),
            Values::UInt16(v) => int(i64::from(v[index])),
            Values::UInt32(v) => int(i64::from(v[index])),
            Values::Float32(v) => Ok(match self.conv {
                Conv::Float4 => float4_datum(v[index]),
                _ => f64::from(v[index]).to_bits(),
            }),
            Values::Float64(v) => Ok(v[index].to_bits()),
            Values::Bytes { .. }
            | Values::LargeBytes { .. }
            | Values::Views { .. }
            | Values::FixedBytes { .. } => Err(Error::new(
                ErrorKind::BrokenShape,
                "a column of another type than the plan's",
            )),
        }
    }

    /// The checks a value of strings or bytes runs, in the order of the
    /// crate: its size, then its UTF-8 and its zero byte, then its length.
    fn check_bytes(&self, bytes: &[u8]) -> Result<()> {
        if self.failures.contains(Failures::TOO_LARGE) && bytes.len() > MAX_VALUE_BYTES {
            return Err(Error::new(
                ErrorKind::TooLarge,
                "a value larger than PostgreSQL's largest",
            ));
        }
        if let Conv::Text { utf8, zero, chars } = self.conv {
            if utf8 && core::str::from_utf8(bytes).is_err() {
                return Err(Error::new(
                    ErrorKind::InvalidText,
                    "invalid byte sequence for encoding \"UTF8\"",
                ));
            }
            if zero && has_zero(bytes) {
                return Err(Error::new(
                    ErrorKind::InvalidText,
                    "invalid byte sequence for encoding \"UTF8\": 0x00",
                ));
            }
            if let Some(chars) = chars
                && bytes.len() > chars as usize
                && count_chars(bytes) > chars as usize
            {
                return Err(Error::new(
                    ErrorKind::TooLong,
                    "value too long for type character varying",
                ));
            }
        }
        Ok(())
    }
}

/// The output of a read: the `Datum` words or the bytes of the slots
/// asked, each at its slot's place, and a mask of the `NULL` slots
/// asked, of at least [`mask_words`](crate::mask_words) words of the
/// column's slots.
#[derive(Debug)]
pub enum ReadOutput<'o, 'a> {
    /// The output of a plan that gives `Datum` words.
    Datums {
        /// A word a slot, at least as many as the column's slots.
        values: &'o mut [u64],
        /// The `NULL` slots asked, a bit a slot.
        nulls: &'o mut [u64],
    },
    /// The output of a plan into `text`, `varchar` or `bytea`.
    Bytes {
        /// A slice of the input's bytes a slot, at least as many as the
        /// column's slots.
        values: &'o mut [&'a [u8]],
        /// The `NULL` slots asked, a bit a slot.
        nulls: &'o mut [u64],
    },
}

const fn varchar_chars(pg: PgType) -> Option<u32> {
    match pg {
        PgType::Varchar(Some(length)) => Some(length.get()),
        _ => None,
    }
}

fn keys_match(keys: &Keys<'_>, key: Key) -> bool {
    matches!(
        (keys, key),
        (Keys::Int8(_), Key::Int8)
            | (Keys::Int16(_), Key::Int16)
            | (Keys::Int32(_), Key::Int32)
            | (Keys::Int64(_), Key::Int64)
            | (Keys::UInt8(_), Key::UInt8)
            | (Keys::UInt16(_), Key::UInt16)
            | (Keys::UInt32(_), Key::UInt32)
            | (Keys::UInt64(_), Key::UInt64)
    )
}

/// Checks that a column's buffers hold its slots and are of `plain`.
fn check_column(column: &Column<'_>, plain: Plain) -> Result<()> {
    let broken = |message| Err(Error::new(ErrorKind::BrokenShape, message));
    let len = column.len;
    if column.nulls.is_some_and(|nulls| !nulls.covers(len)) {
        return broken("a bitmap of NULL values shorter than the column");
    }
    let held = match (column.values, plain) {
        (Values::Boolean(bits), Plain::Boolean) => bits.covers(len),
        (Values::Int8(v), Plain::Int8) => v.len() >= len,
        (Values::Int16(v), Plain::Int16) => v.len() >= len,
        (Values::Int32(v), Plain::Int32 | Plain::Date32) => v.len() >= len,
        (Values::Int64(v), Plain::Int64 | Plain::Timestamp { .. }) => v.len() >= len,
        (Values::UInt8(v), Plain::UInt8) => v.len() >= len,
        (Values::UInt16(v), Plain::UInt16) => v.len() >= len,
        (Values::UInt32(v), Plain::UInt32) => v.len() >= len,
        (Values::Float32(v), Plain::Float32) => v.len() >= len,
        (Values::Float64(v), Plain::Float64) => v.len() >= len,
        (Values::Bytes { offsets, .. }, Plain::Utf8 | Plain::Binary) => {
            offsets.len() > len || (len == 0 && offsets.is_empty())
        }
        (Values::LargeBytes { offsets, .. }, Plain::LargeUtf8 | Plain::LargeBinary) => {
            offsets.len() > len || (len == 0 && offsets.is_empty())
        }
        (Values::Views { views, .. }, Plain::Utf8View | Plain::BinaryView) => {
            len.checked_mul(VIEW).is_some_and(|n| views.len() >= n)
        }
        (Values::FixedBytes { size, data }, Plain::FixedSizeBinary(plan_size)) => {
            size == plan_size && size.checked_mul(len).is_some_and(|n| data.len() >= n)
        }
        _ => return broken("a column of another type than the plan's"),
    };
    if held {
        Ok(())
    } else {
        broken("buffers shorter than the column")
    }
}

/// The bytes of a view.
const VIEW: usize = 16;

/// The most bytes of a value that lies in its view.
const INLINE: usize = 12;

/// The bytes of a column of strings or bytes, by slot.
enum ByteRange<'a> {
    Small(&'a [i32], &'a [u8]),
    Large(&'a [i64], &'a [u8]),
    Views(&'a [u8], &'a [&'a [u8]]),
    Fixed(usize, &'a [u8]),
}

/// The integer of 4 little-endian bytes of a view from `at`.
fn view_word(view: &[u8], at: usize) -> Option<usize> {
    let bytes = view.get(at..at + 4)?.try_into().ok()?;
    usize::try_from(u32::from_le_bytes(bytes)).ok()
}

impl<'a> ByteRange<'a> {
    /// The bytes of `slot`, `None` where its offsets go back or past the
    /// bytes, or the slot is past the buffers.
    fn slot(&self, slot: usize) -> Option<&'a [u8]> {
        let (start, end, data) = match *self {
            ByteRange::Small(offsets, data) => (
                usize::try_from(*offsets.get(slot)?).ok()?,
                usize::try_from(*offsets.get(slot + 1)?).ok()?,
                data,
            ),
            ByteRange::Large(offsets, data) => (
                usize::try_from(*offsets.get(slot)?).ok()?,
                usize::try_from(*offsets.get(slot + 1)?).ok()?,
                data,
            ),
            ByteRange::Views(views, buffers) => {
                let at = slot.checked_mul(VIEW)?;
                let view = views.get(at..at.checked_add(VIEW)?)?;
                let length = view_word(view, 0)?;
                if length <= INLINE {
                    return view.get(4..4 + length);
                }
                let start = view_word(view, 12)?;
                (
                    start,
                    start.checked_add(length)?,
                    *buffers.get(view_word(view, 8)?)?,
                )
            }
            ByteRange::Fixed(size, data) => {
                let start = slot.checked_mul(size)?;
                (start, start.checked_add(size)?, data)
            }
        };
        if start <= end {
            data.get(start..end)
        } else {
            None
        }
    }
}

fn bytes_of<'a>(column: &Column<'a>) -> Option<ByteRange<'a>> {
    match column.values {
        Values::Bytes { offsets, data } => Some(ByteRange::Small(offsets, data)),
        Values::LargeBytes { offsets, data } => Some(ByteRange::Large(offsets, data)),
        Values::Views { views, buffers } => Some(ByteRange::Views(views, buffers)),
        Values::FixedBytes { size, data } => Some(ByteRange::Fixed(size, data)),
        _ => None,
    }
}

/// The valid slots of the word from `first`: set where a slot holds a
/// value, every bit where it has no bitmap; the caller masks the word
/// by the slots asked.
fn valid_word(nulls: Option<Bitmap<'_>>, first: usize, len: usize) -> u64 {
    // Without a bitmap every bit is set, those past the column too: each
    // use masks the word by the slots asked, which stop at the column.
    nulls.map_or(u64::MAX, |nulls| nulls.word(first, len))
}

/// Calls `visit` for each slot asked that is not `NULL`, in order.
fn each_slot(
    asked: &[u64],
    nulls: Option<Bitmap<'_>>,
    len: usize,
    mut visit: impl FnMut(usize) -> Result<()>,
) -> Result<()> {
    for (word, &ask) in asked.iter().take(mask_words(len)).enumerate() {
        let first = word * 64;
        let mut slots = ask & valid_word(nulls, first, len);
        while slots != 0 {
            visit(first + slots.trailing_zeros() as usize)?;
            slots &= slots - 1;
        }
    }
    Ok(())
}

/// A dense loop over a plain column of a fixed width: every slot of a
/// word converted, then the word's failures, masked by the slots asked
/// and not `NULL`, found by their first bit.
#[allow(clippy::too_many_arguments)]
#[inline]
fn dense<T>(
    values: &[T],
    asked: &[u64],
    valid: Option<Bitmap<'_>>,
    out: &mut [u64],
    nulls: &mut [u64],
    convert: impl Fn(&T) -> u64,
    holds: impl Fn(&T) -> bool,
    failure: Option<Error>,
) -> Result<()> {
    let len = values.len();
    for (word, chunk) in values.chunks(64).enumerate() {
        let first = word * 64;
        let out = &mut out[first..first + chunk.len()];
        let mut bad = 0u64;
        for (i, (value, place)) in chunk.iter().zip(out.iter_mut()).enumerate() {
            *place = convert(value);
            bad |= u64::from(!holds(value)) << i;
        }
        let valid = valid_word(valid, first, len);
        nulls[word] = asked[word] & !valid;
        let failed = bad & asked[word] & valid;
        if failed != 0
            && let Some(error) = failure
        {
            return Err(error.at(first + failed.trailing_zeros() as usize));
        }
    }
    Ok(())
}

fn dense_bits(
    bits: Bitmap<'_>,
    len: usize,
    asked: &[u64],
    valid: Option<Bitmap<'_>>,
    out: &mut [u64],
    nulls: &mut [u64],
) {
    let words = asked.iter().zip(nulls.iter_mut()).take(mask_words(len));
    for (word, (&ask, null)) in words.enumerate() {
        let first = word * 64;
        let values = bits.word(first, len);
        let count = (len - first).min(64);
        for (i, place) in out[first..first + count].iter_mut().enumerate() {
            *place = values >> i & 1;
        }
        *null = ask & !valid_word(valid, first, len);
    }
}

fn int_values(
    values: Values<'_>,
    len: usize,
    asked: &[u64],
    valid: Option<Bitmap<'_>>,
    out: &mut [u64],
    nulls: &mut [u64],
) {
    // Integers widen without a failure.
    let _ = match values {
        Values::Int8(v) => dense(
            &v[..len],
            asked,
            valid,
            out,
            nulls,
            |x| i64::from(*x) as u64,
            |_| true,
            None,
        ),
        Values::Int16(v) => dense(
            &v[..len],
            asked,
            valid,
            out,
            nulls,
            |x| i64::from(*x) as u64,
            |_| true,
            None,
        ),
        Values::Int32(v) => dense(
            &v[..len],
            asked,
            valid,
            out,
            nulls,
            |x| i64::from(*x) as u64,
            |_| true,
            None,
        ),
        Values::Int64(v) => dense(
            &v[..len],
            asked,
            valid,
            out,
            nulls,
            |x| *x as u64,
            |_| true,
            None,
        ),
        Values::UInt8(v) => dense(
            &v[..len],
            asked,
            valid,
            out,
            nulls,
            |x| u64::from(*x),
            |_| true,
            None,
        ),
        Values::UInt16(v) => dense(
            &v[..len],
            asked,
            valid,
            out,
            nulls,
            |x| u64::from(*x),
            |_| true,
            None,
        ),
        Values::UInt32(v) => dense(
            &v[..len],
            asked,
            valid,
            out,
            nulls,
            |x| u64::from(*x),
            |_| true,
            None,
        ),
        _ => Ok(()),
    };
}

#[allow(clippy::too_many_arguments)]
fn count_values(
    count: Count,
    pg: PgType,
    checked: bool,
    values: Values<'_>,
    len: usize,
    asked: &[u64],
    valid: Option<Bitmap<'_>>,
    out: &mut [u64],
    nulls: &mut [u64],
) -> Result<()> {
    let failure = checked.then(|| out_of_range(pg));
    // A plan of days in 8 or 16 bits names no failure, and runs no check.
    let holds = |v: i64| !checked || count.holds(v);
    // Days come in integers of at most 32 bits and are only shifted;
    // times come in 64-bit integers and are scaled too. Each loop keeps
    // its own arithmetic, so that the days' needs no multiplication.
    let datum = |v: i64| day_datum(count, v);
    match values {
        Values::Int8(v) => dense(
            &v[..len],
            asked,
            valid,
            out,
            nulls,
            |x| datum(i64::from(*x)),
            |x| holds(i64::from(*x)),
            failure,
        ),
        Values::Int16(v) => dense(
            &v[..len],
            asked,
            valid,
            out,
            nulls,
            |x| datum(i64::from(*x)),
            |x| holds(i64::from(*x)),
            failure,
        ),
        Values::Int32(v) => dense(
            &v[..len],
            asked,
            valid,
            out,
            nulls,
            |x| datum(i64::from(*x)),
            |x| holds(i64::from(*x)),
            failure,
        ),
        Values::Int64(v) => dense(
            &v[..len],
            asked,
            valid,
            out,
            nulls,
            |x| count.convert(*x) as u64,
            |x| holds(*x),
            failure,
        ),
        Values::UInt8(v) => dense(
            &v[..len],
            asked,
            valid,
            out,
            nulls,
            |x| datum(i64::from(*x)),
            |x| holds(i64::from(*x)),
            failure,
        ),
        Values::UInt16(v) => dense(
            &v[..len],
            asked,
            valid,
            out,
            nulls,
            |x| datum(i64::from(*x)),
            |x| holds(i64::from(*x)),
            failure,
        ),
        Values::UInt32(v) => dense(
            &v[..len],
            asked,
            valid,
            out,
            nulls,
            |x| datum(i64::from(*x)),
            |x| holds(i64::from(*x)),
            failure,
        ),
        _ => Err(Error::new(
            ErrorKind::BrokenShape,
            "a column of another type than the plan's",
        )),
    }
}

/// The `Datum` of a stored count of days: days since 2000 as their 32
/// bits widened with their sign, of no meaning outside the range.
#[inline]
fn day_datum(count: Count, stored: i64) -> u64 {
    i64::from(stored.wrapping_sub(count.shift) as i32) as u64
}

/// The `Datum` of a stored count: a `date` as its 32 bits widened with
/// their sign, a time as its 64 bits.
#[inline]
fn count_datum(count: Count, pg: PgType, stored: i64) -> u64 {
    let value = count.convert(stored);
    if pg == PgType::Date {
        i64::from(value as i32) as u64
    } else {
        value as u64
    }
}

/// The `Datum` of a `real`: its 32 bits widened with their sign, as
/// PostgreSQL's `Float4GetDatum` makes it.
#[inline]
fn float4_datum(value: f32) -> u64 {
    i64::from(value.to_bits() as i32) as u64
}

fn out_of_range(pg: PgType) -> Error {
    if pg == PgType::Date {
        Error::new(ErrorKind::OutOfRange, "date out of range")
    } else {
        Error::new(ErrorKind::OutOfRange, "timestamp out of range")
    }
}

/// Whether bytes hold a zero byte, which a PostgreSQL text cannot.
pub(crate) fn has_zero(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

/// The characters of UTF-8 bytes: the bytes that do not continue one.
/// Of bytes that are not UTF-8, a count of no meaning, but no panic.
pub(crate) fn count_chars(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b & 0xc0 != 0x80).count()
}

#[cfg(test)]
mod tests;

//! The Arrow types the crate takes.
//!
//! [`ArrowType::new`] reads an [`arrow_schema::DataType`] into the crate's
//! own form and refuses a type outside its set, so that a plan matches
//! over a closed list.

use arrow_schema::DataType;

use crate::error::{Error, ErrorKind, Result};

/// The unit of an Arrow `Timestamp` the crate takes.
///
/// A `Timestamp` in nanoseconds is refused: PostgreSQL keeps
/// microseconds, and the crate never changes a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimeUnit {
    /// Seconds since 1970-01-01 00:00:00.
    Second,
    /// Milliseconds since 1970-01-01 00:00:00.
    Millisecond,
    /// Microseconds since 1970-01-01 00:00:00.
    Microsecond,
}

/// An Arrow type the crate takes that is not a dictionary: the type of a
/// plain array, or of a dictionary's values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Plain {
    /// `Boolean`.
    Boolean,
    /// `Int8`.
    Int8,
    /// `Int16`.
    Int16,
    /// `Int32`.
    Int32,
    /// `Int64`.
    Int64,
    /// `UInt8`.
    UInt8,
    /// `UInt16`.
    UInt16,
    /// `UInt32`. `UInt64` is refused: no PostgreSQL integer holds it.
    UInt32,
    /// `Float32`.
    Float32,
    /// `Float64`.
    Float64,
    /// `Date32`, days since 1970-01-01.
    Date32,
    /// `Timestamp`, in its unit.
    Timestamp {
        /// The unit of its integers.
        unit: TimeUnit,
        /// Whether it holds instants, counted in UTC: true for any time
        /// zone that is set and not empty, whatever its text; false for
        /// a time zone absent or empty, a time without a zone.
        utc: bool,
    },
    /// `Utf8`, strings with 32-bit offsets.
    Utf8,
    /// `LargeUtf8`, strings with 64-bit offsets.
    LargeUtf8,
    /// `Binary`, bytes with 32-bit offsets.
    Binary,
    /// `LargeBinary`, bytes with 64-bit offsets.
    LargeBinary,
    /// `Utf8View`, strings by views of 16 bytes
    /// ([`Values::Views`](crate::Values::Views)).
    Utf8View,
    /// `BinaryView`, bytes by views of 16 bytes
    /// ([`Values::Views`](crate::Values::Views)).
    BinaryView,
    /// `FixedSizeBinary`, of a size of 0 bytes or more.
    FixedSizeBinary(usize),
}

/// The integer type of a dictionary's indexes: any Arrow allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// `Int8`.
    Int8,
    /// `Int16`.
    Int16,
    /// `Int32`.
    Int32,
    /// `Int64`.
    Int64,
    /// `UInt8`.
    UInt8,
    /// `UInt16`.
    UInt16,
    /// `UInt32`.
    UInt32,
    /// `UInt64`.
    UInt64,
}

/// An Arrow type the crate takes: a plain type, or a dictionary of one.
///
/// The crate takes `Boolean`; the signed integers of 8 to 64 bits and
/// the unsigned of 8 to 32; `Float32` and `Float64`; `Date32`;
/// `Timestamp` in seconds, milliseconds and microseconds, with or
/// without a time zone; `Utf8`, `LargeUtf8`, `Binary`, `LargeBinary`,
/// `Utf8View`, `BinaryView`; `FixedSizeBinary` of a size of 0 or more; and a `Dictionary` of any of
/// them whose indexes are of an integer type. Every other type, a
/// dictionary of a dictionary among them, is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArrowType {
    /// A plain array.
    Plain(Plain),
    /// A dictionary: an index of `key` a slot into a list of values of
    /// `value`.
    Dictionary {
        /// The type of the indexes.
        key: Key,
        /// The type of the dictionary's values.
        value: Plain,
    },
}

impl ArrowType {
    /// The crate's form of `data_type`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotSupported`] for a type outside the crate's set.
    pub fn new(data_type: &DataType) -> Result<Self> {
        match data_type {
            DataType::Dictionary(key, value) => {
                let key = match key.as_ref() {
                    DataType::Int8 => Key::Int8,
                    DataType::Int16 => Key::Int16,
                    DataType::Int32 => Key::Int32,
                    DataType::Int64 => Key::Int64,
                    DataType::UInt8 => Key::UInt8,
                    DataType::UInt16 => Key::UInt16,
                    DataType::UInt32 => Key::UInt32,
                    DataType::UInt64 => Key::UInt64,
                    _ => {
                        return Err(Error::new(
                            ErrorKind::NotSupported,
                            "a dictionary whose indexes are not integers",
                        ));
                    }
                };
                if matches!(value.as_ref(), DataType::Dictionary(..)) {
                    return Err(Error::new(
                        ErrorKind::NotSupported,
                        "a dictionary of a dictionary",
                    ));
                }
                Ok(ArrowType::Dictionary {
                    key,
                    value: plain(value)?,
                })
            }
            other => Ok(ArrowType::Plain(plain(other)?)),
        }
    }

    /// The type of the values: the plain type itself, or the
    /// dictionary's values.
    pub const fn values(self) -> Plain {
        match self {
            ArrowType::Plain(plain) | ArrowType::Dictionary { value: plain, .. } => plain,
        }
    }
}

fn plain(data_type: &DataType) -> Result<Plain> {
    let not_supported = || {
        Err(Error::new(
            ErrorKind::NotSupported,
            "an Arrow type the crate does not take",
        ))
    };
    Ok(match data_type {
        DataType::Boolean => Plain::Boolean,
        DataType::Int8 => Plain::Int8,
        DataType::Int16 => Plain::Int16,
        DataType::Int32 => Plain::Int32,
        DataType::Int64 => Plain::Int64,
        DataType::UInt8 => Plain::UInt8,
        DataType::UInt16 => Plain::UInt16,
        DataType::UInt32 => Plain::UInt32,
        DataType::Float32 => Plain::Float32,
        DataType::Float64 => Plain::Float64,
        DataType::Date32 => Plain::Date32,
        DataType::Timestamp(unit, zone) => {
            let unit = match unit {
                arrow_schema::TimeUnit::Second => TimeUnit::Second,
                arrow_schema::TimeUnit::Millisecond => TimeUnit::Millisecond,
                arrow_schema::TimeUnit::Microsecond => TimeUnit::Microsecond,
                arrow_schema::TimeUnit::Nanosecond => return not_supported(),
            };
            let utc = zone.as_deref().is_some_and(|zone| !zone.is_empty());
            Plain::Timestamp { unit, utc }
        }
        DataType::Utf8 => Plain::Utf8,
        DataType::LargeUtf8 => Plain::LargeUtf8,
        DataType::Binary => Plain::Binary,
        DataType::LargeBinary => Plain::LargeBinary,
        DataType::Utf8View => Plain::Utf8View,
        DataType::BinaryView => Plain::BinaryView,
        DataType::FixedSizeBinary(size) => match usize::try_from(*size) {
            Ok(size) => Plain::FixedSizeBinary(size),
            Err(_) => return not_supported(),
        },
        _ => return not_supported(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_schema::{Field, Fields};

    use super::*;

    fn dictionary(key: DataType, value: DataType) -> DataType {
        DataType::Dictionary(Box::new(key), Box::new(value))
    }

    fn refused(data_type: DataType) -> bool {
        ArrowType::new(&data_type).is_err_and(|e| e.kind() == ErrorKind::NotSupported)
    }

    #[test]
    fn every_plain_type_taken() {
        let types = [
            (DataType::Boolean, Plain::Boolean),
            (DataType::Int8, Plain::Int8),
            (DataType::Int16, Plain::Int16),
            (DataType::Int32, Plain::Int32),
            (DataType::Int64, Plain::Int64),
            (DataType::UInt8, Plain::UInt8),
            (DataType::UInt16, Plain::UInt16),
            (DataType::UInt32, Plain::UInt32),
            (DataType::Float32, Plain::Float32),
            (DataType::Float64, Plain::Float64),
            (DataType::Date32, Plain::Date32),
            (DataType::Utf8, Plain::Utf8),
            (DataType::LargeUtf8, Plain::LargeUtf8),
            (DataType::Binary, Plain::Binary),
            (DataType::LargeBinary, Plain::LargeBinary),
            (DataType::Utf8View, Plain::Utf8View),
            (DataType::BinaryView, Plain::BinaryView),
            (DataType::FixedSizeBinary(0), Plain::FixedSizeBinary(0)),
            (DataType::FixedSizeBinary(16), Plain::FixedSizeBinary(16)),
        ];
        for (data_type, plain) in types {
            assert_eq!(ArrowType::new(&data_type), Ok(ArrowType::Plain(plain)));
            assert_eq!(ArrowType::Plain(plain).values(), plain);
        }
    }

    #[test]
    fn timestamps_by_unit_and_zone() {
        let units = [
            (arrow_schema::TimeUnit::Second, TimeUnit::Second),
            (arrow_schema::TimeUnit::Millisecond, TimeUnit::Millisecond),
            (arrow_schema::TimeUnit::Microsecond, TimeUnit::Microsecond),
        ];
        for (arrow_unit, unit) in units {
            for (zone, utc) in [
                (None, false),
                (Some(""), false),
                (Some("UTC"), true),
                (Some("Europe/Paris"), true),
                (Some("+03:00"), true),
            ] {
                let data_type = DataType::Timestamp(arrow_unit, zone.map(Arc::from));
                assert_eq!(
                    ArrowType::new(&data_type),
                    Ok(ArrowType::Plain(Plain::Timestamp { unit, utc })),
                    "{data_type:?}"
                );
            }
        }
        assert!(refused(DataType::Timestamp(
            arrow_schema::TimeUnit::Nanosecond,
            None
        )));
    }

    #[test]
    fn dictionaries_of_any_integer_index() {
        let keys = [
            (DataType::Int8, Key::Int8),
            (DataType::Int16, Key::Int16),
            (DataType::Int32, Key::Int32),
            (DataType::Int64, Key::Int64),
            (DataType::UInt8, Key::UInt8),
            (DataType::UInt16, Key::UInt16),
            (DataType::UInt32, Key::UInt32),
            (DataType::UInt64, Key::UInt64),
        ];
        for (data_type, key) in keys {
            let ty = ArrowType::new(&dictionary(data_type, DataType::Utf8));
            assert_eq!(
                ty,
                Ok(ArrowType::Dictionary {
                    key,
                    value: Plain::Utf8
                })
            );
            assert_eq!(ty.map(ArrowType::values), Ok(Plain::Utf8));
        }
        assert_eq!(
            ArrowType::new(&dictionary(DataType::Int32, DataType::Int32)),
            Ok(ArrowType::Dictionary {
                key: Key::Int32,
                value: Plain::Int32
            })
        );
    }

    #[test]
    fn types_outside_the_set() {
        let list = DataType::List(Arc::new(Field::new("item", DataType::Int32, true)));
        let fields = Fields::from(vec![Field::new("a", DataType::Int32, true)]);
        for data_type in [
            DataType::UInt64,
            DataType::Float16,
            DataType::Decimal128(10, 2),
            DataType::Date64,
            DataType::Null,
            DataType::FixedSizeBinary(-1),
            list,
            DataType::Struct(fields),
            dictionary(DataType::Int32, dictionary(DataType::Int32, DataType::Utf8)),
            dictionary(DataType::Utf8, DataType::Int32),
            dictionary(DataType::Float32, DataType::Utf8),
            dictionary(DataType::Int32, DataType::UInt64),
            dictionary(DataType::Int32, DataType::Decimal128(10, 2)),
        ] {
            assert!(refused(data_type.clone()), "{data_type:?}");
        }
    }
}

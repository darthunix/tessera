//! The PostgreSQL types the crate knows.
//!
//! A PostgreSQL type is named by its object identifier, its OID, and its
//! type modifier, the number a column's declaration adds, such as the
//! length of a `varchar(n)`; -1 means none. [`PgType::new`] takes the two
//! as a server gives them and refuses a type or a modifier outside the
//! crate's set.

use core::num::NonZeroU32;

use crate::error::{Error, ErrorKind, Result};

/// The object identifiers of the PostgreSQL types the crate knows, as
/// `pg_type.oid` holds them.
pub mod oid {
    /// `boolean`.
    pub const BOOL: u32 = 16;
    /// `bytea`.
    pub const BYTEA: u32 = 17;
    /// `bigint`.
    pub const INT8: u32 = 20;
    /// `smallint`.
    pub const INT2: u32 = 21;
    /// `integer`.
    pub const INT4: u32 = 23;
    /// `text`.
    pub const TEXT: u32 = 25;
    /// `real`.
    pub const FLOAT4: u32 = 700;
    /// `double precision`.
    pub const FLOAT8: u32 = 701;
    /// `varchar`, `character varying`.
    pub const VARCHAR: u32 = 1043;
    /// `date`.
    pub const DATE: u32 = 1082;
    /// `timestamp`, `timestamp without time zone`.
    pub const TIMESTAMP: u32 = 1114;
    /// `timestamptz`, `timestamp with time zone`.
    pub const TIMESTAMPTZ: u32 = 1184;
}

/// The length of the header that PostgreSQL adds to a value of variable
/// length, and to the type modifier of a `varchar(n)`.
pub const VARHDRSZ: i32 = 4;

/// The longest `varchar(n)` PostgreSQL declares, in characters.
pub const MAX_VARCHAR_LENGTH: u32 = 10_485_760;

/// The length of a `varchar(n)`, in characters: from 1 to
/// [`MAX_VARCHAR_LENGTH`], the lengths PostgreSQL declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VarcharLength(NonZeroU32);

impl VarcharLength {
    /// The length of `characters`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotSupported`] for 0 or a length past
    /// [`MAX_VARCHAR_LENGTH`].
    pub fn new(characters: u32) -> Result<Self> {
        match NonZeroU32::new(characters) {
            Some(length) if characters <= MAX_VARCHAR_LENGTH => Ok(VarcharLength(length)),
            _ => Err(Error::new(
                ErrorKind::NotSupported,
                "a varchar length PostgreSQL declares no column",
            )),
        }
    }

    /// The length, in characters.
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// A PostgreSQL type the crate knows, with its type modifier.
///
/// | Type               | OID  | Modifiers taken                        |
/// |--------------------|------|----------------------------------------|
/// | `boolean`          | 16   | -1                                     |
/// | `smallint`         | 21   | -1                                     |
/// | `integer`          | 23   | -1                                     |
/// | `bigint`           | 20   | -1                                     |
/// | `real`             | 700  | -1                                     |
/// | `double precision` | 701  | -1                                     |
/// | `date`             | 1082 | -1                                     |
/// | `timestamp`        | 1114 | -1, or 6, a precision of microseconds  |
/// | `timestamptz`      | 1184 | -1, or 6                               |
/// | `text`             | 25   | -1                                     |
/// | `varchar`          | 1043 | -1, or a length from 1 to 10 485 760 plus 4 |
/// | `bytea`            | 17   | -1                                     |
///
/// A `timestamp(p)` of a precision below 6 is refused: PostgreSQL rounds
/// a time to it on input, and the crate never changes a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PgType {
    /// `boolean`.
    Bool,
    /// `smallint`.
    Int2,
    /// `integer`.
    Int4,
    /// `bigint`.
    Int8,
    /// `real`.
    Float4,
    /// `double precision`.
    Float8,
    /// `date`.
    Date,
    /// `timestamp`, without a time zone.
    Timestamp,
    /// `timestamptz`, an instant.
    Timestamptz,
    /// `text`.
    Text,
    /// `varchar`, with its length where it has one.
    Varchar(Option<VarcharLength>),
    /// `bytea`.
    Bytea,
}

impl PgType {
    /// The type of `oid` with the modifier `typmod`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotSupported`] for an OID outside the table of
    /// [`PgType`], or a modifier the table does not give its type.
    pub fn new(oid: u32, typmod: i32) -> Result<Self> {
        let plain = |ty: PgType| {
            if typmod == -1 {
                Ok(ty)
            } else {
                Err(Error::new(
                    ErrorKind::NotSupported,
                    "the type takes no type modifier",
                ))
            }
        };
        match oid {
            oid::BOOL => plain(PgType::Bool),
            oid::INT2 => plain(PgType::Int2),
            oid::INT4 => plain(PgType::Int4),
            oid::INT8 => plain(PgType::Int8),
            oid::FLOAT4 => plain(PgType::Float4),
            oid::FLOAT8 => plain(PgType::Float8),
            oid::DATE => plain(PgType::Date),
            oid::TEXT => plain(PgType::Text),
            oid::BYTEA => plain(PgType::Bytea),
            oid::TIMESTAMP | oid::TIMESTAMPTZ => {
                if typmod != -1 && typmod != 6 {
                    return Err(Error::new(
                        ErrorKind::NotSupported,
                        "a type modifier of a time other than none or 6",
                    ));
                }
                Ok(if oid == oid::TIMESTAMP {
                    PgType::Timestamp
                } else {
                    PgType::Timestamptz
                })
            }
            oid::VARCHAR => {
                if typmod == -1 {
                    return Ok(PgType::Varchar(None));
                }
                match typmod.checked_sub(VARHDRSZ).map(u32::try_from) {
                    Some(Ok(length)) => Ok(PgType::Varchar(Some(VarcharLength::new(length)?))),
                    _ => Err(Error::new(
                        ErrorKind::NotSupported,
                        "a type modifier PostgreSQL gives no varchar",
                    )),
                }
            }
            _ => Err(Error::new(
                ErrorKind::NotSupported,
                "a PostgreSQL type the crate does not know",
            )),
        }
    }

    /// The type's object identifier.
    pub const fn oid(self) -> u32 {
        match self {
            PgType::Bool => oid::BOOL,
            PgType::Int2 => oid::INT2,
            PgType::Int4 => oid::INT4,
            PgType::Int8 => oid::INT8,
            PgType::Float4 => oid::FLOAT4,
            PgType::Float8 => oid::FLOAT8,
            PgType::Date => oid::DATE,
            PgType::Timestamp => oid::TIMESTAMP,
            PgType::Timestamptz => oid::TIMESTAMPTZ,
            PgType::Text => oid::TEXT,
            PgType::Varchar(_) => oid::VARCHAR,
            PgType::Bytea => oid::BYTEA,
        }
    }

    /// The type's modifier: the length of a `varchar(n)` plus 4, and -1
    /// for every other type, a `timestamp(6)` given as -1 too, the
    /// precision PostgreSQL keeps without one.
    pub const fn typmod(self) -> i32 {
        match self {
            // A VarcharLength is at most MAX_VARCHAR_LENGTH, which fits.
            PgType::Varchar(Some(length)) => length.get() as i32 + VARHDRSZ,
            _ => -1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(oid: u32, typmod: i32) -> bool {
        PgType::new(oid, typmod).is_err_and(|e| e.kind() == ErrorKind::NotSupported)
    }

    #[test]
    fn the_oids_are_postgresql_s() {
        // pg_type.dat of PostgreSQL, written here apart from the crate's
        // constants.
        let oids = [
            (oid::BOOL, 16),
            (oid::BYTEA, 17),
            (oid::INT8, 20),
            (oid::INT2, 21),
            (oid::INT4, 23),
            (oid::TEXT, 25),
            (oid::FLOAT4, 700),
            (oid::FLOAT8, 701),
            (oid::VARCHAR, 1043),
            (oid::DATE, 1082),
            (oid::TIMESTAMP, 1114),
            (oid::TIMESTAMPTZ, 1184),
        ];
        for (constant, postgresql) in oids {
            assert_eq!(constant, postgresql);
        }
    }

    #[test]
    fn every_known_type_and_its_modifiers() {
        let plain = [
            (16, PgType::Bool),
            (21, PgType::Int2),
            (23, PgType::Int4),
            (20, PgType::Int8),
            (700, PgType::Float4),
            (701, PgType::Float8),
            (1082, PgType::Date),
            (25, PgType::Text),
            (17, PgType::Bytea),
        ];
        for (oid, ty) in plain {
            assert_eq!(PgType::new(oid, -1), Ok(ty));
            assert_eq!((ty.oid(), ty.typmod()), (oid, -1));
            assert!(refused(oid, 0), "{ty:?} with a modifier");
            assert!(refused(oid, 8), "{ty:?} with a modifier");
        }
        for (oid, ty) in [(1114, PgType::Timestamp), (1184, PgType::Timestamptz)] {
            assert_eq!(PgType::new(oid, -1), Ok(ty));
            assert_eq!(PgType::new(oid, 6), Ok(ty));
            assert_eq!((ty.oid(), ty.typmod()), (oid, -1));
            for precision in [0, 3, 5, 7] {
                assert!(refused(oid, precision), "{ty:?}({precision})");
            }
        }
    }

    #[test]
    fn a_varchar_by_its_length() {
        for typmod in [-1, 5, 14, 10_485_764] {
            let ty = PgType::new(1043, typmod).unwrap();
            assert_eq!((ty.oid(), ty.typmod()), (1043, typmod));
        }
        assert_eq!(PgType::new(oid::VARCHAR, -1), Ok(PgType::Varchar(None)));
        let length = |n| Some(VarcharLength::new(n).unwrap());
        assert_eq!(PgType::new(oid::VARCHAR, 5), Ok(PgType::Varchar(length(1))));
        assert_eq!(
            PgType::new(oid::VARCHAR, 10_485_764),
            Ok(PgType::Varchar(length(MAX_VARCHAR_LENGTH)))
        );
        assert_eq!(length(10).map(VarcharLength::get), Some(10));
        for typmod in [-2, 0, 3, 4, 10_485_765, i32::MIN, i32::MAX] {
            assert!(refused(oid::VARCHAR, typmod), "varchar typmod {typmod}");
        }
        for characters in [0, MAX_VARCHAR_LENGTH + 1, u32::MAX] {
            assert!(
                VarcharLength::new(characters).is_err_and(|e| e.kind() == ErrorKind::NotSupported),
                "length {characters}"
            );
        }
    }

    #[test]
    fn types_outside_the_set() {
        // numeric, interval, uuid, char(n), json, oid.
        for oid in [1700, 1186, 2950, 1042, 114, 26, 0, u32::MAX] {
            assert!(refused(oid, -1), "oid {oid}");
        }
    }
}

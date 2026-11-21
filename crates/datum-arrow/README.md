# datum-arrow

The boundary between PostgreSQL's types and the types of Apache Arrow,
the form of columns in memory into which Arrow's implementations read
files such as Parquet and from which they write them. A reader of such
files into PostgreSQL, or a writer of PostgreSQL's rows into them,
crosses this boundary on every value; the crate keeps its rules in one
place.

- **A plan checked once.** A column's conversion is a plan made once,
  from the two types and the column's options, before any value is
  converted. It refuses a pair of types it cannot convert, and names
  every way a value may still fail, so that the work per value is the
  plan's checks and nothing more.
- **PostgreSQL's representations.** The conversions work on the forms a
  value has inside a PostgreSQL server, as a `Datum` or a value's bytes
  hold them, in the machine's byte order: a `date` as days since
  2000-01-01, a `timestamp` as microseconds since 2000-01-01 00:00:00, a
  `text` as its bytes without PostgreSQL's header of length. They need
  no server to run.
- **Arrow's types.** The crate speaks [`arrow_schema::DataType`], which
  it re-exports, and depends on no other part of Arrow.
- **Errors with their SQLSTATE.** A refusal or a failure is an
  [`Error`] whose [`ErrorKind`] gives the SQLSTATE a PostgreSQL server
  raises for it; no call panics.

# Types

[`PgType`] lists the PostgreSQL types the crate knows, by their object
identifiers and type modifiers; [`ArrowType`] the Arrow types it takes.

```
use datum_arrow::{
    ArrowType, ErrorKind, PgType, Plain, VarcharLength, arrow_schema::DataType, oid,
};

let ty = PgType::new(oid::VARCHAR, 14)?;
assert_eq!(ty, PgType::Varchar(Some(VarcharLength::new(10)?)));

assert_eq!(
    ArrowType::new(&DataType::UInt16)?,
    ArrowType::Plain(Plain::UInt16)
);
let error = ArrowType::new(&DataType::UInt64).unwrap_err();
assert_eq!(error.kind(), ErrorKind::NotSupported);
assert_eq!(error.sqlstate(), "0A000");
# Ok::<(), datum_arrow::Error>(())
```

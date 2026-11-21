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

# Reading

A [`ReadPlan`] is made from an Arrow type, a PostgreSQL type and the
column's [`ReadOptions`]; [`ReadPlan::read`] converts the slots a mask
asks for of an [`Input`], Arrow's buffers borrowed from the caller,
into a [`ReadOutput`]: `Datum` words for types PostgreSQL passes by
value, slices of the input's bytes for `text`, `varchar` and `bytea`.

```
use datum_arrow::{
    Column, ErrorKind, Failures, Input, PgType, ReadOptions, ReadOutput, ReadPlan, Unit,
    Values, arrow_schema::DataType,
};

// Seconds since 1970 in an Int64, read into timestamp.
let options = ReadOptions { unit: Some(Unit::Seconds), text: None, server_utf8: true };
let plan = ReadPlan::new(&DataType::Int64, PgType::Timestamp, options)?;
assert_eq!(plan.failures(), Failures::OUT_OF_RANGE);

let seconds = [946_684_800i64, 946_684_801, i64::MAX];
let input = Input::Plain(Column { len: 3, values: Values::Int64(&seconds), nulls: None });
let mut values = [0u64; 3];
let mut nulls = [0u64; 1];
let mut output = ReadOutput::Datums { values: &mut values, nulls: &mut nulls };

// The first two slots: 2000-01-01 00:00:00 and one second later, as
// microseconds since 2000.
plan.read(&input, &[0b011], &mut output)?;
assert_eq!(values[..2], [0, 1_000_000]);

// The third is past PostgreSQL's last time.
let mut output = ReadOutput::Datums { values: &mut values, nulls: &mut nulls };
let error = plan.read(&input, &[0b100], &mut output).unwrap_err();
assert_eq!((error.kind(), error.slot()), (ErrorKind::OutOfRange, Some(2)));
# Ok::<(), datum_arrow::Error>(())
```

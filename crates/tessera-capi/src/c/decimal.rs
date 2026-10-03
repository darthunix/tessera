//! The decimal entry points, declared in `include/tessera/decimal.h`:
//! numeric Datums read in place as decimals
//! ([`tessera_kernels::decimal`]), compared, computed, cast to integers and
//! written back as numerics.
//!
//! A numeric Datum points to a varlena. On a little-endian machine a
//! first byte with the low bit set is a 1-byte header (0x01 alone: an
//! external pointer, never read here) holding the size with the header in
//! its upper seven bits; two low bits of zero, a 4-byte header holding the
//! size in its upper 30 bits; two low bits of 10, a compressed value, never
//! read here either. The bytes after the header are the numeric's.
//!
//! # Arguments
//!
//! A `TessDecimalArg` points to a column or holds a scalar numeric Datum.
//! A column must be a valid `TessDatumColumn` of the call's row count whose
//! `values` and `isnull` hold that many elements and whose `decimal_rows`,
//! when set, hold its words, all unchanged for the call; a selected row's
//! flag must be initialized and, when not NULL, its value: a numeric Datum
//! pointing to a whole varlena or, in the decimal side, the decimal's
//! value. A scalar must point to a whole varlena.

use std::ffi::{c_int, c_uint, c_void};
use std::marker::PhantomData;
use std::mem::{MaybeUninit, offset_of};
use std::slice;

use anyhow::{Context, Result, bail, ensure};
use tessera_core::{RowMask, RowMaskView, ones};
use tessera_kernels::decimal::{
    self, Arg, Compare, Decimal, DecimalWord, MAX_SCALE, NUMERIC_MAX, Op, Partial, Partials,
    Results, Scales, Source, Special, Sum, SumState, Term, Terms,
};

use super::column::DatumColumn;
use super::mask::Mask;
use super::status::{Code, Status, guard};
use super::table::{SUM_INT4, SUM_INT8, SUM_NUMERIC, SUM_STATE};
use super::varlena::varlena_data;

/// `TessDecimalArg`: a column, or a scalar numeric when it is null.
#[repr(C)]
#[derive(Debug)]
pub struct DecimalArg {
    /// The column, or null.
    pub column: *const DatumColumn,
    /// The scalar when `column` is null: a numeric Datum.
    pub scalar: u64,
}

/// The decimal of a numeric Datum.
///
/// # Safety
///
/// As for [`varlena_data`].
#[inline(always)]
unsafe fn read_numeric(datum: u64) -> Arg {
    // SAFETY: the caller's contract.
    match unsafe { varlena_data(datum) }.and_then(Decimal::read) {
        Some(decimal) => Arg::Decimal(decimal),
        None => Arg::Other,
    }
}

/// A column's values and flags by row, read through raw pointers behind
/// one check of the row against the row count, which bounds every array;
/// fewer live lengths keep the loops in registers.
pub(super) struct Plain<'a> {
    values: *const u64,
    isnull: *const u8,
    nrows: usize,
    borrow: PhantomData<&'a ()>,
}

impl Plain<'_> {
    /// The row's argument: NULL, or its numeric read.
    #[inline(always)]
    fn get(&self, row: usize) -> Arg {
        assert!(row < self.nrows, "a decimal row past its column");
        // SAFETY: the row is within the arrays, and the calls read selected
        // rows only, whose flags and non-NULL values the constructor's
        // caller guarantees; a flag is read as its byte.
        unsafe {
            if *self.isnull.add(row) != 0 {
                return Arg::Null;
            }
            read_numeric(*self.values.add(row))
        }
    }

    /// The row's Datum as stored.
    #[inline(always)]
    fn datum(&self, row: usize) -> u64 {
        assert!(row < self.nrows, "a decimal row past its column");
        // SAFETY: as for `get`; a NULL row's placeholder is initialized too.
        unsafe { *self.values.add(row) }
    }

    /// The row's Datum, `None` for NULL.
    #[inline(always)]
    fn value(&self, row: usize) -> Option<u64> {
        assert!(row < self.nrows, "a decimal row past its column");
        // SAFETY: as for `get`.
        unsafe { (*self.isnull.add(row) == 0).then(|| *self.values.add(row)) }
    }
}

/// A column with its decimal side: the rows whose value is a decimal of
/// one scale, not a numeric.
pub(super) struct Side<'a> {
    column: Plain<'a>,
    decimals: *const u64,
    scale: u32,
}

impl Side<'_> {
    /// The row's argument: NULL, its decimal, or its numeric read.
    #[inline(always)]
    fn get(&self, row: usize) -> Arg {
        assert!(row < self.column.nrows, "a decimal row past its column");
        // SAFETY: as for `Plain::get`, and the side's words cover the rows.
        unsafe {
            if *self.column.isnull.add(row) != 0 {
                return Arg::Null;
            }
            let value = *self.column.values.add(row);
            if *self.decimals.add(row / 64) >> (row % 64) & 1 == 1 {
                return Decimal::new(value as i64, self.scale).map_or(Arg::Other, Arg::Decimal);
            }
            read_numeric(value)
        }
    }
}

/// A column argument, with or without its decimal side: a loop for each.
enum Column<'a> {
    Plain(Plain<'a>),
    Side(Side<'a>),
}

impl<'a> Column<'a> {
    /// Borrow a column of `nrows` rows.
    ///
    /// # Safety
    ///
    /// `column` must point to a valid `TessDatumColumn` whose `values` and
    /// `isnull` hold its row count of elements, `decimal_rows`, when set,
    /// its words, all valid and unchanged for `'a`. A row the calls read
    /// (a selected row) must have an initialized flag and, when not NULL,
    /// an initialized value: a numeric Datum satisfying [`varlena_data`],
    /// or, in the decimal side, the decimal's value.
    #[inline]
    unsafe fn new(column: *const DatumColumn, nrows: usize) -> Result<Self> {
        // SAFETY: the caller's contract.
        let column = unsafe { column.as_ref() }.context("a null column")?;
        ensure!(
            column.struct_size >= DatumColumn::MIN_SIZE,
            "a Datum column is smaller than its required fields"
        );
        ensure!(
            usize::try_from(column.nrows).ok() == Some(nrows),
            "a numeric column has another row count than its rows"
        );
        ensure!(
            nrows == 0 || (!column.values.is_null() && !column.isnull.is_null()),
            "a column has null buffers"
        );
        let plain = Plain {
            values: column.values,
            isnull: column.isnull.cast(),
            nrows,
            borrow: PhantomData,
        };
        if nrows == 0 || column.struct_size < DECIMALS_SIZE || column.decimal_rows.is_null() {
            return Ok(Self::Plain(plain));
        }
        match u32::try_from(column.decimal_scale)
            .ok()
            .filter(|&scale| scale <= decimal::MAX_READ_SCALE)
        {
            Some(scale) => Ok(Self::Side(Side {
                column: plain,
                decimals: column.decimal_rows,
                scale,
            })),
            // A side without decimals may have no scale either.
            None => {
                // SAFETY: the caller guarantees the words of the decimal side.
                let words =
                    unsafe { slice::from_raw_parts(column.decimal_rows, nrows.div_ceil(64)) };
                ensure!(
                    words.iter().all(|&word| word == 0),
                    "a column's decimals have an invalid scale"
                );
                Ok(Self::Plain(plain))
            }
        }
    }

    /// The row's Datum as stored.
    #[inline(always)]
    fn datum(&self, row: usize) -> u64 {
        match self {
            Self::Plain(column) => column.datum(row),
            Self::Side(side) => side.column.datum(row),
        }
    }
}

/// The size of `TessDatumColumn` through `decimal_scale`
/// (`TESS_DATUM_COLUMN_DECIMALS_SIZE`).
const DECIMALS_SIZE: usize = offset_of!(DatumColumn, decimal_scale) + size_of::<c_int>();

/// A call's argument.
enum Input<'a> {
    Column(Column<'a>),
    Scalar(Arg),
}

impl Input<'_> {
    /// The argument of a call with `nrows` rows.
    ///
    /// # Safety
    ///
    /// `arg` must point to a valid `TessDecimalArg`: its column satisfies
    /// [`Column::new`], or its scalar [`varlena_data`], for the call.
    #[inline]
    unsafe fn new(arg: *const DecimalArg, nrows: usize) -> Result<Self> {
        // SAFETY: the caller's contract.
        unsafe {
            let arg = arg.as_ref().context("a null decimal argument")?;
            Ok(if arg.column.is_null() {
                Self::Scalar(read_numeric(arg.scalar))
            } else {
                Self::Column(Column::new(arg.column, nrows)?)
            })
        }
    }
}

impl Source for Plain<'_> {
    #[inline(always)]
    fn get(&self, row: usize) -> Arg {
        Plain::get(self, row)
    }
}

impl Source for Side<'_> {
    #[inline(always)]
    fn get(&self, row: usize) -> Arg {
        Side::get(self, row)
    }
}

/// The term of a sum of a numeric Datum: a decimal, NaN or an infinity,
/// or a value the caller adds.
///
/// # Safety
///
/// As for [`varlena_data`].
#[inline(always)]
unsafe fn numeric_term(datum: u64) -> Term {
    // SAFETY: the caller's contract.
    match unsafe { varlena_data(datum) } {
        None => Term::Other,
        Some(data) => match Decimal::read(data) {
            Some(decimal) => Term::Decimal(decimal),
            None => Special::read(data).map_or(Term::Other, Term::Special),
        },
    }
}

impl Terms for Plain<'_> {
    /// The rows among `rows` whose numeric is a decimal of the scale of the
    /// word's first one, handed on.
    #[inline(always)]
    fn fold_decimals(
        &self,
        index: usize,
        rows: u64,
        mut add: impl FnMut(usize, i64),
    ) -> Option<DecimalWord> {
        let base = index * 64;
        let mut scale = None;
        let mut bulk = 0;
        let look = rows;
        for bit in ones(look) {
            let row = base + bit;
            assert!(row < self.nrows, "a decimal row past its column");
            // SAFETY: as for `Plain::get`.
            let decimal = unsafe {
                if *self.isnull.add(row) != 0 {
                    continue;
                }
                varlena_data(*self.values.add(row)).and_then(Decimal::read)
            };
            if let Some(decimal) = decimal
                && *scale.get_or_insert(decimal.scale()) == decimal.scale()
            {
                add(bit, decimal.value());
                bulk |= 1 << bit;
            }
        }
        Some(DecimalWord {
            rows: bulk,
            scale: scale.unwrap_or(0),
        })
    }

    #[inline(always)]
    fn term(&self, row: usize) -> Term {
        assert!(row < self.nrows, "a decimal row past its column");
        // SAFETY: as for `Plain::get`.
        unsafe {
            if *self.isnull.add(row) != 0 {
                return Term::Null;
            }
            numeric_term(*self.values.add(row))
        }
    }
}

/// Whether a value is a decimal's, below 10^18 in magnitude.
#[inline(always)]
fn decimal_value(value: i64) -> bool {
    const LIMIT: i64 = decimal::POWERS[decimal::DIGITS as usize];
    value.unsigned_abs() < LIMIT as u64
}

impl Terms for Side<'_> {
    /// The side's rows among `rows` with a decimal, not NULL, handed on.
    #[inline(always)]
    fn fold_decimals(
        &self,
        index: usize,
        rows: u64,
        mut add: impl FnMut(usize, i64),
    ) -> Option<DecimalWord> {
        let base = index * 64;
        let column = &self.column;
        // SAFETY: the side's words cover the rows, and a selected row's
        // flag and value are initialized (the constructor's contract); rows
        // are read only among those asked for, each checked against the
        // row count.
        unsafe {
            let mut bulk = 0;
            let look = rows & *self.decimals.add(index);
            for bit in ones(look) {
                let row = base + bit;
                assert!(row < column.nrows, "a decimal row past its column");
                let value = *column.values.add(row) as i64;
                if *column.isnull.add(row) == 0 && decimal_value(value) {
                    add(bit, value);
                    bulk |= 1 << bit;
                }
            }
            Some(DecimalWord {
                rows: bulk,
                scale: self.scale,
            })
        }
    }

    #[inline(always)]
    fn term(&self, row: usize) -> Term {
        assert!(row < self.column.nrows, "a decimal row past its column");
        // SAFETY: as for `Side::get`.
        unsafe {
            if *self.column.isnull.add(row) != 0 {
                return Term::Null;
            }
            let value = *self.column.values.add(row);
            if *self.decimals.add(row / 64) >> (row % 64) & 1 == 1 {
                return Decimal::new(value as i64, self.scale).map_or(Term::Other, Term::Decimal);
            }
            numeric_term(value)
        }
    }
}

/// An integer column's terms: each value a decimal at scale 0, an int8
/// of 19 digits one the caller adds; int4 words (`WIDE` false) read as
/// their low half.
pub(super) struct Integers<'a, const WIDE: bool> {
    column: Plain<'a>,
}

impl<const WIDE: bool> Terms for Integers<'_, WIDE> {
    /// Every row among `rows` that is not NULL, of a decimal's magnitude,
    /// handed on.
    #[inline(always)]
    fn fold_decimals(
        &self,
        index: usize,
        rows: u64,
        mut add: impl FnMut(usize, i64),
    ) -> Option<DecimalWord> {
        let column = &self.column;
        let base = index * 64;
        // SAFETY: a selected row's flag and value are initialized (the
        // constructor's contract); rows are read only among those asked
        // for, each checked against the row count.
        unsafe {
            let mut bulk = 0;
            let look = rows;
            for bit in ones(look) {
                let row = base + bit;
                assert!(row < column.nrows, "an integer row past its column");
                let word = *column.values.add(row);
                let value = if WIDE {
                    word as i64
                } else {
                    i64::from(word as i32)
                };
                if *column.isnull.add(row) == 0 && decimal_value(value) {
                    add(bit, value);
                    bulk |= 1 << bit;
                }
            }
            Some(DecimalWord {
                rows: bulk,
                scale: 0,
            })
        }
    }

    #[inline(always)]
    fn term(&self, row: usize) -> Term {
        let column = &self.column;
        assert!(row < column.nrows, "an integer row past its column");
        // SAFETY: as for `Plain::get`; a word is read whatever it holds.
        unsafe {
            if *column.isnull.add(row) != 0 {
                return Term::Null;
            }
            let word = *column.values.add(row);
            let value = if WIDE {
                word as i64
            } else {
                i64::from(word as i32)
            };
            Decimal::new(value, 0).map_or(Term::Other, Term::Decimal)
        }
    }
}

/// How `tess_table_accumulate_sums` reads a column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SumInput {
    /// numeric Datums, with or without the column's decimals.
    Numeric,
    /// int4 words (int2 too, as the column holds it).
    Int4,
    /// int8 words.
    Int8,
    /// Partial sum states of the node's own format, bytea Datums
    /// ([`state_partial`]).
    State,
    /// Partial states of `avg(int4)` and `avg(int2)`, int8[] Datums of the
    /// count and the sum ([`pair_partial`]).
    Pair,
}

/// A sum's column, read by its kind: one loop runs over the sums of a
/// call, so the kind is matched a row.
pub(super) enum SumColumn<'a> {
    Numeric(Plain<'a>),
    Side(Side<'a>),
    Int4(Integers<'a, false>),
    Int8(Integers<'a, true>),
}

impl Terms for SumColumn<'_> {
    #[inline(always)]
    fn fold_decimals(
        &self,
        index: usize,
        rows: u64,
        add: impl FnMut(usize, i64),
    ) -> Option<DecimalWord> {
        match self {
            Self::Numeric(column) => column.fold_decimals(index, rows, add),
            Self::Side(side) => side.fold_decimals(index, rows, add),
            Self::Int4(column) => column.fold_decimals(index, rows, add),
            Self::Int8(column) => column.fold_decimals(index, rows, add),
        }
    }

    #[inline(always)]
    fn term(&self, row: usize) -> Term {
        match self {
            Self::Numeric(column) => column.term(row),
            Self::Side(side) => side.term(row),
            Self::Int4(column) => column.term(row),
            Self::Int8(column) => column.term(row),
        }
    }
}

impl<'a> SumColumn<'a> {
    /// A sum's column of `nrows` rows.
    ///
    /// # Safety
    ///
    /// `column` must satisfy [`Column::new`] for `nrows` rows, with values
    /// of `input`'s kind, for `'a`.
    pub(super) unsafe fn new(
        input: SumInput,
        column: *const DatumColumn,
        nrows: usize,
    ) -> Result<Self> {
        // SAFETY: the caller's contract.
        let column = unsafe { Column::new(column, nrows)? };
        Ok(match (input, column) {
            (SumInput::State | SumInput::Pair, _) => bail!("partial states among a call's terms"),
            (SumInput::Numeric, Column::Plain(column)) => Self::Numeric(column),
            (SumInput::Numeric, Column::Side(side)) => Self::Side(side),
            (SumInput::Int4, Column::Plain(column)) => Self::Int4(Integers { column }),
            (SumInput::Int8, Column::Plain(column)) => Self::Int8(Integers { column }),
            (_, Column::Side(_)) => bail!("an integer column with decimals"),
        })
    }
}

/// `TESS_TABLE_SUM_STATE_TAG`: the first bytes of a partial sum state.
const STATE_TAG: u32 = 0x5453_4D31;

/// The bytes after the varlena header of a partial sum state without a
/// rest: the tag and the state's words.
const STATE_BYTES: usize = 4 + 8 * SumState::WORDS;

/// The flags word's bits a state may set: the scale and the three flags.
const STATE_FLAG_BITS: u64 = 0x7FF;

/// A partial sum state of the node's own format, the bytes after a bytea's
/// header: the tag, the state's words in the machine's order, then its
/// numeric rest, whole with its header, when it has one, which the caller
/// merges.
fn state_partial(data: &[u8]) -> Result<Partial> {
    ensure!(
        data.len() >= STATE_BYTES
            && data.first_chunk::<4>().map(|tag| u32::from_ne_bytes(*tag)) == Some(STATE_TAG),
        "a partial sum state of another format"
    );
    if data.len() > STATE_BYTES {
        return Ok(Partial::Other);
    }
    let words: [u64; SumState::WORDS] = std::array::from_fn(|word| {
        let at = 4 + 8 * word;
        let mut bytes = [0; 8];
        bytes.copy_from_slice(&data[at..at + 8]);
        u64::from_ne_bytes(bytes)
    });
    let state = SumState::from_words(words);
    ensure!(
        words[3] & !STATE_FLAG_BITS == 0
            && state.sum.scale <= decimal::MAX_READ_SCALE
            && state.sum.value.abs() < decimal::SUM_BOUND,
        "a partial sum state out of its range"
    );
    Ok(Partial::State(state))
}

/// `INT8OID`.
const INT8_OID: i32 = 20;

/// The partial state of `avg(int4)` or `avg(int2)`, the core's int8[] of
/// the count and the sum, the bytes after its varlena header: one
/// dimension, no NULL bitmap (data offset 0), element type int8, a length
/// of 2 and its lower bound, then the two int8.
fn pair_partial(data: &[u8]) -> Result<Partial> {
    let int = |at: usize| {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(&data[at..at + 4]);
        i32::from_ne_bytes(bytes)
    };
    let int8 = |at: usize| {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(&data[at..at + 8]);
        i64::from_ne_bytes(bytes)
    };
    ensure!(
        data.len() == 36 && int(0) == 1 && int(4) == 0 && int(8) == INT8_OID && int(12) == 2,
        "a partial average of another format"
    );
    let count = u64::try_from(int8(20)).context("a partial average of a negative count")?;
    Ok(Partial::State(SumState {
        sum: Sum {
            value: i128::from(int8(28)),
            scale: 0,
            count,
        },
        ..SumState::default()
    }))
}

/// A sum's column of partial states, read by its kind.
pub(super) enum PartialColumn<'a> {
    /// The node's own partial sum states.
    State(Plain<'a>),
    /// The int8[] pairs of `avg(int4)` and `avg(int2)`.
    Pair(Plain<'a>),
    /// The int8 of `sum(int2)`: a state of one value at scale 0, as a
    /// bigint term adds.
    Int8(Plain<'a>),
}

/// A reader of a partial state's bytes after the varlena header.
type ReadPartial = fn(&[u8]) -> Result<Partial>;

impl Partials for PartialColumn<'_> {
    #[inline]
    fn partial(&self, row: usize) -> Result<Partial> {
        let (column, read): (&Plain<'_>, ReadPartial) = match self {
            Self::State(column) => (column, state_partial),
            Self::Pair(column) => (column, pair_partial),
            Self::Int8(column) => {
                return Ok(column.value(row).map_or(Partial::Null, |word| {
                    Partial::State(SumState {
                        sum: Sum {
                            value: i128::from(word as i64),
                            scale: 0,
                            count: 1,
                        },
                        ..SumState::default()
                    })
                }));
            }
        };
        let Some(datum) = column.value(row) else {
            return Ok(Partial::Null);
        };
        // SAFETY: the constructor's caller guarantees a whole varlena
        // behind a selected row's Datum.
        match unsafe { varlena_data(datum) } {
            None => Ok(Partial::Other),
            Some(data) => read(data),
        }
    }
}

impl<'a> PartialColumn<'a> {
    /// A sum's column of partial states of `nrows` rows.
    ///
    /// # Safety
    ///
    /// `column` must satisfy [`Column::new`] for `nrows` rows, its values
    /// varlena Datums of `input`'s kind, for `'a`.
    pub(super) unsafe fn new(
        input: SumInput,
        column: *const DatumColumn,
        nrows: usize,
    ) -> Result<Self> {
        // SAFETY: the caller's contract.
        let column = unsafe { Column::new(column, nrows)? };
        Ok(match (input, column) {
            (SumInput::State, Column::Plain(column)) => Self::State(column),
            (SumInput::Pair, Column::Plain(column)) => Self::Pair(column),
            (SumInput::Int8, Column::Plain(column)) => Self::Int8(column),
            (SumInput::State | SumInput::Pair | SumInput::Int8, Column::Side(_)) => {
                bail!("a column of partial states with decimals")
            }
            _ => bail!("terms among a call's partial states"),
        })
    }
}

/// A scalar argument, the same for every row.
struct Constant(Arg);

impl Source for Constant {
    #[inline(always)]
    fn get(&self, _row: usize) -> Arg {
        self.0
    }
}

/// Run `$body` with `$source` bound to the concrete source of a column, so
/// that a column with its decimal side and one without each get a loop.
macro_rules! with_column {
    ($column:expr, |$source:ident| $body:expr) => {
        match $column {
            Column::Plain(column) => {
                let $source = &column;
                $body
            }
            Column::Side(column) => {
                let $source = &column;
                $body
            }
        }
    };
}

/// [`with_column!`] over an input, a scalar too.
macro_rules! with_source {
    ($input:expr, |$source:ident| $body:expr) => {
        match $input {
            Input::Column(column) => with_column!(column, |$source| $body),
            Input::Scalar(arg) => {
                let $source = &Constant(arg);
                $body
            }
        }
    };
}

/// [`with_source!`] over two inputs: a loop for each pair of shapes.
macro_rules! with_sources {
    ($left:expr, $right:expr, |$l:ident, $r:ident| $body:expr) => {
        with_source!($left, |$l| with_source!($right, |$r| $body))
    };
}

/// A selection to read.
///
/// # Safety
///
/// `rows` must point to a valid mask, unchanged for `'a`.
#[inline]
unsafe fn selection<'a>(rows: *const Mask) -> Result<RowMaskView<'a>> {
    // SAFETY: the caller's contract.
    unsafe { rows.as_ref().context("a null row mask")?.view() }
}

/// A mask to write.
///
/// # Safety
///
/// `mask` must point to a valid mask that nothing else accesses for `'a`.
#[inline]
unsafe fn output<'a>(mask: *mut Mask) -> Result<RowMask<'a>> {
    // SAFETY: the caller's contract.
    unsafe { mask.as_mut().context("a null result mask")?.mask() }
}

/// An array of `nrows` slots to write.
///
/// # Safety
///
/// `values` must point to `nrows` writable slots of `T`, possibly
/// uninitialized, that nothing else accesses for `'a`.
#[inline]
unsafe fn slots<'a, T>(values: *mut T, nrows: usize) -> Result<&'a mut [MaybeUninit<T>]> {
    if nrows == 0 {
        return Ok(&mut []);
    }
    ensure!(!values.is_null(), "a null result buffer");
    // SAFETY: the caller's contract.
    Ok(unsafe { slice::from_raw_parts_mut(values.cast(), nrows) })
}

fn compare_op(op: c_int) -> Result<Compare> {
    Ok(match op {
        0 => Compare::Eq,
        1 => Compare::Ne,
        2 => Compare::Lt,
        3 => Compare::Le,
        4 => Compare::Gt,
        5 => Compare::Ge,
        _ => anyhow::bail!("an unknown decimal comparison {op}"),
    })
}

fn compute_op(op: c_int) -> Result<Op> {
    Ok(match op {
        0 => Op::Add,
        1 => Op::Sub,
        2 => Op::Mul,
        3 => Op::Negate,
        4 => Op::Abs,
        _ => anyhow::bail!("an unknown decimal operation {op}"),
    })
}

/// `tess_decimal_filter`: `rows` narrowed to the rows where the comparison
/// (a `TessCompareOp`) of two decimals holds, the rows whose arguments are
/// not both decimals moved to `rest`.
///
/// # Safety
///
/// `left` and `right` must be valid arguments (see the module documentation)
/// of `rows`' row count,
/// `rows` and `rest` point to valid masks that nothing else accesses for
/// the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_decimal_filter(
    op: c_int,
    left: *const DecimalArg,
    right: *const DecimalArg,
    rows: *mut Mask,
    rest: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let op = compare_op(op)?;
            let mut rows = output(rows)?;
            let nrows = rows.as_view().nrows();
            let left = Input::new(left, nrows)?;
            let right = Input::new(right, nrows)?;
            let mut rest = output(rest)?;
            with_sources!(left, right, |left, right| decimal::filter(
                op, left, right, &mut rows, &mut rest
            ))
        })
    }
}

/// `tess_decimal_compute`: an operation (a `TessDecimalOp`) over the
/// selected rows, with the rows it leaves to the caller in `rest`.
///
/// # Safety
///
/// `left`, and `right` for a binary operation, must be valid arguments
/// (see the module documentation) of `rows`' row count; `right` may be null otherwise.
/// `values` and `scales` must point to as many writable slots as `rows`
/// has rows, and `non_nulls`, `decimals` and `rest` to valid masks, none of
/// them accessed by anything else for the call; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_decimal_compute(
    op: c_int,
    left: *const DecimalArg,
    right: *const DecimalArg,
    rows: *const Mask,
    scale: c_int,
    values: *mut i64,
    scales: *mut u8,
    non_nulls: *mut Mask,
    decimals: *mut Mask,
    rest: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let op = compute_op(op)?;
            let rows = selection(rows)?;
            let nrows = rows.nrows();
            let left = Input::new(left, nrows)?;
            let right = if op.binary() {
                Some(Input::new(right, nrows)?)
            } else {
                None
            };
            let scale = u32::try_from(scale).ok();
            let mut results = Results {
                values: slots(values, nrows)?,
                scales: slots(scales, nrows)?,
                non_nulls: output(non_nulls)?,
                decimals: output(decimals)?,
                rest: output(rest)?,
            };
            match right {
                Some(right) => with_sources!(left, right, |left, right| decimal::compute(
                    op,
                    left,
                    right,
                    rows,
                    scale,
                    &mut results
                )),
                None => with_source!(left, |left| decimal::compute(
                    op,
                    left,
                    &Constant(Arg::Null),
                    rows,
                    scale,
                    &mut results
                )),
            }
        })
    }
}

/// `tess_decimal_to_int4`: `int4(numeric)` over the selected rows, a row
/// that is not a decimal or passes the int4 range moved to `rest`.
///
/// # Safety
///
/// `arg` must be a valid argument (see the module documentation) of
/// `rows`' row count, `values` point
/// to as many writable slots, `non_nulls` and `rest` to valid masks, none
/// of them accessed by anything else for the call; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_decimal_to_int4(
    arg: *const DecimalArg,
    rows: *const Mask,
    values: *mut i32,
    non_nulls: *mut Mask,
    rest: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let rows = selection(rows)?;
            let arg = Input::new(arg, rows.nrows())?;
            let values = slots(values, rows.nrows())?;
            let (mut non_nulls, mut rest) = (output(non_nulls)?, output(rest)?);
            with_source!(arg, |arg| decimal::to_int4(
                arg,
                rows,
                values,
                &mut non_nulls,
                &mut rest
            ))
        })
    }
}

/// `tess_decimal_to_int8`: `int8(numeric)` over the selected rows, a row
/// that is not a decimal moved to `rest`.
///
/// # Safety
///
/// As for [`tess_decimal_to_int4`], with `values` of int8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_decimal_to_int8(
    arg: *const DecimalArg,
    rows: *const Mask,
    values: *mut i64,
    non_nulls: *mut Mask,
    rest: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let rows = selection(rows)?;
            let arg = Input::new(arg, rows.nrows())?;
            let values = slots(values, rows.nrows())?;
            let (mut non_nulls, mut rest) = (output(non_nulls)?, output(rest)?);
            with_source!(arg, |arg| decimal::to_int8(
                arg,
                rows,
                values,
                &mut non_nulls,
                &mut rest
            ))
        })
    }
}

/// `tess_decimal_read`: the selected rows of a numeric column read as
/// decimals into `values`, every other selected row's Datum copied there;
/// every decimal with its scale in `scales`, or, when `scales` is null,
/// those of `*scale` (the first decimal's when -1, left there).
///
/// # Safety
///
/// `column` must be a valid column (see the module documentation) of
/// `rows`' row count, `rows`
/// point to a valid mask, `values` to as many writable Datum slots,
/// `scales` be null or point to as many writable bytes, `scale` be valid
/// when `scales` is null, and `decimals` point to a valid mask, the
/// outputs accessed by nothing else for the call; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_decimal_read(
    column: *const DatumColumn,
    rows: *const Mask,
    scale: *mut c_int,
    values: *mut u64,
    scales: *mut u8,
    decimals: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let rows = selection(rows)?;
            let nrows = rows.nrows();
            let column = Column::new(column, nrows)?;
            let values = slots(values.cast::<i64>(), nrows)?;
            let mut decimals = output(decimals)?;
            let mut uniform = None;
            let scales = if scales.is_null() {
                let scale = scale.as_mut().context("a null scale")?;
                uniform = u32::try_from(*scale).ok();
                ensure!(
                    *scale == -1 || uniform.is_some_and(|scale| scale <= decimal::MAX_READ_SCALE),
                    "an invalid decimal scale {}",
                    *scale
                );
                Scales::Uniform(&mut uniform)
            } else {
                Scales::ByRow(slots(scales, nrows)?)
            };
            let by_row = matches!(scales, Scales::ByRow(_));
            match &column {
                Column::Plain(source) => decimal::read(source, rows, scales, values, &mut decimals),
                Column::Side(source) => decimal::read(source, rows, scales, values, &mut decimals),
            }?;
            // The other selected rows keep their Datums.
            for word in 0..nrows.div_ceil(64) {
                let look = rows.word_at(word) & !decimals.as_view().word_at(word);
                for bit in ones(look) {
                    let row = word * 64 + bit;
                    values[row].write(column.datum(row) as i64);
                }
            }
            if !by_row {
                *scale = uniform.map_or(-1, |scale| scale as c_int);
            }
            Ok(())
        })
    }
}

/// `TessDecimalSum`: a running sum of decimals ([`Sum`]), its `int128`
/// value as two words.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DecimalSum {
    /// The low 64 bits of the value.
    pub low: u64,
    /// The high 64 bits, with the sign.
    pub high: i64,
    /// The display scale of the value.
    pub scale: c_int,
    /// The decimals added.
    pub count: i64,
}

/// `tess_decimal_sum`: the selected rows' values, of a column of `kind`
/// (`TessTableSumInput`: numeric, int4 or int8), added to `*sum` as
/// decimals, or its partial states (the node's own) merged into it
/// ([`decimal::merge_partials`]), the rows it does not take moved to
/// `rest`.
///
/// # Safety
///
/// `column` must be a valid column (see the module documentation) of
/// `rows`' row count with values of `kind`, `rows`
/// point to a valid mask, `sum` to a valid sum, and `rest` to a valid mask
/// that nothing else accesses for the call; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_decimal_sum(
    kind: c_uint,
    column: *const DatumColumn,
    rows: *const Mask,
    sum: *mut DecimalSum,
    rest: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let rows = selection(rows)?;
            let input = match kind {
                SUM_NUMERIC => SumInput::Numeric,
                SUM_INT4 => SumInput::Int4,
                SUM_INT8 => SumInput::Int8,
                SUM_STATE => SumInput::State,
                other => bail!("a sum of input {other} without groups"),
            };
            let sum = sum.as_mut().context("a null sum")?;
            let mut total = Sum {
                value: (i128::from(sum.high) << 64) | i128::from(sum.low),
                scale: u32::try_from(sum.scale).context("a negative scale of a sum")?,
                count: u64::try_from(sum.count).context("a negative count of a sum")?,
            };
            let mut rest = output(rest)?;
            if input == SumInput::State {
                let partials = PartialColumn::new(input, column, rows.nrows())?;
                decimal::merge_partials(&partials, rows, &mut total, &mut rest)?;
            } else {
                let terms = SumColumn::new(input, column, rows.nrows())?;
                decimal::sum_terms(&terms, rows, &mut total, &mut rest)?;
            }
            *sum = DecimalSum {
                low: total.value as u64,
                high: (total.value >> 64) as i64,
                scale: total.scale as c_int,
                count: total.count as i64,
            };
            Ok(())
        })
    }
}

/// `TessDecimalExtreme`: the extreme of `min` or `max` of numeric so far,
/// a decimal when `valid`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DecimalExtreme {
    /// The value at its display scale.
    pub value: i64,
    /// The display scale.
    pub scale: c_int,
    /// Whether `value` and `scale` hold the extreme.
    pub valid: bool,
}

/// `tess_decimal_extreme`: `max` (or `min`) of the selected rows of a
/// numeric column against `*extreme` ([`decimal::extreme`]): `*row` gets
/// the row of the batch's extreme, whose decimal goes to `*extreme`, or
/// -1, and `rest` the rows neither NULL nor decimals.
///
/// # Safety
///
/// `column` must be a valid column (see the module documentation) of
/// `rows`' row count, `rows` point to a valid mask, `extreme` to a valid
/// extreme, `row` to a writable int and `rest` to a valid mask, none of
/// these three accessed by anything else for the call; `status` as for
/// every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_decimal_extreme(
    column: *const DatumColumn,
    rows: *const Mask,
    max: bool,
    extreme: *mut DecimalExtreme,
    row: *mut c_int,
    rest: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let rows = selection(rows)?;
            let terms = SumColumn::new(SumInput::Numeric, column, rows.nrows())?;
            let extreme = extreme.as_mut().context("a null extreme")?;
            let row = row.as_mut().context("a null row")?;
            let mut rest = output(rest)?;
            let state = if extreme.valid {
                let scale = u32::try_from(extreme.scale).context("a negative scale")?;
                Some(Decimal::new(extreme.value, scale).context("an extreme not a decimal")?)
            } else {
                None
            };
            let found = decimal::extreme(&terms, rows, max, state, &mut rest)?;
            *row = match found {
                None => -1,
                Some((at, decimal)) => {
                    *extreme = DecimalExtreme {
                        value: decimal.value(),
                        scale: decimal.scale() as c_int,
                        valid: true,
                    };
                    c_int::try_from(at).context("a row past an int")?
                }
            };
            Ok(())
        })
    }
}

/// `tess_decimal_write`: each selected row's decimal in `values`, at its
/// scale in `scales` or at `scale` when `scales` is null, replaced by the
/// pointer to its numeric, written into `space` one after another at
/// offsets of 8 bytes; `*used` gets the bytes taken.
///
/// # Safety
///
/// `values` must point to as many initialized, writable Datums as `rows`
/// has rows, `scales` be null or point to as many initialized bytes,
/// `rows` point to a valid mask, `space` to `len` writable bytes aligned
/// to 8 and `used` to a writable size, none of them accessed by anything
/// else for the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_decimal_write(
    values: *mut u64,
    scales: *const u8,
    scale: c_int,
    rows: *const Mask,
    space: *mut c_void,
    len: usize,
    used: *mut usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let rows = selection(rows)?;
            let nrows = rows.nrows();
            let used = used.as_mut().context("a null size")?;
            ensure!(
                len == 0 || (!space.is_null() && space.align_offset(8) == 0),
                "a decimal write needs space aligned to 8 bytes"
            );
            let values = if nrows == 0 {
                &mut []
            } else {
                ensure!(!values.is_null(), "a null value buffer");
                slice::from_raw_parts_mut(values, nrows)
            };
            let scales = if scales.is_null() {
                None
            } else {
                Some(slice::from_raw_parts(scales, nrows))
            };
            let space = if len == 0 {
                &mut []
            } else {
                slice::from_raw_parts_mut(space.cast::<u8>(), len)
            };
            let base = space.as_mut_ptr() as usize;
            let mut offset = 0;
            for row in rows.selected_indices() {
                let scale = match scales {
                    Some(scales) => u32::from(scales[row]),
                    None => u32::try_from(scale).context("a negative decimal scale")?,
                };
                let decimal = Decimal::new(values[row] as i64, scale)
                    .context("a decimal past 18 digits or a scale of 36")?;
                let mut numeric = [0; NUMERIC_MAX];
                let size = decimal.write(&mut numeric);
                let end = offset + size;
                ensure!(end <= space.len(), "a decimal write ran out of space");
                space[offset..end].copy_from_slice(&numeric[..size]);
                values[row] = (base + offset) as u64;
                offset = end.next_multiple_of(8).min(space.len());
            }
            *used = offset;
            Ok(())
        })
    }
}

/// `tess_decimal_read_datum`: a numeric Datum as a decimal, `*found`
/// false when it is none.
///
/// # Safety
///
/// `datum` must point to a whole varlena, unchanged for the call, `value`, `scale`
/// and `found` point to writable values; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_decimal_read_datum(
    datum: u64,
    value: *mut i64,
    scale: *mut c_int,
    found: *mut bool,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let (value, scale, found) = (
                value.as_mut().context("a null value")?,
                scale.as_mut().context("a null scale")?,
                found.as_mut().context("a null flag")?,
            );
            *found = false;
            if let Arg::Decimal(decimal) = read_numeric(datum) {
                *value = decimal.value();
                *scale = decimal.scale() as c_int;
                *found = true;
            }
            Ok(())
        })
    }
}

/// `tess_decimal_write_datum`: the numeric of `value / 10^scale` into
/// `out`, its size in `*size`.
///
/// # Safety
///
/// `out` must point to `len` writable bytes and `size` to a writable size;
/// `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_decimal_write_datum(
    value: i64,
    scale: c_int,
    out: *mut c_void,
    len: usize,
    size: *mut usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let size = size.as_mut().context("a null size")?;
            let decimal = u32::try_from(scale)
                .ok()
                .filter(|&scale| scale <= MAX_SCALE)
                .and_then(|scale| Decimal::new(value, scale))
                .context("a decimal past 18 digits or a scale of 36")?;
            ensure!(
                !out.is_null() && len >= NUMERIC_MAX,
                "a numeric needs {NUMERIC_MAX} bytes"
            );
            let out = &mut *out.cast::<[u8; NUMERIC_MAX]>();
            *size = decimal.write(out);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{DECIMALS_SIZE, DecimalArg};
    use crate::c::varlena::varlena_data;

    #[test]
    fn layout_matches_the_header() {
        assert_eq!(size_of::<DecimalArg>(), 16);
        assert_eq!(size_of::<super::DecimalSum>(), 32);
        assert_eq!(size_of::<super::DecimalExtreme>(), 16);
        assert_eq!(std::mem::offset_of!(super::DecimalExtreme, valid), 12);
        assert_eq!(std::mem::offset_of!(super::DecimalSum, count), 24);
        assert_eq!(DECIMALS_SIZE, 44);
    }

    #[test]
    fn an_int8_partial_is_a_state_of_one_value() {
        use tessera_kernels::decimal::{Partial, Partials, Sum, SumState};

        let values = [42_i64 as u64, -7_i64 as u64, 0];
        let isnull = [false, false, true];
        let column = super::DatumColumn {
            values: values.as_ptr(),
            isnull: isnull.as_ptr(),
            nrows: 3,
            ..super::DatumColumn::EMPTY
        };
        // SAFETY: the arrays hold the column's three rows.
        let partials =
            unsafe { super::PartialColumn::new(super::SumInput::Int8, &column, 3) }.unwrap();
        let state = |value| {
            Partial::State(SumState {
                sum: Sum {
                    value,
                    scale: 0,
                    count: 1,
                },
                ..SumState::default()
            })
        };
        assert_eq!(partials.partial(0).unwrap(), state(42));
        assert_eq!(partials.partial(1).unwrap(), state(-7));
        assert_eq!(partials.partial(2).unwrap(), Partial::Null);
    }

    #[test]
    fn varlena_headers_are_read_or_refused() {
        // A 1-byte header: size 3 with the header.
        let short = [0x07_u8, 0xAA, 0xBB];
        // SAFETY: a whole varlena.
        assert_eq!(
            unsafe { varlena_data(short.as_ptr() as u64) },
            Some(&[0xAA, 0xBB][..])
        );
        // A 4-byte header: size 6 with the header.
        let mut long = [0_u8; 8];
        long[..4].copy_from_slice(&(6_u32 << 2).to_le_bytes());
        long[4..6].copy_from_slice(&[1, 2]);
        // SAFETY: a whole varlena, aligned or not.
        assert_eq!(
            unsafe { varlena_data(long.as_ptr() as u64) },
            Some(&[1, 2][..])
        );
        // An external pointer, a compressed value, a null pointer.
        let external = [0x01_u8, 18];
        let compressed = [0x02_u8, 0, 0, 0];
        // SAFETY: only the first byte of each is read.
        unsafe {
            assert_eq!(varlena_data(external.as_ptr() as u64), None);
            assert_eq!(varlena_data(compressed.as_ptr() as u64), None);
            assert_eq!(varlena_data(0), None);
        }
    }
}

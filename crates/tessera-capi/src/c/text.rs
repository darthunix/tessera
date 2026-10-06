//! The text entry points, declared in `include/tessera/text.h`: strings
//! read in place behind their Datums ([`tessera_kernels::text`]), a
//! compressed or external one left to the caller.
//!
//! # Arguments
//!
//! A `TessTextArg` points to a column or holds a scalar Datum. A column must
//! be a valid `TessDatumColumn` of the call's row count whose `values` and
//! `isnull` hold that many elements, unchanged for the call; a selected
//! row's flag must be initialized and, when not NULL, its value a pointer
//! to a whole varlena. A scalar must point to a whole varlena read in place
//! (not compressed nor external).

use std::ffi::{c_char, c_int};
use std::marker::PhantomData;
use std::slice;

use anyhow::{Context, Result, bail, ensure};
use tessera_kernels::text::{self, Bounds, Chars, Length, Like, Piece, Strings, Text};

use super::column::DatumColumn;
use super::mask::Mask;
use super::source::{self, Constant, output, selection, slots, with_source};
use super::status::{Code, Status, guard};
use super::varlena::varlena_data;

/// `TessTextArg`: a column, or a scalar Datum when it is null.
#[repr(C)]
#[derive(Debug)]
pub struct TextArg {
    /// The column, or null.
    pub column: *const DatumColumn,
    /// The scalar when `column` is null.
    pub scalar: u64,
}

/// A column's strings, read through raw pointers behind one check of the
/// row against the row count.
struct Column<'a> {
    values: *const u64,
    isnull: *const u8,
    nrows: usize,
    borrow: PhantomData<&'a ()>,
}

impl Strings for Column<'_> {
    #[inline(always)]
    fn get(&self, row: usize) -> Text<'_> {
        assert!(row < self.nrows, "a text row past its column");
        // SAFETY: the row is within the arrays, and the calls read selected
        // rows only, whose flags and non-NULL values the constructor's
        // caller guarantees; a flag is read as its byte.
        unsafe {
            if *self.isnull.add(row) != 0 {
                return Text::Null;
            }
            match varlena_data(*self.values.add(row)) {
                Some(bytes) => Text::Bytes(bytes),
                None => Text::Other,
            }
        }
    }
}

impl Strings for Constant<&[u8]> {
    #[inline(always)]
    fn get(&self, _row: usize) -> Text<'_> {
        Text::Bytes(self.0)
    }
}

/// A call's argument: a column or a scalar's bytes, a loop for each.
type Input<'a> = source::Input<Column<'a>, &'a [u8]>;

/// Borrow a column of `nrows` rows.
///
/// # Safety
///
/// As the module documentation says of a column.
#[inline]
unsafe fn column<'a>(column: *const DatumColumn, nrows: usize) -> Result<Column<'a>> {
    // SAFETY: the caller's contract.
    let column = unsafe { column.as_ref() }.context("a null column")?;
    ensure!(
        column.struct_size >= DatumColumn::MIN_SIZE,
        "a Datum column is smaller than its required fields"
    );
    ensure!(
        usize::try_from(column.nrows).ok() == Some(nrows),
        "a text column has another row count than its rows"
    );
    ensure!(
        nrows == 0 || (!column.values.is_null() && !column.isnull.is_null()),
        "a column has null buffers"
    );
    Ok(Column {
        values: column.values,
        isnull: column.isnull.cast(),
        nrows,
        borrow: PhantomData,
    })
}

/// The bytes of a scalar read in place.
///
/// # Safety
///
/// As the module documentation says of a scalar.
#[inline]
unsafe fn scalar<'a>(datum: u64) -> Result<&'a [u8]> {
    // SAFETY: the caller's contract.
    unsafe { varlena_data(datum) }.context("a text scalar not read in place")
}

/// The argument of a call with `nrows` rows.
///
/// # Safety
///
/// As the module documentation says of an argument.
#[inline]
unsafe fn input<'a>(arg: *const TextArg, nrows: usize) -> Result<Input<'a>> {
    // SAFETY: the caller's contract.
    unsafe {
        let arg = arg.as_ref().context("a null text argument")?;
        Ok(if arg.column.is_null() {
            Input::Scalar(scalar(arg.scalar)?)
        } else {
            Input::Column(column(arg.column, nrows)?)
        })
    }
}

/// A byte string of the caller.
///
/// # Safety
///
/// `bytes` must point to `len` bytes, unchanged for `'a`, or be anything
/// when `len` is 0.
#[inline]
unsafe fn bytes<'a>(bytes: *const c_char, len: usize) -> Result<&'a [u8]> {
    if len == 0 {
        return Ok(&[]);
    }
    ensure!(!bytes.is_null(), "a null byte string");
    // SAFETY: the caller's contract.
    Ok(unsafe { slice::from_raw_parts(bytes.cast(), len) })
}

fn chars_of(chars: c_int) -> Result<Chars> {
    Ok(match chars {
        0 => Chars::Bytes,
        1 => Chars::Utf8,
        _ => bail!("an unknown way of counting characters {chars}"),
    })
}

/// `tess_text_compare`: `rows` narrowed to the rows where two strings are
/// equal (`equal`) or not, a bpchar's without its trailing spaces; a row
/// with a string not in place moved to `rest`.
///
/// # Safety
///
/// `left` and `right` must be valid arguments (see the module
/// documentation) of `rows`' row count, `rows` and `rest` point to valid
/// masks that nothing else accesses for the call; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_text_compare(
    equal: bool,
    bpchar: bool,
    left: *const TextArg,
    right: *const TextArg,
    rows: *mut Mask,
    rest: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let mut rows = output(rows)?;
            let nrows = rows.as_view().nrows();
            let (left, right) = (input(left, nrows)?, input(right, nrows)?);
            let mut rest = output(rest)?;
            with_source!(left, |left| with_source!(right, |right| text::compare(
                equal, bpchar, left, right, &mut rows, &mut rest
            )))
        })
    }
}

/// `tess_text_starts_with`: `rows` narrowed to the rows whose string
/// starts with `prefix`, `len` bytes.
///
/// # Safety
///
/// `column` must be a valid column (see the module documentation) of
/// `rows`' row count, `prefix` point to `len` bytes, `rows` and `rest` to
/// valid masks that nothing else accesses for the call; `status` as for
/// every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_text_starts_with(
    column: *const DatumColumn,
    prefix: *const c_char,
    len: usize,
    rows: *mut Mask,
    rest: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let mut rows = output(rows)?;
            let source = self::column(column, rows.as_view().nrows())?;
            let prefix = bytes(prefix, len)?;
            text::starts_with(&source, prefix, &mut rows, &mut output(rest)?)
        })
    }
}

/// `tess_text_like`: `rows` narrowed to the rows whose string matches (or,
/// `negate`, does not match) `pattern`, `len` bytes of literals and `%`;
/// `*simple` false, and nothing done, for a pattern the kernels do not take.
///
/// # Safety
///
/// As for [`tess_text_starts_with`], and `simple` must point to a writable
/// flag.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_text_like(
    column: *const DatumColumn,
    pattern: *const c_char,
    len: usize,
    negate: bool,
    rows: *mut Mask,
    rest: *mut Mask,
    simple: *mut bool,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let simple = simple.as_mut().context("a null flag")?;
            let pattern = bytes(pattern, len)?;
            let Some(like) = Like::parse(pattern) else {
                *simple = false;
                return Ok(());
            };
            let mut rows = output(rows)?;
            let source = self::column(column, rows.as_view().nrows())?;
            text::like(&source, &like, negate, &mut rows, &mut output(rest)?)?;
            *simple = true;
            Ok(())
        })
    }
}

/// `tess_text_lengths`: the lengths (0 characters, 1 a bpchar's characters
/// without trailing spaces, 2 bytes) of the selected rows' strings,
/// characters counted by `chars` (0 bytes, 1 UTF-8).
///
/// # Safety
///
/// `column` must be a valid column (see the module documentation) of
/// `rows`' row count, `values` point to as many writable int32 slots,
/// `non_nulls` and `rest` to valid masks, none accessed by anything else
/// for the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_text_lengths(
    length: c_int,
    chars: c_int,
    column: *const DatumColumn,
    rows: *const Mask,
    values: *mut i32,
    non_nulls: *mut Mask,
    rest: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let length = match length {
                0 => Length::Chars,
                1 => Length::BpcharChars,
                2 => Length::Octets,
                _ => bail!("an unknown string length {length}"),
            };
            let chars = chars_of(chars)?;
            let rows = selection(rows)?;
            let source = self::column(column, rows.nrows())?;
            text::lengths(
                length,
                chars,
                &source,
                rows,
                slots(values, rows.nrows())?,
                &mut output(non_nulls)?,
                &mut output(rest)?,
            )
        })
    }
}

/// `tess_text_pieces`: the bounds of a piece (a `TessTextPiece` with its
/// constants `first` and `second`, `second` for a substring's length when
/// `has_second`) of the selected rows' strings, a start and a length in
/// bytes.
///
/// # Safety
///
/// `column` must be a valid column (see the module documentation) of
/// `rows`' row count, `starts` and `lengths` point to as many writable
/// int32 slots, `non_nulls` and `rest` to valid masks, none accessed by
/// anything else for the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_text_pieces(
    piece: c_int,
    first: i32,
    second: i32,
    has_second: bool,
    chars: c_int,
    column: *const DatumColumn,
    rows: *const Mask,
    starts: *mut i32,
    lengths: *mut i32,
    non_nulls: *mut Mask,
    rest: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let piece = match piece {
                0 => Piece::Substring {
                    start: first,
                    length: has_second.then_some(second),
                },
                1 => Piece::Left(first),
                2 => Piece::Right(first),
                3 => Piece::Rtrim,
                4 => Piece::Ltrim,
                5 => Piece::Btrim,
                _ => bail!("an unknown string piece {piece}"),
            };
            let chars = chars_of(chars)?;
            let rows = selection(rows)?;
            let nrows = rows.nrows();
            let source = self::column(column, nrows)?;
            let mut out = Bounds {
                starts: slots(starts, nrows)?,
                lengths: slots(lengths, nrows)?,
                non_nulls: output(non_nulls)?,
                rest: output(rest)?,
            };
            text::pieces(piece, chars, &source, rows, &mut out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::TextArg;

    #[test]
    fn layout_matches_the_header() {
        assert_eq!(size_of::<TextArg>(), 16);
    }
}

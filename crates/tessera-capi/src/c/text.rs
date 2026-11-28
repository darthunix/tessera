//! The text entry points, declared in `include/tessera/text.h`: strings
//! read in place behind their Datums or given in the bytes form
//! ([`tessera_kernels::text`]), a compressed or external one left to the
//! caller.
//!
//! # Arguments
//!
//! A `TessTextArg` points to a column of `Datum` values, to a column in the
//! bytes form, or holds a scalar Datum when neither is set. A column of
//! `Datum` values must be a valid `TessDatumColumn` of the call's row count
//! whose `values` and `isnull` hold that many elements, unchanged for the
//! call; a selected row's flag must be initialized and, when not NULL, its
//! value a pointer to a whole varlena. A column in the bytes form must be a
//! valid `TessBytesColumn` of the call's row count whose `isnull`, `data`
//! and `lengths` hold that many elements, unchanged for the call; a
//! selected row's flag must be initialized and, when not NULL, its pointer
//! point to as many bytes as its length says, unchanged for the call. A
//! scalar must point to a whole varlena read in place (not compressed nor
//! external).

use std::ffi::{c_char, c_int};
use std::marker::PhantomData;
use std::slice;

use anyhow::{Context, Result, bail, ensure};
use tessera_kernels::text::{self, Bounds, Chars, Length, Like, Piece, Strings, Text};

use super::column::{BytesColumn, DatumColumn};
use super::mask::Mask;
use super::source::{self, Constant, output, selection, slots, with_sources};
use super::status::{Code, Status, guard};
use super::table::output as place;
use super::varlena::varlena_data;

/// `TessTextArg`: a column of `Datum` values, a column in the bytes form,
/// or a scalar Datum when both are null.
#[repr(C)]
#[derive(Debug)]
pub struct TextArg {
    /// The column of `Datum` values, or null.
    pub column: *const DatumColumn,
    /// The scalar when both columns are null.
    pub scalar: u64,
    /// The column in the bytes form, or null; never set with `column`.
    pub bytes: *const BytesColumn,
}

/// A column's strings behind their Datums, read through raw pointers
/// behind one check of the row against the row count.
struct Datums<'a> {
    values: *const u64,
    isnull: *const u8,
    nrows: usize,
    borrow: PhantomData<&'a ()>,
}

impl Strings for Datums<'_> {
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

/// A column's strings in the bytes form, read through raw pointers behind
/// one check of the row against the row count: every string is in place.
struct Bytes<'a> {
    isnull: *const u8,
    data: *const *const u8,
    lengths: *const u32,
    nrows: usize,
    borrow: PhantomData<&'a ()>,
}

impl Strings for Bytes<'_> {
    #[inline(always)]
    fn get(&self, row: usize) -> Text<'_> {
        assert!(row < self.nrows, "a text row past its column");
        // SAFETY: the row is within the arrays, and the calls read selected
        // rows only, whose flags, pointers and lengths the constructor's
        // caller guarantees; a flag is read as its byte.
        unsafe {
            if *self.isnull.add(row) != 0 {
                return Text::Null;
            }
            Text::Bytes(slice::from_raw_parts(
                *self.data.add(row),
                *self.lengths.add(row) as usize,
            ))
        }
    }
}

impl Strings for Constant<&[u8]> {
    #[inline(always)]
    fn get(&self, _row: usize) -> Text<'_> {
        Text::Bytes(self.0)
    }

    #[inline(always)]
    fn constant(&self) -> Option<&[u8]> {
        Some(self.0)
    }
}

/// A column of strings in either form.
enum Column<'a> {
    Datums(Datums<'a>),
    Bytes(Bytes<'a>),
}

/// Run `$body` with `$source` bound to the column of either form: a loop
/// for each.
macro_rules! with_column {
    ($column:expr, |$source:ident| $body:expr) => {
        match $column {
            Column::Datums(column) => {
                let $source = &column;
                $body
            }
            Column::Bytes(column) => {
                let $source = &column;
                $body
            }
        }
    };
}

/// A call's argument: a column of either form or a scalar's bytes, a loop
/// for each.
type Input<'a> = source::Input<Column<'a>, &'a [u8]>;

/// Borrow a column of `Datum` values of `nrows` rows.
///
/// # Safety
///
/// As the module documentation says of a column of `Datum` values.
#[inline]
unsafe fn datums<'a>(column: *const DatumColumn, nrows: usize) -> Result<Datums<'a>> {
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
    Ok(Datums {
        values: column.values,
        isnull: column.isnull.cast(),
        nrows,
        borrow: PhantomData,
    })
}

/// Borrow a column in the bytes form of `nrows` rows.
///
/// # Safety
///
/// As the module documentation says of a column in the bytes form.
#[inline]
unsafe fn bytes_column<'a>(column: *const BytesColumn, nrows: usize) -> Result<Bytes<'a>> {
    // SAFETY: the caller's contract.
    let column = unsafe { column.as_ref() }.context("a null column")?;
    ensure!(
        column.struct_size >= BytesColumn::MIN_SIZE,
        "a bytes column is smaller than its required fields"
    );
    ensure!(
        usize::try_from(column.nrows).ok() == Some(nrows),
        "a text column has another row count than its rows"
    );
    ensure!(
        nrows == 0
            || (!column.isnull.is_null() && !column.data.is_null() && !column.lengths.is_null()),
        "a column has null buffers"
    );
    Ok(Bytes {
        isnull: column.isnull.cast(),
        data: column.data,
        lengths: column.lengths,
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
        let given = arg.as_ref().context("a null text argument")?;
        Ok(if given.column.is_null() && given.bytes.is_null() {
            Input::Scalar(scalar(given.scalar)?)
        } else {
            Input::Column(column(arg, nrows)?)
        })
    }
}

/// The column of an argument that must be one, of either form.
///
/// # Safety
///
/// As the module documentation says of an argument.
#[inline]
unsafe fn column<'a>(arg: *const TextArg, nrows: usize) -> Result<Column<'a>> {
    // SAFETY: the caller's contract.
    unsafe {
        let arg = arg.as_ref().context("a null text argument")?;
        match (arg.column.is_null(), arg.bytes.is_null()) {
            (false, true) => Ok(Column::Datums(datums(arg.column, nrows)?)),
            (true, false) => Ok(Column::Bytes(bytes_column(arg.bytes, nrows)?)),
            (true, true) => bail!("a text function takes a column"),
            (false, false) => bail!("a text argument of two columns"),
        }
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
            with_sources!(left, right, with_column!, |left, right| text::compare(
                equal, bpchar, left, right, &mut rows, &mut rest
            ))
        })
    }
}

/// `tess_text_starts_with`: `rows` narrowed to the rows whose string
/// starts with `prefix`, `len` bytes.
///
/// # Safety
///
/// `column` must be a valid argument (see the module documentation) of
/// `rows`' row count, a column of either form, `prefix` point to `len` bytes, `rows` and `rest` to
/// valid masks that nothing else accesses for the call; `status` as for
/// every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_text_starts_with(
    column: *const TextArg,
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
            let column = self::column(column, rows.as_view().nrows())?;
            let prefix = bytes(prefix, len)?;
            let mut rest = output(rest)?;
            with_column!(column, |source| text::starts_with(
                source, prefix, &mut rows, &mut rest
            ))
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
    column: *const TextArg,
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
            let simple = place(simple, "flag")?;
            let pattern = bytes(pattern, len)?;
            let Some(like) = Like::parse(pattern) else {
                simple.write(false);
                return Ok(());
            };
            let mut rows = output(rows)?;
            let column = self::column(column, rows.as_view().nrows())?;
            let mut rest = output(rest)?;
            with_column!(column, |source| text::like(
                source, &like, negate, &mut rows, &mut rest
            ))?;
            simple.write(true);
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
/// `column` must be a valid argument (see the module documentation) of
/// `rows`' row count, a column of either form, `values` point to as many writable int32 slots,
/// `non_nulls` and `rest` to valid masks, none accessed by anything else
/// for the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_text_lengths(
    length: c_int,
    chars: c_int,
    column: *const TextArg,
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
            let column = self::column(column, rows.nrows())?;
            let values = slots(values, rows.nrows())?;
            let (mut non_nulls, mut rest) = (output(non_nulls)?, output(rest)?);
            with_column!(column, |source| text::lengths(
                length,
                chars,
                source,
                rows,
                values,
                &mut non_nulls,
                &mut rest,
            ))
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
/// `column` must be a valid argument (see the module documentation) of
/// `rows`' row count, a column of either form, `starts` and `lengths` point to as many writable
/// int32 slots, `non_nulls` and `rest` to valid masks, none accessed by
/// anything else for the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_text_pieces(
    piece: c_int,
    first: i32,
    second: i32,
    has_second: bool,
    chars: c_int,
    column: *const TextArg,
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
            let column = self::column(column, nrows)?;
            let mut out = Bounds {
                starts: slots(starts, nrows)?,
                lengths: slots(lengths, nrows)?,
                non_nulls: output(non_nulls)?,
                rest: output(rest)?,
            };
            with_column!(column, |source| text::pieces(
                piece, chars, source, rows, &mut out
            ))
        })
    }
}

/// `tess_text_sort_words`: the sort words of the selected rows' strings,
/// their first eight bytes big-endian, zero-padded, the top bit flipped
/// ([`text::sort_word`]); `non_nulls` gets the rows without NULL, `rest`
/// those of them whose string is not in place, whose words are not
/// written.
///
/// # Safety
///
/// `column` must be a valid argument (see the module documentation) of
/// `rows`' row count, a column of either form, `words` point to as many
/// writable int64 slots, `non_nulls` and `rest` to valid masks, none
/// accessed by anything else for the call; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_text_sort_words(
    column: *const TextArg,
    rows: *const Mask,
    words: *mut i64,
    non_nulls: *mut Mask,
    rest: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let rows = selection(rows)?;
            let column = self::column(column, rows.nrows())?;
            let words = slots(words, rows.nrows())?;
            let (mut non_nulls, mut rest) = (output(non_nulls)?, output(rest)?);
            with_column!(column, |source| text::sort_words(
                source,
                rows,
                words,
                &mut non_nulls,
                &mut rest
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::TextArg;

    #[test]
    fn layout_matches_the_header() {
        assert_eq!(size_of::<TextArg>(), 24);
        assert_eq!(std::mem::offset_of!(TextArg, scalar), 8);
        assert_eq!(std::mem::offset_of!(TextArg, bytes), 16);
    }
}

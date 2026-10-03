//! Strings as PostgreSQL's text functions read them, over their bytes:
//! equality of text and of bpchar (trailing spaces not counted),
//! `starts_with`, LIKE of a pattern of literals and `%`, the lengths in
//! characters and in bytes, and the bounds of a piece (`substring`, `left`,
//! `right`, the trims of spaces), which the caller copies.
//!
//! Characters are counted as the database encoding counts them: a byte each
//! in a single-byte encoding, or UTF-8 by its lead bytes ([`Chars`]); other
//! multibyte encodings stay with the core's functions. A LIKE pattern is
//! matched by its pieces between `%`s: the first against the start, the
//! last against the end, the others found in order between; a pattern with
//! `_` or an escape, or with more pieces than [`MAX_PIECES`], is not taken
//! ([`Like::parse`]), for the caller's core function.
//!
//! The batch functions run over the selected rows of a [`Strings`] source:
//! a row whose bytes the source cannot give in place (a compressed or
//! external value) goes to `rest`, for the caller to unpack and hand back
//! as a batch of its own.

use std::fmt;
use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{RowMask, RowMaskView, ones};

/// How the database encoding counts characters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Chars {
    /// A byte each.
    Bytes,
    /// UTF-8: a character is its lead byte and continuation bytes.
    Utf8,
}

/// An error the core raises for a string, SQLSTATE 22011.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextError {
    /// A substring of negative length.
    NegativeSubstringLength,
}

impl TextError {
    /// The five-character SQLSTATE of the error.
    pub fn sqlstate(self) -> &'static str {
        "22011"
    }
}

impl fmt::Display for TextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("negative substring length not allowed")
    }
}

impl std::error::Error for TextError {}

/// The characters of the bytes: in UTF-8 the bytes that do not continue one.
#[inline]
pub fn char_count(bytes: &[u8], chars: Chars) -> i64 {
    match chars {
        Chars::Bytes => bytes.len() as i64,
        Chars::Utf8 => bytes.iter().filter(|&&byte| byte & 0xC0 != 0x80).count() as i64,
    }
}

/// A UTF-8 character's length by its lead byte, as `pg_utf_mblen`.
#[inline(always)]
const fn utf8_length(lead: u8) -> usize {
    if lead & 0x80 == 0 {
        1
    } else if lead & 0xE0 == 0xC0 {
        2
    } else if lead & 0xF0 == 0xE0 {
        3
    } else if lead & 0xF8 == 0xF0 {
        4
    } else {
        1
    }
}

/// The byte offset past the first `n` characters of the bytes, at most
/// their length; 0 for `n` of 0 or less.
#[inline]
pub fn char_offset(bytes: &[u8], n: i64, chars: Chars) -> usize {
    match chars {
        Chars::Bytes => n.clamp(0, bytes.len() as i64) as usize,
        Chars::Utf8 => {
            let mut offset = 0;
            let mut left = n;
            while left > 0 && offset < bytes.len() {
                offset += utf8_length(bytes[offset]);
                left -= 1;
            }
            offset.min(bytes.len())
        }
    }
}

/// A bpchar's bytes without its trailing spaces, as `bcTruelen` counts.
#[inline]
pub fn bpchar_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .rposition(|&byte| byte != b' ')
        .map_or(0, |at| at + 1)
}

/// The first place of `needle` in `haystack`.
#[inline]
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let Some((&first, rest)) = needle.split_first() else {
        return Some(0);
    };
    let last = haystack.len().checked_sub(needle.len())?;
    let mut from = 0;
    while from <= last {
        let at = from + memchr::memchr(first, &haystack[from..=last])?;
        if &haystack[at + 1..at + needle.len()] == rest {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// The most pieces between `%`s a LIKE pattern [`Like::parse`] takes.
pub const MAX_PIECES: usize = 32;

/// A LIKE pattern of literals and `%`: its pieces, and whether the first
/// is anchored at the start and the last at the end.
#[derive(Clone, Copy, Debug)]
pub struct Like<'p> {
    pattern: &'p [u8],
    /// No `%`: the string equals the pattern.
    exact: bool,
    start: bool,
    end: bool,
    count: usize,
    pieces: [(u32, u32); MAX_PIECES],
}

impl<'p> Like<'p> {
    /// The pieces of a pattern, or `None` for one with `_` or an escape, or
    /// with more than [`MAX_PIECES`] pieces.
    pub fn parse(pattern: &'p [u8]) -> Option<Self> {
        let mut like = Self {
            pattern,
            exact: true,
            start: pattern.first() != Some(&b'%'),
            end: pattern.last() != Some(&b'%'),
            count: 0,
            pieces: [(0, 0); MAX_PIECES],
        };
        let mut begin = 0;
        for at in 0..=pattern.len() {
            match pattern.get(at) {
                Some(b'_' | b'\\') => return None,
                Some(b'%') | None => {
                    if like.count == MAX_PIECES {
                        return None;
                    }
                    like.pieces[like.count] = (begin as u32, (at - begin) as u32);
                    like.count += 1;
                    begin = at + 1;
                    if at < pattern.len() {
                        like.exact = false;
                    }
                }
                Some(_) => {}
            }
        }
        Some(like)
    }

    #[inline(always)]
    fn piece(&self, index: usize) -> &'p [u8] {
        let (from, len) = self.pieces[index];
        &self.pattern[from as usize..(from + len) as usize]
    }

    /// Whether a string matches the pattern.
    #[inline]
    pub fn matches(&self, string: &[u8]) -> bool {
        if self.exact {
            return string == self.pattern;
        }
        let mut first = 0;
        let mut last = self.count - 1;
        let mut from = 0;
        let mut to = string.len();
        if self.start {
            let head = self.piece(0);
            if !string.starts_with(head) {
                return false;
            }
            from = head.len();
            first = 1;
        }
        if self.end {
            let tail = self.piece(last);
            if to - from < tail.len() || !string.ends_with(tail) {
                return false;
            }
            to -= tail.len();
            // Not exact: at least two pieces, so one stays before the tail.
            last -= 1;
        }
        for index in first..=last {
            let piece = self.piece(index);
            if piece.is_empty() {
                continue;
            }
            match find(&string[from..to], piece) {
                Some(at) => from += at + piece.len(),
                None => return false,
            }
        }
        true
    }
}

/// A row's string as the batch functions read it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Text<'a> {
    /// SQL NULL.
    Null,
    /// The string's bytes.
    Bytes(&'a [u8]),
    /// A value whose bytes the source cannot give in place.
    Other,
}

/// The strings of a batch function: a column or a scalar.
pub trait Strings {
    /// The string of a row. The batch functions ask only for selected rows.
    fn get(&self, row: usize) -> Text<'_>;
}

#[inline]
fn check_rows(nrows: usize, masks: &[usize]) -> Result<()> {
    ensure!(
        masks.iter().all(|&rows| rows == nrows),
        "the masks of a text call have different row counts"
    );
    Ok(())
}

/// Narrow `rows` to the rows where `test` holds of the rows' strings: a row
/// with a NULL leaves, a row with a string not in place leaves too and is
/// set in `rest`, whose other bits are cleared.
#[inline(always)]
fn filter_rows(
    rows: &mut RowMask<'_>,
    rest: &mut RowMask<'_>,
    mut test: impl FnMut(usize) -> Option<Option<bool>>,
) -> Result<()> {
    let nrows = rows.as_view().nrows();
    check_rows(nrows, &[rest.as_view().nrows()])?;
    for word in 0..nrows.div_ceil(64) {
        let look = rows.as_view().word_at(word);
        let (mut keep, mut other) = (0, 0);
        for bit in ones(look) {
            match test(word * 64 + bit) {
                None => {}
                Some(None) => other |= 1 << bit,
                Some(Some(true)) => keep |= 1 << bit,
                Some(Some(false)) => {}
            }
        }
        rows.set_word(word, keep)?;
        rest.set_word(word, other)?;
    }
    Ok(())
}

/// Equality (`equal`) or inequality of two strings, a bpchar's without its
/// trailing spaces.
///
/// # Errors
///
/// Masks of different row counts.
pub fn compare(
    equal: bool,
    bpchar: bool,
    left: &impl Strings,
    right: &impl Strings,
    rows: &mut RowMask<'_>,
    rest: &mut RowMask<'_>,
) -> Result<()> {
    filter_rows(rows, rest, |row| match (left.get(row), right.get(row)) {
        (Text::Null, _) | (_, Text::Null) => None,
        (Text::Bytes(a), Text::Bytes(b)) => Some(Some(
            if bpchar {
                a[..bpchar_len(a)] == b[..bpchar_len(b)]
            } else {
                a == b
            } == equal,
        )),
        _ => Some(None),
    })
}

/// `starts_with` of a string column and a prefix.
///
/// # Errors
///
/// Masks of different row counts.
pub fn starts_with(
    source: &impl Strings,
    prefix: &[u8],
    rows: &mut RowMask<'_>,
    rest: &mut RowMask<'_>,
) -> Result<()> {
    filter_rows(rows, rest, |row| match source.get(row) {
        Text::Null => None,
        Text::Bytes(string) => Some(Some(string.starts_with(prefix))),
        Text::Other => Some(None),
    })
}

/// LIKE (or, `negate`, NOT LIKE) of a string column and a parsed pattern.
///
/// # Errors
///
/// Masks of different row counts.
pub fn like(
    source: &impl Strings,
    pattern: &Like<'_>,
    negate: bool,
    rows: &mut RowMask<'_>,
    rest: &mut RowMask<'_>,
) -> Result<()> {
    filter_rows(rows, rest, |row| match source.get(row) {
        Text::Null => None,
        Text::Bytes(string) => Some(Some(pattern.matches(string) != negate)),
        Text::Other => Some(None),
    })
}

/// A length of [`lengths`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Length {
    /// The characters of a text.
    Chars,
    /// The characters of a bpchar without its trailing spaces.
    BpcharChars,
    /// The bytes.
    Octets,
}

/// The lengths of the selected rows' strings into `values`: `non_nulls`
/// gets the rows without NULL, `rest` those of them whose string is not in
/// place.
///
/// # Errors
///
/// Masks and arrays of different row counts.
pub fn lengths(
    length: Length,
    chars: Chars,
    source: &impl Strings,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i32>],
    non_nulls: &mut RowMask<'_>,
    rest: &mut RowMask<'_>,
) -> Result<()> {
    check_rows(
        rows.nrows(),
        &[
            values.len(),
            non_nulls.as_view().nrows(),
            rest.as_view().nrows(),
        ],
    )?;
    for word in 0..rows.nrows().div_ceil(64) {
        let look = rows.word_at(word);
        let (mut present, mut other) = (0, 0);
        for bit in ones(look) {
            let row = word * 64 + bit;
            match source.get(row) {
                Text::Null => continue,
                Text::Bytes(string) => {
                    values[row].write(match length {
                        Length::Chars => char_count(string, chars) as i32,
                        Length::BpcharChars => {
                            char_count(&string[..bpchar_len(string)], chars) as i32
                        }
                        Length::Octets => string.len() as i32,
                    });
                }
                Text::Other => other |= 1 << bit,
            }
            present |= 1 << bit;
        }
        non_nulls.set_word(word, present)?;
        rest.set_word(word, other)?;
    }
    Ok(())
}

/// A piece of a string, by constants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Piece {
    /// `substring(s from start for length)`, to the end without a length.
    Substring {
        /// The first character, from 1.
        start: i32,
        /// The characters, `None` to the end.
        length: Option<i32>,
    },
    /// `left(s, n)`: the first `n`, or all but the last `-n`.
    Left(i32),
    /// `right(s, n)`: the last `n`, or all but the first `-n`.
    Right(i32),
    /// `rtrim` of spaces.
    Rtrim,
    /// `ltrim` of spaces.
    Ltrim,
    /// `btrim` of spaces.
    Btrim,
}

impl Piece {
    /// The byte bounds of the piece of a string, as the core's
    /// `text_substring`, `text_left`, `text_right` and trims take them:
    /// substring from `start` for `length` begins at `max(start, 1)` and
    /// ends before `start + length`, an overflow running to the end; left of
    /// `n < 0` drops the last `-n` characters and right of `n < 0` the first
    /// `-n`.
    ///
    /// # Errors
    ///
    /// A substring of negative length.
    #[inline]
    pub fn bounds(self, string: &[u8], chars: Chars) -> Result<(usize, usize), TextError> {
        let (skip, take) = match self {
            Self::Rtrim => return Ok((0, bpchar_len(string))),
            Self::Ltrim | Self::Btrim => {
                let to = if self == Self::Btrim {
                    bpchar_len(string)
                } else {
                    string.len()
                };
                let from = string[..to]
                    .iter()
                    .position(|&byte| byte != b' ')
                    .unwrap_or(to);
                return Ok((from, to));
            }
            Self::Substring { start, length } => {
                let skip = i64::from(start.max(1)) - 1;
                let take = match length {
                    None => i64::MAX,
                    Some(length) if length < 0 => {
                        return Err(TextError::NegativeSubstringLength);
                    }
                    Some(length) => match start.checked_add(length) {
                        Some(end) if end < 1 => 0,
                        Some(end) => i64::from(end) - i64::from(start.max(1)),
                        None => i64::MAX,
                    },
                };
                (skip, take)
            }
            Self::Left(n) if n >= 0 => (0, i64::from(n)),
            Self::Left(n) => (0, char_count(string, chars) + i64::from(n)),
            // -n in 64 bits: right(-2147483648) skips it all, as the core clamps.
            Self::Right(n) if n < 0 => (-i64::from(n), i64::MAX),
            Self::Right(n) => (char_count(string, chars) - i64::from(n), i64::MAX),
        };
        let from = char_offset(string, skip, chars);
        let to = if take <= 0 {
            from
        } else {
            from + char_offset(&string[from..], take, chars)
        };
        Ok((from, to))
    }
}

/// Where [`pieces`] writes a batch's bounds.
pub struct Bounds<'a> {
    /// A piece's first byte in its string, by row.
    pub starts: &'a mut [MaybeUninit<i32>],
    /// A piece's bytes, by row.
    pub lengths: &'a mut [MaybeUninit<i32>],
    /// The selected rows without NULL.
    pub non_nulls: RowMask<'a>,
    /// The rows of `non_nulls` whose string is not in place.
    pub rest: RowMask<'a>,
}

/// The bounds of a piece of the selected rows' strings.
///
/// # Errors
///
/// A substring of negative length, at the first row without NULL, or masks
/// and arrays of different row counts.
pub fn pieces(
    piece: Piece,
    chars: Chars,
    source: &impl Strings,
    rows: RowMaskView<'_>,
    out: &mut Bounds<'_>,
) -> Result<()> {
    check_rows(
        rows.nrows(),
        &[
            out.starts.len(),
            out.lengths.len(),
            out.non_nulls.as_view().nrows(),
            out.rest.as_view().nrows(),
        ],
    )?;
    for word in 0..rows.nrows().div_ceil(64) {
        let look = rows.word_at(word);
        let (mut present, mut other) = (0, 0);
        for bit in ones(look) {
            let row = word * 64 + bit;
            match source.get(row) {
                Text::Null => continue,
                Text::Bytes(string) => {
                    let (from, to) = piece.bounds(string, chars)?;
                    out.starts[row].write(from as i32);
                    out.lengths[row].write((to - from) as i32);
                }
                Text::Other => {
                    // The error of a negative length comes at the first row.
                    if let Piece::Substring {
                        length: Some(length),
                        ..
                    } = piece
                        && length < 0
                    {
                        return Err(TextError::NegativeSubstringLength.into());
                    }
                    other |= 1 << bit;
                }
            }
            present |= 1 << bit;
        }
        out.non_nulls.set_word(word, present)?;
        out.rest.set_word(word, other)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_count_and_offset_by_lead_bytes() {
        let s = "aé€𝄞b".as_bytes(); // 1 + 2 + 3 + 4 + 1 bytes
        assert_eq!(char_count(s, Chars::Utf8), 5);
        assert_eq!(char_count(s, Chars::Bytes), 11);
        assert_eq!(char_offset(s, 2, Chars::Utf8), 3);
        assert_eq!(char_offset(s, 4, Chars::Utf8), 10);
        assert_eq!(char_offset(s, 99, Chars::Utf8), 11);
        assert_eq!(char_offset(s, -3, Chars::Utf8), 0);
        assert_eq!(char_offset(s, 3, Chars::Bytes), 3);
        // A string cut inside a character stops at its end.
        assert_eq!(char_offset(&s[..2], 2, Chars::Utf8), 2);
        assert_eq!(bpchar_len(b"ab  "), 2);
        assert_eq!(bpchar_len(b"   "), 0);
    }

    #[test]
    fn like_matches_pieces_as_the_core() {
        let cases: [(&[u8], &[u8], bool); 18] = [
            (b"abc", b"abc", true),
            (b"abc", b"abd", false),
            (b"a%", b"abc", true),
            (b"a%", b"", false),
            (b"%c", b"abc", true),
            (b"%b%", b"abc", true),
            (b"%x%", b"abc", false),
            (b"a%c", b"ac", true),
            (b"a%c", b"abcbc", true),
            (b"ab%bc", b"abc", false),
            (b"a%b%c", b"axbyc", true),
            (b"a%b%c", b"axcyb", false),
            (b"%", b"", true),
            (b"%%", b"x", true),
            (b"", b"", true),
            (b"", b"x", false),
            (b"%aa%aa%", b"aaa", false),
            (b"%aa%aa%", b"aaaa", true),
        ];
        for (pattern, string, expected) in cases {
            let like = Like::parse(pattern).unwrap();
            assert_eq!(
                like.matches(string),
                expected,
                "{} {}",
                String::from_utf8_lossy(pattern),
                String::from_utf8_lossy(string)
            );
        }
        assert!(Like::parse(b"a_c").is_none());
        assert!(Like::parse(b"a\\%").is_none());
        // n %s make n + 1 pieces.
        assert!(Like::parse(&[b'%'; MAX_PIECES - 1]).is_some());
        assert!(Like::parse(&[b'%'; MAX_PIECES]).is_none());
    }

    #[test]
    fn pieces_as_the_core_takes_them() {
        let s = "héllo world".as_bytes();
        let text = |piece: Piece| {
            piece
                .bounds(s, Chars::Utf8)
                .map(|(from, to)| String::from_utf8(s[from..to].to_vec()).unwrap())
        };
        let substring = |start, length| Piece::Substring { start, length };
        assert_eq!(text(substring(2, Some(3))).unwrap(), "éll");
        assert_eq!(text(substring(-1, Some(3))).unwrap(), "h");
        assert_eq!(text(substring(-5, Some(3))).unwrap(), "");
        assert_eq!(text(substring(7, None)).unwrap(), "world");
        assert_eq!(text(substring(3, Some(i32::MAX))).unwrap(), "llo world");
        assert_eq!(text(substring(99, Some(2))).unwrap(), "");
        assert_eq!(
            text(substring(1, Some(-1))),
            Err(TextError::NegativeSubstringLength)
        );
        assert_eq!(text(Piece::Left(2)).unwrap(), "hé");
        assert_eq!(text(Piece::Left(-6)).unwrap(), "héllo");
        assert_eq!(text(Piece::Left(-99)).unwrap(), "");
        assert_eq!(text(Piece::Right(5)).unwrap(), "world");
        assert_eq!(text(Piece::Right(-6)).unwrap(), "world");
        assert_eq!(text(Piece::Right(i32::MIN)).unwrap(), "");
        let padded = b"  ab c  ";
        let trim = |piece: Piece| {
            let (from, to) = piece.bounds(padded, Chars::Bytes).unwrap();
            &padded[from..to]
        };
        assert_eq!(trim(Piece::Rtrim), b"  ab c");
        assert_eq!(trim(Piece::Ltrim), b"ab c  ");
        assert_eq!(trim(Piece::Btrim), b"ab c");
        assert_eq!(Piece::Btrim.bounds(b"   ", Chars::Bytes), Ok((0, 0)));
    }
}

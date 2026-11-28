//! The text entry points called with raw pointers, as C calls them:
//! strings of both varlena headers, a compressed one left in the rest, the
//! error of a negative substring length; and every entry point over both
//! forms of the same strings, a column of `Datum` values and a column in
//! the bytes form, compared by a property test and, under Miri, which is
//! too slow for it, by fixed cases of every entry point.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use std::ptr::{self, NonNull};

use proptest::prelude::*;
use tessera_capi::c::{
    BytesColumn, Code, DatumColumn, Mask, Status, TextArg, tess_text_compare, tess_text_lengths,
    tess_text_like, tess_text_pieces, tess_text_sort_words, tess_text_starts_with,
};
use tessera_testing::{property, words};

mod support;
use support::unset;

/// Varlenas kept at stable addresses, each in words so that a 4-byte
/// header is aligned. They are kept as raw pointers, freed on drop: a
/// `Box` moved after its address was taken would, under Miri's rules of
/// borrows, make that address invalid.
struct Strings(Vec<*mut [u64]>);

impl Strings {
    fn add(&mut self, bytes: &[u8]) -> u64 {
        let mut words = vec![0_u64; bytes.len().div_ceil(8)].into_boxed_slice();
        // SAFETY: the words hold at least as many bytes as the string.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), words.as_mut_ptr().cast(), bytes.len()) };
        let words = Box::into_raw(words);
        self.0.push(words);
        words.cast::<u64>().expose_provenance() as u64
    }

    /// A string with a 1-byte header, as a table stores a short one.
    fn short(&mut self, text: &str) -> u64 {
        self.short_bytes(text.as_bytes())
    }

    fn short_bytes(&mut self, text: &[u8]) -> u64 {
        assert!(text.len() < 127, "a 1-byte header holds up to 126 bytes");
        let mut bytes = vec![(((text.len() + 1) << 1) | 1) as u8];
        bytes.extend_from_slice(text);
        self.add(&bytes)
    }

    /// A string with a 4-byte header.
    fn long(&mut self, text: &str) -> u64 {
        self.long_bytes(text.as_bytes())
    }

    fn long_bytes(&mut self, text: &[u8]) -> u64 {
        let mut bytes = (((text.len() + 4) as u32) << 2).to_le_bytes().to_vec();
        bytes.extend_from_slice(text);
        self.add(&bytes)
    }

    /// A header of a compressed value: never read in place.
    fn compressed(&mut self) -> u64 {
        self.add(&(((12_u32) << 2) | 0b10).to_le_bytes())
    }
}

impl Drop for Strings {
    fn drop(&mut self) {
        for words in self.0.drain(..) {
            // SAFETY: each pointer came from `Box::into_raw` and is freed once.
            drop(unsafe { Box::from_raw(words) });
        }
    }
}

fn column(values: &[u64], isnull: &[bool]) -> DatumColumn {
    DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: values.len() as i32,
        ..DatumColumn::EMPTY
    }
}

/// A column as an argument.
fn arg(column: &DatumColumn) -> TextArg {
    TextArg {
        column,
        scalar: 0,
        bytes: ptr::null(),
    }
}

fn mask(nrows: usize, words: &mut [u64]) -> Mask {
    Mask {
        nrows: nrows as i32,
        bits: words.as_mut_ptr(),
    }
}

/// A bpchar column against a scalar with a trailing space, on either side,
/// equal and not equal: values that differ in their trailing spaces
/// alone are equal, a NULL leaves the rows, a compressed value goes to the
/// rest, as the generic comparison of both strings row by row.
#[test]
fn a_bpchar_scalar_on_either_side() {
    let mut strings = Strings(Vec::new());
    let values = vec![
        strings.short("ab"),
        strings.short("ab  "),
        strings.short("a"),
        strings.compressed(),
        0,
        strings.short("ab c"),
    ];
    let isnull = [false, false, false, false, true, false];
    let source = column(&values, &isnull);
    let scalar = TextArg {
        column: ptr::null(),
        scalar: strings.short("ab "),
        bytes: ptr::null(),
    };
    let values_arg = arg(&source);
    for (equal, kept) in [(true, 0b00_0011_u64), (false, 0b10_0100)] {
        for scalar_left in [false, true] {
            let (left, right) = if scalar_left {
                (&scalar, &values_arg)
            } else {
                (&values_arg, &scalar)
            };
            let (mut rows, mut rest) = ([0b11_1111_u64], [0_u64]);
            let mut status = Status::new();
            // SAFETY: local columns, masks and status.
            let code = unsafe {
                tess_text_compare(
                    equal,
                    true,
                    left,
                    right,
                    &mut mask(6, &mut rows),
                    &mut mask(6, &mut rest),
                    &raw mut status,
                )
            };
            assert_eq!(code, Code::Ok, "{}", status.message());
            assert_eq!(
                (rows[0], rest[0]),
                (kept, 0b00_1000),
                "equal {equal}, scalar on the left {scalar_left}"
            );
        }
    }
}

#[test]
fn comparisons_patterns_and_the_rest() {
    let mut strings = Strings(Vec::new());
    let values = vec![
        strings.short("abc"),
        strings.long("abc"),
        strings.short("abd"),
        strings.compressed(),
        0,
    ];
    let isnull = [false, false, false, false, true];
    let source = column(&values, &isnull);
    let scalar = TextArg {
        column: ptr::null(),
        scalar: strings.long("abc"),
        bytes: ptr::null(),
    };
    let left = arg(&source);
    let (mut rows, mut rest) = ([0b1_1111_u64], [0_u64]);
    let mut status = Status::new();
    // SAFETY: local columns, masks and status.
    let code = unsafe {
        tess_text_compare(
            true,
            false,
            &raw const left,
            &raw const scalar,
            &mut mask(5, &mut rows),
            &mut mask(5, &mut rest),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!((rows[0], rest[0]), (0b0011, 0b1000));
    // LIKE of pieces, a pattern it does not take; the flag comes unset, as
    // C's variable does.
    let (mut rows, mut rest) = ([0b1_1111_u64], [0_u64]);
    let pattern = b"a%c";
    // SAFETY: as above; the pattern is a local byte string.
    let simple = unsafe {
        unset(|simple| {
            tess_text_like(
                &raw const left,
                pattern.as_ptr().cast(),
                pattern.len(),
                false,
                &mut mask(5, &mut rows),
                &mut mask(5, &mut rest),
                simple,
                &raw mut status,
            )
        })
    };
    assert!(simple);
    assert_eq!((rows[0], rest[0]), (0b0011, 0b1000));
    let underscore = b"a_c";
    let mut untouched = [0b1_1111_u64];
    // SAFETY: as above.
    let simple = unsafe {
        unset(|simple| {
            tess_text_like(
                &raw const left,
                underscore.as_ptr().cast(),
                underscore.len(),
                false,
                &mut mask(5, &mut untouched),
                &mut mask(5, &mut rest),
                simple,
                &raw mut status,
            )
        })
    };
    assert!(!simple);
    assert_eq!(untouched[0], 0b1_1111);
    let prefix = b"ab";
    let (mut rows, mut rest) = ([0b1_1111_u64], [0_u64]);
    // SAFETY: as above.
    let code = unsafe {
        tess_text_starts_with(
            &raw const left,
            prefix.as_ptr().cast(),
            prefix.len(),
            &mut mask(5, &mut rows),
            &mut mask(5, &mut rest),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!((rows[0], rest[0]), (0b0111, 0b1000));
}

#[test]
fn lengths_and_pieces_by_characters() {
    let mut strings = Strings(Vec::new());
    let values = vec![strings.short("héllo  "), strings.compressed()];
    let isnull = [false, false];
    let source = column(&values, &isnull);
    let source = arg(&source);
    let rows = [0b11_u64];
    let mut lengths = [0_i32; 2];
    let (mut present, mut rest) = ([0_u64], [0_u64]);
    let mut status = Status::new();
    // SAFETY: local columns, masks, buffers and status.
    let code = unsafe {
        tess_text_lengths(
            1,
            1,
            &raw const source,
            &mask(2, &mut rows.clone()),
            lengths.as_mut_ptr(),
            &mut mask(2, &mut present),
            &mut mask(2, &mut rest),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!((lengths[0], present[0], rest[0]), (5, 0b11, 0b10));
    // A compressed value's sort word is left to the caller, in the rest.
    let worded = sort_words(&source, &Rows::of(&[true, true]));
    assert_eq!(worded.code, Code::Ok, "{}", worded.message);
    assert_eq!((worded.masks[0][0], worded.masks[1][0]), (0b11, 0b10));
    assert_eq!(worded.words[1], UNTOUCHED_WORD);
    let (mut starts, mut sizes) = ([0_i32; 2], [0_i32; 2]);
    // SAFETY: as above.
    let code = unsafe {
        tess_text_pieces(
            0,
            2,
            3,
            true,
            1,
            &raw const source,
            &mask(2, &mut rows.clone()),
            starts.as_mut_ptr(),
            sizes.as_mut_ptr(),
            &mut mask(2, &mut present),
            &mut mask(2, &mut rest),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    // "éll": from byte 1, 4 bytes.
    assert_eq!((starts[0], sizes[0], rest[0]), (1, 4, 0b10));
    // A negative length fails at the first row without NULL.
    // SAFETY: as above.
    let code = unsafe {
        tess_text_pieces(
            0,
            1,
            -1,
            true,
            1,
            &raw const source,
            &mask(2, &mut rows.clone()),
            starts.as_mut_ptr(),
            sizes.as_mut_ptr(),
            &mut mask(2, &mut present),
            &mut mask(2, &mut rest),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::DataException);
    assert_eq!(
        (status.sqlstate(), status.message()),
        (
            "22011".to_owned(),
            "negative substring length not allowed".to_owned()
        )
    );
}

/// One string column in both forms: varlenas behind `Datum`s, and the
/// bytes form with each value in an allocation of its own, so that Miri
/// sees a read past a value's end. A row not selected holds, in the bytes
/// form, a null pointer and the largest length, and a `NULL` row a null
/// pointer, which no kernel may follow; an empty value a dangling one, as
/// the form allows any pointer but null for it. The values are kept as raw
/// pointers, freed on drop, as [`Strings`] keeps its own.
struct Forms {
    _varlenas: Strings,
    values: Vec<u64>,
    isnull: Vec<bool>,
    buffers: Vec<*mut [u8]>,
    data: Vec<*const u8>,
    lengths: Vec<u32>,
    bytes_isnull: Vec<bool>,
}

impl Forms {
    fn new(strings: &[Option<Vec<u8>>], long: &[bool], selected: &[bool]) -> Self {
        let mut varlenas = Strings(Vec::new());
        let mut forms = Forms {
            _varlenas: Strings(Vec::new()),
            values: Vec::new(),
            isnull: Vec::new(),
            buffers: Vec::new(),
            data: Vec::new(),
            lengths: Vec::new(),
            bytes_isnull: Vec::new(),
        };
        for (row, string) in strings.iter().enumerate() {
            let asked = selected[row];
            forms.isnull.push(string.is_none());
            forms.bytes_isnull.push(asked && string.is_none());
            let Some(string) = string else {
                forms.values.push(0);
                forms.data.push(ptr::null());
                forms.lengths.push(if asked { 0 } else { u32::MAX });
                continue;
            };
            forms.values.push(if long[row] || string.len() >= 127 {
                varlenas.long_bytes(string)
            } else {
                varlenas.short_bytes(string)
            });
            if !asked {
                forms.data.push(ptr::null());
                forms.lengths.push(u32::MAX);
            } else if string.is_empty() {
                forms.data.push(NonNull::<u8>::dangling().as_ptr());
                forms.lengths.push(0);
            } else {
                let buffer = Box::into_raw(string.clone().into_boxed_slice());
                forms.data.push(buffer.cast::<u8>());
                forms.lengths.push(string.len() as u32);
                forms.buffers.push(buffer);
            }
        }
        forms._varlenas = varlenas;
        forms
    }

    fn datums(&self) -> DatumColumn {
        column(&self.values, &self.isnull)
    }

    fn bytes(&self) -> BytesColumn {
        BytesColumn {
            nrows: self.values.len() as i32,
            isnull: self.bytes_isnull.as_ptr(),
            data: self.data.as_ptr(),
            lengths: self.lengths.as_ptr(),
            ..BytesColumn::EMPTY
        }
    }
}

impl Drop for Forms {
    fn drop(&mut self) {
        for buffer in self.buffers.drain(..) {
            // SAFETY: each pointer came from `Box::into_raw` and is freed once.
            drop(unsafe { Box::from_raw(buffer) });
        }
    }
}

/// An argument of either form.
fn arg_of(datums: &DatumColumn, bytes: &BytesColumn, as_bytes: bool) -> TextArg {
    if as_bytes {
        TextArg {
            column: ptr::null(),
            scalar: 0,
            bytes,
        }
    } else {
        arg(datums)
    }
}

/// What a call wrote: its code and status, and its outputs, the slots of
/// rows not selected among them, filled before the call.
#[derive(Debug, PartialEq)]
struct Outcome {
    code: Code,
    sqlstate: String,
    message: String,
    masks: Vec<Vec<u64>>,
    slots: Vec<Vec<i32>>,
    words: Vec<i64>,
}

/// A slot of a row not selected before a call, which it must keep.
const UNTOUCHED: i32 = i32::MIN + 7;

/// A selection of `nrows` rows, as its words.
struct Rows {
    nrows: usize,
    words: Vec<u64>,
}

impl Rows {
    fn of(selected: &[bool]) -> Self {
        Rows {
            nrows: selected.len(),
            words: words(selected),
        }
    }

    /// An output mask before a call: every row set, so that a call must
    /// clear the rows it does not set.
    fn full(&self) -> Vec<u64> {
        words(&vec![true; self.nrows])
    }
}

fn compare(equal: bool, bpchar: bool, left: &TextArg, right: &TextArg, rows: &Rows) -> Outcome {
    let full = rows.full();
    let (nrows, rows) = (rows.nrows, &rows.words[..]);
    let (mut kept, mut rest) = (rows.to_vec(), full.clone());
    let mut status = Status::new();
    // SAFETY: local arguments, masks and status.
    let code = unsafe {
        tess_text_compare(
            equal,
            bpchar,
            left,
            right,
            &mut mask(nrows, &mut kept),
            &mut mask(nrows, &mut rest),
            &raw mut status,
        )
    };
    Outcome {
        code,
        sqlstate: status.sqlstate(),
        message: status.message(),
        masks: vec![kept, rest],
        slots: Vec::new(),
        words: Vec::new(),
    }
}

fn starts_with(column: &TextArg, prefix: &[u8], rows: &Rows) -> Outcome {
    let full = rows.full();
    let (nrows, rows) = (rows.nrows, &rows.words[..]);
    let (mut kept, mut rest) = (rows.to_vec(), full.clone());
    let mut status = Status::new();
    // SAFETY: local arguments, masks, prefix and status.
    let code = unsafe {
        tess_text_starts_with(
            column,
            prefix.as_ptr().cast(),
            prefix.len(),
            &mut mask(nrows, &mut kept),
            &mut mask(nrows, &mut rest),
            &raw mut status,
        )
    };
    Outcome {
        code,
        sqlstate: status.sqlstate(),
        message: status.message(),
        masks: vec![kept, rest],
        slots: Vec::new(),
        words: Vec::new(),
    }
}

fn like(column: &TextArg, pattern: &[u8], negate: bool, rows: &Rows) -> Outcome {
    let full = rows.full();
    let (nrows, rows) = (rows.nrows, &rows.words[..]);
    let (mut kept, mut rest) = (rows.to_vec(), full.clone());
    let mut simple = false;
    let mut status = Status::new();
    // SAFETY: local arguments, masks, pattern, flag and status.
    let code = unsafe {
        tess_text_like(
            column,
            pattern.as_ptr().cast(),
            pattern.len(),
            negate,
            &mut mask(nrows, &mut kept),
            &mut mask(nrows, &mut rest),
            &raw mut simple,
            &raw mut status,
        )
    };
    Outcome {
        code,
        sqlstate: status.sqlstate(),
        message: status.message(),
        masks: vec![kept, rest, vec![u64::from(simple)]],
        slots: Vec::new(),
        words: Vec::new(),
    }
}

fn lengths(length: i32, chars: i32, column: &TextArg, rows: &Rows) -> Outcome {
    let full = rows.full();
    let (nrows, rows) = (rows.nrows, &rows.words[..]);
    let mut values = vec![UNTOUCHED; nrows];
    let (mut present, mut rest) = (full.clone(), full.clone());
    let mut status = Status::new();
    // SAFETY: local arguments, masks, slots and status.
    let code = unsafe {
        tess_text_lengths(
            length,
            chars,
            column,
            &mask(nrows, &mut rows.to_vec()),
            values.as_mut_ptr(),
            &mut mask(nrows, &mut present),
            &mut mask(nrows, &mut rest),
            &raw mut status,
        )
    };
    Outcome {
        code,
        sqlstate: status.sqlstate(),
        message: status.message(),
        masks: vec![present, rest],
        slots: vec![values],
        words: Vec::new(),
    }
}

fn pieces(
    piece: i32,
    first: i32,
    second: i32,
    has_second: bool,
    chars: i32,
    column: &TextArg,
    rows: &Rows,
) -> Outcome {
    let full = rows.full();
    let (nrows, rows) = (rows.nrows, &rows.words[..]);
    let (mut starts, mut sizes) = (vec![UNTOUCHED; nrows], vec![UNTOUCHED; nrows]);
    let (mut present, mut rest) = (full.clone(), full.clone());
    let mut status = Status::new();
    // SAFETY: local arguments, masks, slots and status.
    let code = unsafe {
        tess_text_pieces(
            piece,
            first,
            second,
            has_second,
            chars,
            column,
            &mask(nrows, &mut rows.to_vec()),
            starts.as_mut_ptr(),
            sizes.as_mut_ptr(),
            &mut mask(nrows, &mut present),
            &mut mask(nrows, &mut rest),
            &raw mut status,
        )
    };
    if code != Code::Ok {
        // A failure leaves its outputs unspecified.
        return Outcome {
            code,
            sqlstate: status.sqlstate(),
            message: status.message(),
            masks: Vec::new(),
            slots: Vec::new(),
            words: Vec::new(),
        };
    }
    Outcome {
        code,
        sqlstate: status.sqlstate(),
        message: status.message(),
        masks: vec![present, rest],
        slots: vec![starts, sizes],
        words: Vec::new(),
    }
}

/// A word of a row not selected before a call, which it must keep.
const UNTOUCHED_WORD: i64 = i64::MIN + 7;

fn sort_words(column: &TextArg, rows: &Rows) -> Outcome {
    let full = rows.full();
    let (nrows, rows) = (rows.nrows, &rows.words[..]);
    let mut words = vec![UNTOUCHED_WORD; nrows];
    let (mut present, mut rest) = (full.clone(), full.clone());
    let mut status = Status::new();
    // SAFETY: local arguments, masks, slots and status.
    let code = unsafe {
        tess_text_sort_words(
            column,
            &mask(nrows, &mut rows.to_vec()),
            words.as_mut_ptr(),
            &mut mask(nrows, &mut present),
            &mut mask(nrows, &mut rest),
            &raw mut status,
        )
    };
    Outcome {
        code,
        sqlstate: status.sqlstate(),
        message: status.message(),
        masks: vec![present, rest],
        slots: Vec::new(),
        words,
    }
}

/// Whether every slot of a row not selected kept its value.
fn untouched(outcome: &Outcome, selected: &[bool]) -> bool {
    let words = outcome
        .words
        .iter()
        .zip(selected)
        .all(|(&word, &asked)| asked || word == UNTOUCHED_WORD);
    words
        && outcome.slots.iter().all(|slots| {
            slots
                .iter()
                .zip(selected)
                .all(|(&slot, &asked)| asked || slot == UNTOUCHED)
        })
}

fn concat(pieces: Vec<&'static str>) -> Vec<u8> {
    pieces.concat().into_bytes()
}

/// Strings of ASCII, two- and three-byte characters, spaces and `%`; some
/// past 126 bytes, which only a 4-byte header holds, but under Miri, which
/// runs each case slowly and takes both headers by `Case::long`.
fn text() -> impl Strategy<Value = Vec<u8>> {
    let piece = prop_oneof![
        Just("a"),
        Just("b"),
        Just(" "),
        Just("é"),
        Just("€"),
        Just("%")
    ];
    let long = if cfg!(miri) { 0 } else { 1 };
    prop_oneof![
        8 => prop::collection::vec(piece.clone(), 0..12).prop_map(concat),
        long => prop::collection::vec(piece, 100..140).prop_map(concat),
    ]
}

/// A pattern of literals and `%`, as the kernels' matcher takes it.
fn pattern() -> impl Strategy<Value = Vec<u8>> {
    let literal = prop_oneof![Just(""), Just("a"), Just("é"), Just("b a"), Just("€ ")];
    prop::collection::vec(literal, 1..4).prop_map(|pieces| pieces.join("%").into_bytes())
}

#[derive(Debug, Clone)]
struct Case {
    strings: Vec<Option<Vec<u8>>>,
    others: Vec<Option<Vec<u8>>>,
    long: Vec<bool>,
    selected: Vec<bool>,
    scalar: Vec<u8>,
    pattern: Vec<u8>,
    first: i32,
    second: i32,
}

fn cases() -> impl Strategy<Value = Case> {
    // Past two words of rows; under Miri past one.
    let most: usize = if cfg!(miri) { 66 } else { 130 };
    (0..=most).prop_flat_map(|nrows| {
        let strings = || prop::collection::vec(prop::option::weighted(0.85, text()), nrows);
        let flags = || prop::collection::vec(any::<bool>(), nrows);
        (
            strings(),
            strings(),
            flags(),
            flags(),
            text(),
            pattern(),
            -3..12_i32,
            -3..12_i32,
        )
            .prop_map(
                |(strings, others, long, selected, scalar, pattern, first, second)| Case {
                    strings,
                    others,
                    long,
                    selected,
                    scalar,
                    pattern,
                    first,
                    second,
                },
            )
    })
}

/// Each entry point gives over the bytes form of a column what it gives
/// over its `Datum` values, an argument of each form beside one of the
/// other among them, and keeps the slots of the rows not selected. Too
/// slow for Miri, which runs the fixed cases below.
#[test]
#[cfg_attr(miri, ignore = "a hundred calls a case")]
fn every_function_reads_both_forms_alike() {
    property(cases(), |case| -> Result<(), String> {
        let rows = Rows::of(&case.selected);
        let (one, two) = (
            Forms::new(&case.strings, &case.long, &case.selected),
            Forms::new(&case.others, &case.long, &case.selected),
        );
        let (one_datums, one_bytes, two_datums, two_bytes) =
            (one.datums(), one.bytes(), two.datums(), two.bytes());
        let column = |as_bytes| arg_of(&one_datums, &one_bytes, as_bytes);
        let other = |as_bytes| arg_of(&two_datums, &two_bytes, as_bytes);
        let mut scalars = Strings(Vec::new());
        let scalar = TextArg {
            column: ptr::null(),
            scalar: scalars.long_bytes(&case.scalar),
            bytes: ptr::null(),
        };
        let same = |what: &str, datums: Outcome, bytes: Outcome| -> Result<(), String> {
            // A failure the strings make, never one of the call's arguments.
            if !matches!(datums.code, Code::Ok | Code::DataException) {
                return Err(format!("{what}: {datums:?}"));
            }
            if datums != bytes {
                return Err(format!(
                    "{what}: {datums:?} over Datums, {bytes:?} over bytes"
                ));
            }
            if !untouched(&bytes, &case.selected) {
                return Err(format!("{what}: a row not selected was written"));
            }
            Ok(())
        };
        for (equal, bpchar) in [(true, false), (false, false), (true, true), (false, true)] {
            let what = format!("compare equal {equal} bpchar {bpchar}");
            same(
                &what,
                compare(equal, bpchar, &column(false), &scalar, &rows),
                compare(equal, bpchar, &column(true), &scalar, &rows),
            )?;
            same(
                &what,
                compare(equal, bpchar, &scalar, &column(false), &rows),
                compare(equal, bpchar, &scalar, &column(true), &rows),
            )?;
            for (left, right) in [(true, true), (true, false), (false, true)] {
                same(
                    &format!("{what}, columns of bytes {left} and {right}"),
                    compare(equal, bpchar, &column(false), &other(false), &rows),
                    compare(equal, bpchar, &column(left), &other(right), &rows),
                )?;
            }
        }
        same(
            "starts_with",
            starts_with(&column(false), &case.scalar, &rows),
            starts_with(&column(true), &case.scalar, &rows),
        )?;
        for negate in [false, true] {
            same(
                &format!("like negate {negate}"),
                like(&column(false), &case.pattern, negate, &rows),
                like(&column(true), &case.pattern, negate, &rows),
            )?;
        }
        for length in 0..3 {
            for chars in 0..2 {
                same(
                    &format!("lengths {length} chars {chars}"),
                    lengths(length, chars, &column(false), &rows),
                    lengths(length, chars, &column(true), &rows),
                )?;
            }
        }
        same(
            "sort words",
            sort_words(&column(false), &rows),
            sort_words(&column(true), &rows),
        )?;
        for piece in 0..6 {
            for has_second in [false, true] {
                for chars in 0..2 {
                    let at = |as_bytes| {
                        pieces(
                            piece,
                            case.first,
                            case.second,
                            has_second,
                            chars,
                            &column(as_bytes),
                            &rows,
                        )
                    };
                    same(
                        &format!("piece {piece} second {has_second} chars {chars}"),
                        at(false),
                        at(true),
                    )?;
                }
            }
        }
        Ok(())
    });
}

/// The bytes form over a sparse selection, as Miri reads it: the rows
/// selected are a `NULL` row, an empty string and two strings in words
/// apart; every other row holds a null pointer and the largest length;
/// the results of every entry point are the strings'.
#[test]
fn a_bytes_column_s_rows_not_selected_are_never_read() {
    let nrows = 130;
    let mut strings: Vec<Option<Vec<u8>>> = vec![Some(b"never".to_vec()); nrows];
    strings[1] = Some("héllo".as_bytes().to_vec());
    strings[64] = None;
    strings[70] = Some(Vec::new());
    strings[129] = Some(b"ab  ".to_vec());
    let mut selected = vec![false; nrows];
    for row in [1, 64, 70, 129] {
        selected[row] = true;
    }
    let forms = Forms::new(&strings, &vec![false; nrows], &selected);
    let (datums, bytes) = (forms.datums(), forms.bytes());
    let column = arg_of(&datums, &bytes, true);
    let rows = Rows::of(&selected);
    let counted = lengths(0, 1, &column, &rows);
    assert_eq!(counted.code, Code::Ok, "{}", counted.message);
    assert!(untouched(&counted, &selected));
    assert_eq!(
        [1, 70, 129].map(|row| counted.slots[0][row]),
        [5, 0, 4],
        "the characters of the strings selected"
    );
    assert_eq!(
        counted.masks[0],
        words(&{
            let mut present = selected.clone();
            present[64] = false;
            present
        })
    );
    let started = starts_with(&column, b"h", &rows);
    assert_eq!(started.code, Code::Ok, "{}", started.message);
    assert_eq!(
        started.masks[0],
        words(&{
            let mut kept = vec![false; nrows];
            kept[1] = true;
            kept
        })
    );
    // LIKE of pieces, and NOT LIKE: the empty string matches only `%`.
    for (pattern, negate, kept) in [
        (&b"%l%o"[..], false, vec![1]),
        (&b"%"[..], false, vec![1, 70, 129]),
        (&b"a%"[..], true, vec![1, 70]),
    ] {
        let matched = like(&column, pattern, negate, &rows);
        assert_eq!(matched.code, Code::Ok, "{}", matched.message);
        assert_eq!(matched.masks[2], vec![1], "the kernels take the pattern");
        assert_eq!(
            matched.masks[0],
            words(&{
                let mut rows = vec![false; nrows];
                for row in kept {
                    rows[row] = true;
                }
                rows
            })
        );
    }
    // The sort words: the strings' starts, the NULL row and the rows not
    // selected left.
    let worded = sort_words(&column, &rows);
    assert_eq!(worded.code, Code::Ok, "{}", worded.message);
    assert!(untouched(&worded, &selected));
    let word = |bytes: &[u8]| {
        let mut first = [0_u8; 8];
        first[..bytes.len().min(8)].copy_from_slice(&bytes[..bytes.len().min(8)]);
        (u64::from_be_bytes(first) ^ (1 << 63)) as i64
    };
    assert_eq!(
        [1, 70, 129].map(|row| worded.words[row]),
        [word("héllo".as_bytes()), word(b""), word(b"ab  ")]
    );
    assert_eq!(
        worded.words[64], UNTOUCHED_WORD,
        "a NULL row's word is not written"
    );
    let mut scalars = Strings(Vec::new());
    let scalar = TextArg {
        column: ptr::null(),
        scalar: scalars.short("ab"),
        bytes: ptr::null(),
    };
    // Beside a column of Datum values of the same strings, a bytes column
    // on either side.
    let datum_arg = arg(&datums);
    for (left, right) in [(&column, &datum_arg), (&datum_arg, &column)] {
        let equal = compare(true, false, left, right, &rows);
        assert_eq!(equal.code, Code::Ok, "{}", equal.message);
        assert_eq!(
            equal.masks[0],
            words(&{
                let mut kept = selected.clone();
                kept[64] = false;
                kept
            })
        );
    }
    let equal = compare(true, true, &column, &scalar, &rows);
    assert_eq!(
        equal.masks[0],
        words(&{
            let mut kept = vec![false; nrows];
            kept[129] = true;
            kept
        })
    );
    let trimmed = pieces(3, 0, 0, false, 1, &column, &rows);
    assert_eq!(trimmed.code, Code::Ok);
    assert!(untouched(&trimmed, &selected));
    assert_eq!(
        [1, 70, 129].map(|row| (trimmed.slots[0][row], trimmed.slots[1][row])),
        [(0, 6), (0, 0), (0, 2)]
    );
}

/// An argument of two columns, a bytes column of another row count, and a
/// scalar where a column is due are refused, as a column of `Datum` values
/// is.
#[test]
fn a_bytes_column_that_does_not_fit_is_refused() {
    let nrows = 3;
    let strings = vec![Some(b"a".to_vec()), None, Some(b"bc".to_vec())];
    let selected = vec![true; nrows];
    let forms = Forms::new(&strings, &[false; 3], &selected);
    let (datums, mut bytes) = (forms.datums(), forms.bytes());
    let rows = Rows::of(&selected);
    let both = TextArg {
        column: &datums,
        scalar: 0,
        bytes: &bytes,
    };
    assert_eq!(lengths(2, 0, &both, &rows).code, Code::InvalidArgument);
    let mut scalars = Strings(Vec::new());
    let scalar = TextArg {
        column: ptr::null(),
        scalar: scalars.short("a"),
        bytes: ptr::null(),
    };
    assert_eq!(lengths(2, 0, &scalar, &rows).code, Code::InvalidArgument);
    bytes.nrows = 2;
    let short = arg_of(&datums, &bytes, true);
    assert_eq!(lengths(2, 0, &short, &rows).code, Code::InvalidArgument);
    assert_eq!(
        compare(true, false, &short, &arg(&datums), &rows).code,
        Code::InvalidArgument
    );
    bytes.nrows = 3;
    bytes.struct_size = BytesColumn::MIN_SIZE - 8;
    let small = arg_of(&datums, &bytes, true);
    assert_eq!(lengths(2, 0, &small, &rows).code, Code::InvalidArgument);
}

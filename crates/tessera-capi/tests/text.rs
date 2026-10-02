//! The text entry points called with raw pointers, as C calls them:
//! strings of both varlena headers, a compressed one left in the rest, the
//! error of a negative substring length.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use std::ptr;

use tessera_capi::c::{
    Code, DatumColumn, Mask, Status, TextArg, tess_text_compare, tess_text_lengths, tess_text_like,
    tess_text_pieces, tess_text_starts_with,
};

/// Varlenas kept at stable addresses, each in words so that a 4-byte
/// header is aligned.
#[expect(
    clippy::vec_box,
    reason = "each string keeps its address as the Vec grows"
)]
struct Strings(Vec<Box<[u64; 8]>>);

impl Strings {
    fn add(&mut self, bytes: &[u8]) -> u64 {
        let mut words = Box::new([0_u64; 8]);
        // SAFETY: the words hold 64 bytes, more than any string here.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), words.as_mut_ptr().cast(), bytes.len()) };
        let datum = words.as_ptr() as u64;
        self.0.push(words);
        datum
    }

    /// A string with a 1-byte header, as a table stores a short one.
    fn short(&mut self, text: &str) -> u64 {
        let mut bytes = vec![(((text.len() + 1) << 1) | 1) as u8];
        bytes.extend_from_slice(text.as_bytes());
        self.add(&bytes)
    }

    /// A string with a 4-byte header.
    fn long(&mut self, text: &str) -> u64 {
        let mut bytes = (((text.len() + 4) as u32) << 2).to_le_bytes().to_vec();
        bytes.extend_from_slice(text.as_bytes());
        self.add(&bytes)
    }

    /// A header of a compressed value: never read in place.
    fn compressed(&mut self) -> u64 {
        self.add(&(((12_u32) << 2) | 0b10).to_le_bytes())
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

fn mask(nrows: usize, words: &mut [u64]) -> Mask {
    Mask {
        nrows: nrows as i32,
        bits: words.as_mut_ptr(),
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
    };
    let left = TextArg {
        column: &raw const source,
        scalar: 0,
    };
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
    // LIKE of pieces, a pattern it does not take.
    let (mut rows, mut rest, mut simple) = ([0b1_1111_u64], [0_u64], false);
    let pattern = b"a%c";
    // SAFETY: as above; the pattern is a local byte string.
    let code = unsafe {
        tess_text_like(
            &raw const source,
            pattern.as_ptr().cast(),
            pattern.len(),
            false,
            &mut mask(5, &mut rows),
            &mut mask(5, &mut rest),
            &raw mut simple,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert!(simple);
    assert_eq!((rows[0], rest[0]), (0b0011, 0b1000));
    let underscore = b"a_c";
    let mut untouched = [0b1_1111_u64];
    // SAFETY: as above.
    let code = unsafe {
        tess_text_like(
            &raw const source,
            underscore.as_ptr().cast(),
            underscore.len(),
            false,
            &mut mask(5, &mut untouched),
            &mut mask(5, &mut rest),
            &raw mut simple,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert!(!simple);
    assert_eq!(untouched[0], 0b1_1111);
    let prefix = b"ab";
    let (mut rows, mut rest) = ([0b1_1111_u64], [0_u64]);
    // SAFETY: as above.
    let code = unsafe {
        tess_text_starts_with(
            &raw const source,
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

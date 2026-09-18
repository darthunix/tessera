//! The C entry points called from Rust with raw pointers, as C would call
//! them; the C test module covers the same ground against the linked
//! static library.

use std::ptr;

use tessera_capi::c::{Code, DatumColumn, Mask, Status, tess_int4_filter, tess_kernels_test_panic};
use tessera_capi::c::{tess_kernels_abi_version, tess_kernels_layout};

/// A column of `nrows` int4 Datums with every fifth row NULL, and the
/// selection words of every row but each third.
struct Fixture {
    values: Vec<u64>,
    isnull: Vec<bool>,
    words: Vec<u64>,
}

impl Fixture {
    fn new(nrows: usize) -> Self {
        let values = (0..nrows)
            .map(|row| i64::from((row as i32).wrapping_mul(7919) % 1000 - 500) as u64)
            .collect();
        let isnull = (0..nrows).map(|row| row % 5 == 0).collect();
        let mut words = vec![0; nrows.div_ceil(64)];
        for row in (0..nrows).filter(|row| row % 3 != 1) {
            words[row / 64] |= 1 << (row % 64);
        }
        Self {
            values,
            isnull,
            words,
        }
    }

    fn column(&self) -> DatumColumn {
        DatumColumn {
            struct_size: size_of::<DatumColumn>(),
            values: self.values.as_ptr(),
            isnull: self.isnull.as_ptr(),
            nrows: self.values.len() as i32,
        }
    }

    fn mask(&mut self) -> Mask {
        Mask {
            nrows: self.values.len() as i32,
            bits: self.words.as_mut_ptr(),
        }
    }

    /// The rows a `> 0` filter keeps, by a scalar loop.
    fn expected_words(&self) -> Vec<u64> {
        let mut words = vec![0; self.words.len()];
        for row in 0..self.values.len() {
            let selected = self.words[row / 64] & (1 << (row % 64)) != 0;
            if selected && !self.isnull[row] && (self.values[row] as i32) > 0 {
                words[row / 64] |= 1 << (row % 64);
            }
        }
        words
    }
}

#[test]
fn abi_and_layout_match_the_rust_types() {
    assert_eq!(tess_kernels_abi_version(), 0);
    assert_eq!(tess_kernels_layout(0), size_of::<Mask>());
    assert_eq!(tess_kernels_layout(1), size_of::<DatumColumn>());
    assert_eq!(tess_kernels_layout(2), 24);
    assert_eq!(tess_kernels_layout(3), size_of::<Status>());
    assert_eq!(tess_kernels_layout(4), 18);
    assert_eq!(tess_kernels_layout(5), 0);
}

#[test]
fn filter_narrows_the_mask_and_reports_success() {
    let mut fixture = Fixture::new(200);
    let expected = fixture.expected_words();
    let column = fixture.column();
    let mut rows = fixture.mask();
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else.
    let code = unsafe {
        tess_int4_filter(
            &raw const column,
            ptr::null(),
            &raw mut rows,
            4,
            0,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!(status.code, Code::Ok);
    assert_eq!(status.message(), "");
    assert_eq!(status.sqlstate(), "");
    assert_eq!(fixture.words, expected);
    // With the readiness mask covering the selection, the same result; with
    // a readiness mask short of it, an error.
    let mut fixture = Fixture::new(200);
    let mut prepared_words = fixture.words.clone();
    let mut prepared = Mask {
        nrows: 200,
        bits: prepared_words.as_mut_ptr(),
    };
    let column = fixture.column();
    let mut rows = fixture.mask();
    // SAFETY: as above; `prepared` has its own words.
    let code = unsafe {
        tess_int4_filter(
            &raw const column,
            &raw const prepared,
            &raw mut rows,
            4,
            0,
            ptr::null_mut(),
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!(fixture.words, expected);
    prepared_words[0] &= !(1 << 2);
    prepared.bits = prepared_words.as_mut_ptr();
    let mut fixture = Fixture::new(200);
    let column = fixture.column();
    let mut rows = fixture.mask();
    // SAFETY: as above.
    let code = unsafe {
        tess_int4_filter(
            &raw const column,
            &raw const prepared,
            &raw mut rows,
            4,
            0,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
    assert!(
        status.message().contains("unprepared"),
        "{}",
        status.message()
    );
}

#[test]
fn invalid_arguments_leave_the_mask_alone() {
    let mut fixture = Fixture::new(70);
    let original = fixture.words.clone();
    let mut column = fixture.column();
    column.nrows = 69;
    let mut rows = fixture.mask();
    let mut status = Status::new();
    // SAFETY: the column claims fewer rows than it has, which is rejected
    // before any read.
    let code = unsafe {
        tess_int4_filter(
            &raw const column,
            ptr::null(),
            &raw mut rows,
            4,
            0,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
    assert_eq!(status.code, Code::InvalidArgument);
    assert_eq!(status.sqlstate(), "XX000");
    assert!(
        status.message().contains("row counts"),
        "{}",
        status.message()
    );
    assert_eq!(fixture.words, original);
    let column = fixture.column();
    let mut rows = fixture.mask();
    // SAFETY: local buffers; the operation code is unknown.
    let code = unsafe {
        tess_int4_filter(
            &raw const column,
            ptr::null(),
            &raw mut rows,
            9,
            0,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
    assert!(status.message().contains("unknown"), "{}", status.message());
    // SAFETY: null pointers are rejected before use.
    let code = unsafe {
        tess_int4_filter(
            ptr::null(),
            ptr::null(),
            &raw mut rows,
            4,
            0,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
    assert_eq!(fixture.words, original);
}

#[test]
fn a_panic_becomes_a_status_and_the_library_stays_usable() {
    let mut status = Status::new();
    // SAFETY: a local status.
    let code = unsafe { tess_kernels_test_panic(&raw mut status) };
    assert_eq!(code, Code::Panic);
    assert_eq!(status.code, Code::Panic);
    assert_eq!(status.sqlstate(), "XX000");
    assert!(
        status.message().contains("injected"),
        "{}",
        status.message()
    );
    let mut fixture = Fixture::new(64);
    let expected = fixture.expected_words();
    let column = fixture.column();
    let mut rows = fixture.mask();
    // SAFETY: local buffers.
    let code = unsafe {
        tess_int4_filter(
            &raw const column,
            ptr::null(),
            &raw mut rows,
            4,
            0,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!(status.code, Code::Ok);
    assert_eq!(fixture.words, expected);
}

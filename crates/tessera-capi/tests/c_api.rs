//! The C entry points called from Rust with raw pointers, as C would call
//! them; the C test module covers the same ground against the linked
//! static library.

use std::ptr;

use tessera_capi::c::{
    Code, DatumColumn, Mask, Status, tess_int4_count, tess_int4_filter, tess_int4_max,
    tess_int4_min, tess_int4_sum, tess_kernels_abi_version, tess_kernels_layout,
    tess_kernels_test_panic,
};

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
fn aggregates_match_a_scalar_loop_and_are_null_without_rows() {
    let mut fixture = Fixture::new(200);
    let (mut count, mut sum, mut least, mut greatest) = (0, 0, i32::MAX, i32::MIN);
    for row in 0..200 {
        let selected = fixture.words[row / 64] & (1 << (row % 64)) != 0;
        if selected && !fixture.isnull[row] {
            let value = fixture.values[row] as i32;
            count += 1;
            sum += i64::from(value);
            least = least.min(value);
            greatest = greatest.max(value);
        }
    }
    let column = fixture.column();
    let rows = fixture.mask();
    let mut status = Status::new();
    let (mut got_count, mut got_sum, mut got_min, mut got_max) = (-1, -1, -1, -1);
    let (mut sum_null, mut min_null, mut max_null) = (true, true, true);
    // SAFETY: local buffers of the declared sizes.
    unsafe {
        let column = &raw const column;
        let rows = &raw const rows;
        assert_eq!(
            tess_int4_count(
                column,
                ptr::null(),
                rows,
                &raw mut got_count,
                &raw mut status
            ),
            Code::Ok
        );
        assert_eq!(
            tess_int4_sum(
                column,
                ptr::null(),
                rows,
                &raw mut sum_null,
                &raw mut got_sum,
                &raw mut status
            ),
            Code::Ok
        );
        assert_eq!(
            tess_int4_min(
                column,
                ptr::null(),
                rows,
                &raw mut min_null,
                &raw mut got_min,
                &raw mut status
            ),
            Code::Ok
        );
        assert_eq!(
            tess_int4_max(
                column,
                ptr::null(),
                rows,
                &raw mut max_null,
                &raw mut got_max,
                &raw mut status
            ),
            Code::Ok
        );
    }
    assert_eq!(
        (got_count, got_sum, got_min, got_max),
        (count, sum, least, greatest)
    );
    assert_eq!((sum_null, min_null, max_null), (false, false, false));
    // Without selected rows: count 0, the rest NULL.
    fixture.words.iter_mut().for_each(|word| *word = 0);
    let column = fixture.column();
    let rows = fixture.mask();
    // SAFETY: as above.
    unsafe {
        let column = &raw const column;
        let rows = &raw const rows;
        assert_eq!(
            tess_int4_count(
                column,
                ptr::null(),
                rows,
                &raw mut got_count,
                ptr::null_mut()
            ),
            Code::Ok
        );
        assert_eq!(
            tess_int4_sum(
                column,
                ptr::null(),
                rows,
                &raw mut sum_null,
                &raw mut got_sum,
                ptr::null_mut()
            ),
            Code::Ok
        );
        assert_eq!(
            tess_int4_min(
                column,
                ptr::null(),
                rows,
                &raw mut min_null,
                &raw mut got_min,
                ptr::null_mut()
            ),
            Code::Ok
        );
        assert_eq!(
            tess_int4_max(
                column,
                ptr::null(),
                rows,
                &raw mut max_null,
                &raw mut got_max,
                ptr::null_mut()
            ),
            Code::Ok
        );
    }
    assert_eq!((got_count, got_sum, got_min, got_max), (0, 0, 0, 0));
    assert_eq!((sum_null, min_null, max_null), (true, true, true));
    // A dimension error writes no result.
    let mut column = fixture.column();
    column.nrows = 199;
    got_count = 7;
    // SAFETY: rejected before any read.
    let code = unsafe {
        tess_int4_count(
            &raw const column,
            ptr::null(),
            &raw const rows,
            &raw mut got_count,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
    assert_eq!(got_count, 7);
    // SAFETY: a null result pointer is rejected before any read.
    let code = unsafe {
        tess_int4_sum(
            &raw const column,
            ptr::null(),
            &raw const rows,
            ptr::null_mut(),
            &raw mut got_sum,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
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

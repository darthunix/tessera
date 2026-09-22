//! The C entry points called from Rust with raw pointers, as C would call
//! them; the C test module covers the same ground against the linked
//! static library.

use std::ptr;

use tessera_capi::c::{
    Code, DatumColumn, Mask, Status, tess_count, tess_int4_arith_columns, tess_int4_arith_scalar,
    tess_int4_arith_scalar_left, tess_int4_count, tess_int4_filter, tess_int4_hash,
    tess_int4_hash_next, tess_int4_max, tess_int4_min, tess_int4_sum, tess_int8_arith_columns,
    tess_int8_arith_scalar, tess_int8_arith_scalar_left, tess_int8_filter, tess_int8_max,
    tess_int8_min, tess_kernels_abi_version, tess_kernels_layout, tess_kernels_test_panic,
};
use tessera_kernels::int32::{hash_combine, murmurhash32};

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

/// The int4 fixture's values shifted past the int4 range, as int8 Datums:
/// a 32-bit read would see zeros and keep no row.
fn int8_fixture(nrows: usize) -> Fixture {
    let mut fixture = Fixture::new(nrows);
    fixture.values = fixture
        .values
        .iter()
        .map(|&datum| ((datum as i32 as i64) << 33) as u64)
        .collect();
    fixture
}

#[test]
fn int8_filter_reads_whole_datums() {
    let mut fixture = int8_fixture(200);
    let expected = Fixture::new(200).expected_words();
    let column = fixture.column();
    let mut rows = fixture.mask();
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else.
    let code = unsafe {
        tess_int8_filter(
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
    // A scalar beyond the int4 range keeps only the values above it.
    let mut fixture = int8_fixture(200);
    let column = fixture.column();
    let mut rows = fixture.mask();
    // SAFETY: as above.
    let code = unsafe {
        tess_int8_filter(
            &raw const column,
            ptr::null(),
            &raw mut rows,
            4,
            400 << 33,
            ptr::null_mut(),
        )
    };
    assert_eq!(code, Code::Ok);
    let kept: Vec<usize> = (0..200)
        .filter(|row| fixture.words[row / 64] & (1 << (row % 64)) != 0)
        .collect();
    assert!(!kept.is_empty());
    for row in 0..200 {
        let selected = row % 3 != 1 && row % 5 != 0;
        let above = (fixture.values[row] as i64) > (400 << 33);
        assert_eq!(kept.contains(&row), selected && above, "{row}");
    }
    // An unknown operation is an error that leaves the mask alone.
    let mut fixture = int8_fixture(200);
    let original = fixture.words.clone();
    let column = fixture.column();
    let mut rows = fixture.mask();
    // SAFETY: as above.
    let code = unsafe {
        tess_int8_filter(
            &raw const column,
            ptr::null(),
            &raw mut rows,
            6,
            0,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
    assert_eq!(fixture.words, original);
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

/// A small column and its selection for the arithmetic tests.
fn small(values: &[i32], isnull: &[bool]) -> (Vec<u64>, Vec<bool>, u64) {
    let datums = values.iter().map(|&v| i64::from(v) as u64).collect();
    let selected = (1u64 << values.len()) - 1;
    (datums, isnull.to_vec(), selected)
}

#[test]
fn arithmetic_writes_results_and_reports_postgresql_codes() {
    let mut fixture = Fixture::new(200);
    let column = fixture.column();
    let rows = fixture.mask();
    let mut values = vec![i32::MIN; 200];
    let mut result_words = vec![0; 4];
    let mut non_nulls = Mask {
        nrows: 200,
        bits: result_words.as_mut_ptr(),
    };
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else.
    let code = unsafe {
        tess_int4_arith_scalar(
            0,
            &raw const column,
            7,
            ptr::null(),
            &raw const rows,
            values.as_mut_ptr(),
            &raw mut non_nulls,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    for row in 0..200 {
        let selected = fixture.words[row / 64] & (1 << (row % 64)) != 0;
        let present = result_words[row / 64] & (1 << (row % 64)) != 0;
        assert_eq!(present, selected && !fixture.isnull[row], "row {row}");
        if present {
            assert_eq!(values[row], fixture.values[row] as i32 + 7, "row {row}");
        }
    }
    // scalar - column and column + column on a small column with a NULL.
    let (datums, isnull, selected) = small(&[10, 20, 30], &[false, true, false]);
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: datums.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 3,
    };
    let mut selection = [selected];
    let rows = Mask {
        nrows: 3,
        bits: selection.as_mut_ptr(),
    };
    let mut values = [0; 3];
    let mut result_words = [0];
    let mut non_nulls = Mask {
        nrows: 3,
        bits: result_words.as_mut_ptr(),
    };
    // SAFETY: as above.
    let code = unsafe {
        tess_int4_arith_scalar_left(
            1,
            100,
            &raw const column,
            ptr::null(),
            &raw const rows,
            values.as_mut_ptr(),
            &raw mut non_nulls,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!(result_words, [0b101]);
    assert_eq!((values[0], values[2]), (90, 70));
    // SAFETY: as above; the column is both operands.
    let code = unsafe {
        tess_int4_arith_columns(
            2,
            &raw const column,
            ptr::null(),
            &raw const column,
            ptr::null(),
            &raw const rows,
            values.as_mut_ptr(),
            &raw mut non_nulls,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!(result_words, [0b101]);
    assert_eq!((values[0], values[2]), (100, 900));
    // PostgreSQL's error codes.
    let (datums, isnull, selected) = small(&[i32::MAX, i32::MIN, 1], &[false, false, false]);
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: datums.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 3,
    };
    let mut selection = [selected];
    let rows = Mask {
        nrows: 3,
        bits: selection.as_mut_ptr(),
    };
    for (op, scalar, expected, sqlstate) in [
        (0, 1, Code::IntegerOutOfRange, "22003"),
        (3, 0, Code::DivisionByZero, "22012"),
        (4, 0, Code::DivisionByZero, "22012"),
        (3, -1, Code::IntegerOutOfRange, "22003"),
        (7, 1, Code::InvalidArgument, "XX000"),
    ] {
        // SAFETY: as above.
        let code = unsafe {
            tess_int4_arith_scalar(
                op,
                &raw const column,
                scalar,
                ptr::null(),
                &raw const rows,
                values.as_mut_ptr(),
                &raw mut non_nulls,
                &raw mut status,
            )
        };
        assert_eq!(code, expected, "op {op} scalar {scalar}");
        assert_eq!(status.code, expected);
        assert_eq!(status.sqlstate(), sqlstate, "op {op} scalar {scalar}");
        assert!(!status.message().is_empty());
    }
    // A NULL operand never fails; x % -1 is 0.
    let (datums, isnull, selected) = small(&[i32::MIN, i32::MAX, 5], &[true, false, false]);
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: datums.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 3,
    };
    let mut selection = [selected];
    let rows = Mask {
        nrows: 3,
        bits: selection.as_mut_ptr(),
    };
    // SAFETY: as above.
    let code = unsafe {
        tess_int4_arith_scalar(
            4,
            &raw const column,
            -1,
            ptr::null(),
            &raw const rows,
            values.as_mut_ptr(),
            &raw mut non_nulls,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!(result_words, [0b110]);
    assert_eq!((values[1], values[2]), (0, 0));
}

#[test]
fn int8_arithmetic_writes_whole_results_and_reports_bigint_codes() {
    let mut fixture = int8_fixture(200);
    let column = fixture.column();
    let rows = fixture.mask();
    let mut values = vec![i64::MIN; 200];
    let mut result_words = vec![0; 4];
    let mut non_nulls = Mask {
        nrows: 200,
        bits: result_words.as_mut_ptr(),
    };
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else.
    let code = unsafe {
        tess_int8_arith_scalar(
            0,
            &raw const column,
            1 << 40,
            ptr::null(),
            &raw const rows,
            values.as_mut_ptr(),
            &raw mut non_nulls,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    for row in 0..200 {
        let selected = fixture.words[row / 64] & (1 << (row % 64)) != 0;
        let present = result_words[row / 64] & (1 << (row % 64)) != 0;
        assert_eq!(present, selected && !fixture.isnull[row], "row {row}");
        if present {
            assert_eq!(
                values[row],
                fixture.values[row] as i64 + (1 << 40),
                "row {row}"
            );
        }
    }
    // scalar - column and column * column on a small column with a NULL.
    let datums: Vec<u64> = [10_i64 << 20, 20 << 20, 30 << 20]
        .iter()
        .map(|&v| v as u64)
        .collect();
    let isnull = [false, true, false];
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: datums.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 3,
    };
    let mut selection = [0b111];
    let rows = Mask {
        nrows: 3,
        bits: selection.as_mut_ptr(),
    };
    let mut values = [0_i64; 3];
    let mut result_words = [0];
    let mut non_nulls = Mask {
        nrows: 3,
        bits: result_words.as_mut_ptr(),
    };
    // SAFETY: as above.
    let code = unsafe {
        tess_int8_arith_scalar_left(
            1,
            100 << 20,
            &raw const column,
            ptr::null(),
            &raw const rows,
            values.as_mut_ptr(),
            &raw mut non_nulls,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!(result_words, [0b101]);
    assert_eq!((values[0], values[2]), (90 << 20, 70 << 20));
    // SAFETY: as above; the column is both operands.
    let code = unsafe {
        tess_int8_arith_columns(
            2,
            &raw const column,
            ptr::null(),
            &raw const column,
            ptr::null(),
            &raw const rows,
            values.as_mut_ptr(),
            &raw mut non_nulls,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!(result_words, [0b101]);
    assert_eq!((values[0], values[2]), (100 << 40, 900 << 40));
    // PostgreSQL's codes and messages for bigint.
    let datums = [i64::MAX as u64, i64::MIN as u64, 1];
    let isnull = [false, false, false];
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: datums.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 3,
    };
    // SAFETY: as above.
    let code = unsafe {
        tess_int8_arith_scalar(
            0,
            &raw const column,
            1,
            ptr::null(),
            &raw const rows,
            values.as_mut_ptr(),
            &raw mut non_nulls,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::IntegerOutOfRange);
    assert_eq!(status.sqlstate(), "22003");
    assert_eq!(status.message(), "bigint out of range");
    for (op, scalar, code, sqlstate) in [
        (3, 0, Code::DivisionByZero, "22012"),
        (3, -1, Code::IntegerOutOfRange, "22003"),
        (7, 1, Code::InvalidArgument, "XX000"),
    ] {
        // SAFETY: as above.
        let got = unsafe {
            tess_int8_arith_scalar(
                op,
                &raw const column,
                scalar,
                ptr::null(),
                &raw const rows,
                values.as_mut_ptr(),
                &raw mut non_nulls,
                &raw mut status,
            )
        };
        assert_eq!(got, code, "op {op} scalar {scalar}");
        assert_eq!(status.sqlstate(), sqlstate, "op {op} scalar {scalar}");
    }
    // x % -1 is 0 for every value, i64::MIN included.
    // SAFETY: as above.
    let code = unsafe {
        tess_int8_arith_scalar(
            4,
            &raw const column,
            -1,
            ptr::null(),
            &raw const rows,
            values.as_mut_ptr(),
            &raw mut non_nulls,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!((result_words, values), ([0b111], [0, 0, 0]));
}

#[test]
fn int8_extremes_and_the_count_of_any_type_match_a_scalar_loop() {
    let mut fixture = int8_fixture(200);
    let (mut count, mut least, mut greatest) = (0, i64::MAX, i64::MIN);
    for row in 0..200 {
        let selected = fixture.words[row / 64] & (1 << (row % 64)) != 0;
        if selected && !fixture.isnull[row] {
            let value = fixture.values[row] as i64;
            count += 1;
            least = least.min(value);
            greatest = greatest.max(value);
        }
    }
    let column = fixture.column();
    let rows = fixture.mask();
    let mut status = Status::new();
    let (mut got_count, mut got_min, mut got_max) = (-1, -1, -1);
    let (mut min_null, mut max_null) = (true, true);
    // SAFETY: local buffers of the declared sizes.
    unsafe {
        let column = &raw const column;
        let rows = &raw const rows;
        assert_eq!(
            tess_count(
                column,
                ptr::null(),
                rows,
                &raw mut got_count,
                &raw mut status
            ),
            Code::Ok
        );
        assert_eq!(
            tess_int8_min(
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
            tess_int8_max(
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
    assert_eq!((got_count, got_min, got_max), (count, least, greatest));
    assert_eq!((min_null, max_null), (false, false));
    // The count reads flags alone: the Datums may hold anything, such as
    // pointers of a by-reference type.
    let pointers: Vec<u64> = (0..200)
        .map(|row| 0xdead_beef_0000 + row as u64 * 8)
        .collect();
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: pointers.as_ptr(),
        isnull: fixture.isnull.as_ptr(),
        nrows: 200,
    };
    let rows = fixture.mask();
    // SAFETY: as above.
    let code = unsafe {
        tess_count(
            &raw const column,
            ptr::null(),
            &raw const rows,
            &raw mut got_count,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!(got_count, count);
    // Without selected rows: count 0, the extremes NULL.
    let mut fixture = int8_fixture(200);
    fixture.words.iter_mut().for_each(|word| *word = 0);
    let column = fixture.column();
    let rows = fixture.mask();
    // SAFETY: as above.
    unsafe {
        let column = &raw const column;
        let rows = &raw const rows;
        assert_eq!(
            tess_count(
                column,
                ptr::null(),
                rows,
                &raw mut got_count,
                ptr::null_mut()
            ),
            Code::Ok
        );
        assert_eq!(
            tess_int8_min(
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
            tess_int8_max(
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
    assert_eq!((got_count, got_min, got_max), (0, 0, 0));
    assert_eq!((min_null, max_null), (true, true));
}

#[test]
fn hashes_follow_pg_batch_and_the_null_policy() {
    // Keys 1, NULL, 42 selected; murmurhash32(1) is 0x514e28b7.
    let (datums, isnull, selected) = small(&[1, 7, 42], &[false, true, false]);
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: datums.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 3,
    };
    let mut selection = [selected];
    let rows = Mask {
        nrows: 3,
        bits: selection.as_mut_ptr(),
    };
    let mut hashes = [0xdead_beef_u32; 3];
    let mut valid_words = [0];
    let mut valid = Mask {
        nrows: 3,
        bits: valid_words.as_mut_ptr(),
    };
    let mut status = Status::new();
    for (policy, expected_valid, null_hash) in [(0, 0b101, None), (1, 0b111, Some(0x92ca_2f0e))] {
        // SAFETY: local buffers of the declared sizes.
        let code = unsafe {
            tess_int4_hash(
                &raw const column,
                ptr::null(),
                &raw const rows,
                policy,
                hashes.as_mut_ptr(),
                &raw mut valid,
                &raw mut status,
            )
        };
        assert_eq!(code, Code::Ok, "policy {policy}");
        assert_eq!(valid_words, [expected_valid]);
        assert_eq!(hashes[0], 0x514e_28b7);
        assert_eq!(hashes[2], murmurhash32(42));
        if let Some(null_hash) = null_hash {
            assert_eq!(hashes[1], null_hash);
        }
        // The same column as a second key.
        // SAFETY: as above.
        let code = unsafe {
            tess_int4_hash_next(
                &raw const column,
                ptr::null(),
                policy,
                hashes.as_mut_ptr(),
                &raw mut valid,
                &raw mut status,
            )
        };
        assert_eq!(code, Code::Ok, "policy {policy}");
        assert_eq!(valid_words, [expected_valid]);
        assert_eq!(hashes[0], hash_combine(murmurhash32(1), murmurhash32(1)));
        if let Some(null_hash) = null_hash {
            assert_eq!(hashes[1], hash_combine(null_hash, null_hash));
        }
    }
    // An unknown policy and a null hash buffer are rejected.
    // SAFETY: rejected before any read.
    let code = unsafe {
        tess_int4_hash(
            &raw const column,
            ptr::null(),
            &raw const rows,
            2,
            hashes.as_mut_ptr(),
            &raw mut valid,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
    // SAFETY: as above.
    let code = unsafe {
        tess_int4_hash_next(
            &raw const column,
            ptr::null(),
            0,
            ptr::null_mut(),
            &raw mut valid,
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

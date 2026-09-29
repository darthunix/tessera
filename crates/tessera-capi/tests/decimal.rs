//! The decimal entry points called with raw pointers, as C calls them:
//! numerics in both varlena headers, NaN, a column's decimal side, the
//! scalar argument, and the numerics written back.

use std::ptr;

use tessera_capi::c::{
    Code, DatumColumn, DecimalArg, Mask, Status, tess_decimal_compute, tess_decimal_filter,
    tess_decimal_read, tess_decimal_read_datum, tess_decimal_to_int4, tess_decimal_to_int8,
    tess_decimal_write, tess_decimal_write_datum,
};
use tessera_kernels::decimal::{Decimal, NUMERIC_MAX};

/// Numerics kept at stable addresses, each in words so that a 4-byte
/// header is aligned as PostgreSQL aligns it.
#[expect(
    clippy::vec_box,
    reason = "each numeric keeps its address as the Vec grows"
)]
struct Numerics(Vec<Box<[u64; 5]>>);

impl Numerics {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn add(&mut self, bytes: &[u8]) -> u64 {
        let mut words = Box::new([0_u64; 5]);
        // SAFETY: the words hold 40 bytes, more than any numeric here.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), words.as_mut_ptr().cast(), bytes.len()) };
        let datum = words.as_ptr() as u64;
        self.0.push(words);
        datum
    }

    /// The numeric of a decimal with a 4-byte header.
    fn long(&mut self, value: i64, scale: u32) -> u64 {
        let mut out = [0; NUMERIC_MAX];
        let size = Decimal::new(value, scale).unwrap().write(&mut out);
        self.add(&out[..size])
    }

    /// The same numeric with a 1-byte header, as a table stores it.
    fn short(&mut self, value: i64, scale: u32) -> u64 {
        let mut out = [0; NUMERIC_MAX];
        let size = Decimal::new(value, scale).unwrap().write(&mut out);
        let mut bytes = vec![((size - 3) << 1) as u8 | 1];
        bytes.extend_from_slice(&out[4..size]);
        self.add(&bytes)
    }

    /// NaN, with a 4-byte header.
    fn nan(&mut self) -> u64 {
        let mut bytes = (6_u32 << 2).to_le_bytes().to_vec();
        bytes.extend(0xC000_u16.to_ne_bytes());
        self.add(&bytes)
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

/// The decimal of a numeric Datum by the entry point.
fn read_datum(datum: u64) -> Option<(i64, i32)> {
    let (mut value, mut scale, mut found) = (0, 0, false);
    let mut status = Status::new();
    // SAFETY: a numeric Datum and local outputs.
    let code = unsafe {
        tess_decimal_read_datum(
            datum,
            &raw mut value,
            &raw mut scale,
            &raw mut found,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    found.then_some((value, scale))
}

#[test]
fn filter_compares_decimals_and_leaves_the_rest() {
    let mut numerics = Numerics::new();
    // 1.50, NULL, NaN, -2 (short header), 1.5 (scale 1), 3
    let values = vec![
        numerics.long(150, 2),
        0,
        numerics.nan(),
        numerics.short(-2, 0),
        numerics.short(15, 1),
        numerics.long(3, 0),
    ];
    let isnull = [false, true, false, false, false, false];
    let left = column(&values, &isnull);
    let scalar = numerics.long(15, 1);
    let args = [
        DecimalArg {
            column: &raw const left,
            scalar: 0,
        },
        DecimalArg {
            column: ptr::null(),
            scalar,
        },
    ];
    for (op, kept) in [(0, 0b01_0001), (2, 0b00_1000), (5, 0b11_0001)] {
        let mut rows = [0b11_1111];
        let mut rest = [0];
        let mut status = Status::new();
        // SAFETY: local columns, masks and status.
        let code = unsafe {
            tess_decimal_filter(
                op,
                &raw const args[0],
                &raw const args[1],
                &mut mask(6, &mut rows),
                &mut mask(6, &mut rest),
                &raw mut status,
            )
        };
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!((rows[0], rest[0]), (kept, 0b100), "op {op}");
    }
}

#[test]
fn compute_reads_the_decimal_side_and_writes_decimals_at_the_scale() {
    let mut numerics = Numerics::new();
    // Row 0 and row 2 are decimals of the column's side (scale 2), row 1
    // a numeric, row 3 NaN, row 4 a numeric whose product passes 18 digits.
    let values = vec![
        125,
        numerics.short(-5, 1),
        (-250_i64) as u64,
        numerics.nan(),
        numerics.long(999_999_999_999, 0),
    ];
    let isnull = [false; 5];
    let side = [0b101_u64];
    let left = DatumColumn {
        decimal_rows: side.as_ptr(),
        decimal_scale: 2,
        ..column(&values, &isnull)
    };
    let args = [
        DecimalArg {
            column: &raw const left,
            scalar: 0,
        },
        DecimalArg {
            column: ptr::null(),
            scalar: numerics.long(1_000_000_000, 2),
        },
    ];
    let rows = [0b1_1111];
    let (mut results, mut scales) = ([0_i64; 5], [0_u8; 5]);
    let (mut present, mut decimals, mut rest) = ([0], [0], [0]);
    let mut status = Status::new();
    // SAFETY: local columns, masks, buffers and status.
    let code = unsafe {
        tess_decimal_compute(
            2, // *
            &raw const args[0],
            &raw const args[1],
            &mask(5, &mut rows.clone()),
            4,
            results.as_mut_ptr(),
            scales.as_mut_ptr(),
            &mut mask(5, &mut present),
            &mut mask(5, &mut decimals),
            &mut mask(5, &mut rest),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!(
        (present[0], rest[0], decimals[0]),
        (0b1_1111, 0b1_1000, 0b101)
    );
    // 1.25 * 10000000.00 = 12500000.0000; -0.5 * ... at scale 3.
    assert_eq!((results[0], scales[0]), (125 * 1_000_000_000, 4));
    assert_eq!((results[1], scales[1]), (-5 * 1_000_000_000, 3));
    assert_eq!((results[2], scales[2]), (-250 * 1_000_000_000, 4));

    // The numerics of the rows that are no decimals of scale 4.
    let mut space = [0_u64; 8];
    let mut used = 0;
    let write = [0b10_u64];
    // SAFETY: local buffers aligned to 8 bytes.
    let code = unsafe {
        tess_decimal_write(
            results.as_mut_ptr().cast(),
            scales.as_ptr(),
            -1,
            &mask(5, &mut write.clone()),
            space.as_mut_ptr().cast(),
            size_of_val(&space),
            &raw mut used,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!(results[1] as u64, space.as_ptr() as u64);
    assert_eq!(used, 8);
    assert_eq!(read_datum(results[1] as u64), Some((-5_000_000_000, 3)));
}

#[test]
fn casts_round_half_away_and_leave_the_rest() {
    let mut numerics = Numerics::new();
    let values = vec![
        numerics.short(25, 1),
        numerics.long(-25, 1),
        numerics.long(3_000_000_000, 0),
        numerics.nan(),
        0,
    ];
    let isnull = [false, false, false, false, true];
    let arg_column = column(&values, &isnull);
    let arg = DecimalArg {
        column: &raw const arg_column,
        scalar: 0,
    };
    let rows = [0b1_1111];
    let mut ints = [0_i32; 5];
    let (mut present, mut rest) = ([0], [0]);
    let mut status = Status::new();
    // SAFETY: local column, masks, buffers and status.
    let code = unsafe {
        tess_decimal_to_int4(
            &raw const arg,
            &mask(5, &mut rows.clone()),
            ints.as_mut_ptr(),
            &mut mask(5, &mut present),
            &mut mask(5, &mut rest),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!((present[0], rest[0]), (0b1111, 0b1100));
    assert_eq!(&ints[..2], [3, -3]);
    let mut longs = [0_i64; 5];
    // SAFETY: as above.
    let code = unsafe {
        tess_decimal_to_int8(
            &raw const arg,
            &mask(5, &mut rows.clone()),
            longs.as_mut_ptr(),
            &mut mask(5, &mut present),
            &mut mask(5, &mut rest),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!((present[0], rest[0]), (0b1111, 0b1000));
    assert_eq!(&longs[..3], [3, -3, 3_000_000_000]);
}

#[test]
fn read_takes_a_uniform_scale_and_copies_the_other_datums() {
    let mut numerics = Numerics::new();
    let values = vec![
        numerics.short(12345, 2),
        numerics.long(-7, 2),
        numerics.short(5, 1),
        numerics.nan(),
        0,
        numerics.short(1, 2),
    ];
    let isnull = [false, false, false, false, true, false];
    let source = column(&values, &isnull);
    // Row 5 is not read: its slot and bit stay.
    let rows = [0b01_1111];
    let mut out = [u64::MAX; 6];
    let mut decimals = [0b10_0000];
    let mut scale = -1;
    let mut status = Status::new();
    // SAFETY: local column, masks, buffers and status.
    let code = unsafe {
        tess_decimal_read(
            &raw const source,
            &mask(6, &mut rows.clone()),
            &raw mut scale,
            out.as_mut_ptr(),
            ptr::null_mut(),
            &mut mask(6, &mut decimals),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!((scale, decimals[0]), (2, 0b10_0011));
    assert_eq!(
        out,
        [
            12345,
            (-7_i64) as u64,
            values[2],
            values[3],
            values[4],
            u64::MAX
        ]
    );
    // Every scale.
    let mut scales = [0_u8; 6];
    // SAFETY: as above.
    let code = unsafe {
        tess_decimal_read(
            &raw const source,
            &mask(6, &mut rows.clone()),
            ptr::null_mut(),
            out.as_mut_ptr(),
            scales.as_mut_ptr(),
            &mut mask(6, &mut decimals),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!(decimals[0], 0b10_0111);
    assert_eq!((out[2], &scales[..3]), (5, &[2, 2, 1][..]));
}

#[test]
fn datums_round_trip_and_bad_inputs_fail() {
    let mut numerics = Numerics::new();
    assert_eq!(read_datum(numerics.short(-123_456, 3)), Some((-123_456, 3)));
    assert_eq!(read_datum(numerics.nan()), None);
    let mut out = [0_u64; 4];
    let mut size = 0;
    let mut status = Status::new();
    // SAFETY: a local buffer of 32 bytes.
    let code = unsafe {
        tess_decimal_write_datum(
            -42,
            1,
            out.as_mut_ptr().cast(),
            size_of_val(&out),
            &raw mut size,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!(size, 10);
    assert_eq!(read_datum(out.as_ptr() as u64), Some((-42, 1)));
    for (value, scale, len) in [(i64::MAX, 0, 32), (1, 37, 32), (1, -1, 32), (1, 0, 31)] {
        // SAFETY: as above; the call fails before writing.
        let code = unsafe {
            tess_decimal_write_datum(
                value,
                scale,
                out.as_mut_ptr().cast(),
                len,
                &raw mut size,
                &raw mut status,
            )
        };
        assert_eq!(code, Code::InvalidArgument, "{value} {scale} {len}");
    }
    // A write without room, a misaligned space.
    let mut values = [7_u64, 8];
    let rows = [0b11_u64];
    let mut used = 0;
    let mut space = [0_u64; 2];
    for (at, len) in [(0, 12), (1, 8)] {
        // SAFETY: local buffers; the second is misaligned on purpose.
        let code = unsafe {
            tess_decimal_write(
                values.as_mut_ptr(),
                ptr::null(),
                0,
                &mask(2, &mut rows.clone()),
                space.as_mut_ptr().cast::<u8>().add(at).cast(),
                len,
                &raw mut used,
                &raw mut status,
            )
        };
        assert_eq!(code, Code::InvalidArgument, "{at} {len}");
    }
    // An unknown operation.
    let arg = DecimalArg {
        column: ptr::null(),
        scalar: numerics.long(1, 0),
    };
    let mut rows = [1_u64];
    // SAFETY: local masks and status.
    let code = unsafe {
        tess_decimal_filter(
            9,
            &raw const arg,
            &raw const arg,
            &mut mask(1, &mut rows),
            &mut mask(1, &mut [0]),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
}

#[test]
fn a_decimal_side_without_decimals_may_have_no_scale() {
    let mut numerics = Numerics::new();
    let values = vec![numerics.short(5, 1), numerics.long(-3, 0)];
    let isnull = [false, false];
    let (empty, set) = ([0_u64], [0b1_u64]);
    let rows = [0b11_u64];
    for (side, code) in [(&empty, Code::Ok), (&set, Code::InvalidArgument)] {
        let left = DatumColumn {
            decimal_rows: side.as_ptr(),
            decimal_scale: -1,
            ..column(&values, &isnull)
        };
        let arg = DecimalArg {
            column: &raw const left,
            scalar: 0,
        };
        let (mut out, mut decimals, mut scale) = ([0_u64; 2], [0], -1);
        let mut status = Status::new();
        // SAFETY: local column, masks, buffers and status.
        let result = unsafe {
            tess_decimal_read(
                arg.column,
                &mask(2, &mut rows.clone()),
                &raw mut scale,
                out.as_mut_ptr(),
                ptr::null_mut(),
                &mut mask(2, &mut decimals),
                &raw mut status,
            )
        };
        assert_eq!(result, code, "{}", status.message());
        if code == Code::Ok {
            // The first decimal fixes the scale: -3 is of another.
            assert_eq!((scale, decimals[0], out[0]), (1, 0b01, 5));
        }
    }
}

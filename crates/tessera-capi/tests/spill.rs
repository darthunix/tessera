//! The status of a spilled block read back through the C entry points:
//! damaged bytes report SQLSTATE XX001, a misuse of the call XX000.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use tessera_capi::c::{
    Code, SpillHeader, Status, tess_spill_header_read, tess_spill_header_size,
    tess_spill_header_write, tess_spill_unpack,
};

#[test]
fn damaged_blocks_report_data_corrupted() {
    let header = SpillHeader {
        kind: 1,
        number: 3,
        partition: 2,
        level: 0,
        fingerprint: 0x5eed,
        len: 4096,
        packed: 0,
    };
    let mut bytes = vec![0_u8; tess_spill_header_size()];
    let mut status = Status::new();
    let mut back = SpillHeader { len: 0, ..header };
    // SAFETY: local buffers of the declared sizes throughout this test.
    unsafe {
        let code = tess_spill_header_write(
            bytes.as_mut_ptr(),
            bytes.len(),
            &raw const header,
            1 << 20,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        let read = |bytes: &[u8], status: &mut Status, back: &mut SpillHeader| {
            tess_spill_header_read(bytes.as_ptr(), bytes.len(), 0x5eed, 1 << 20, back, status)
        };
        assert_eq!(read(&bytes, &mut status, &mut back), Code::Ok);
        assert_eq!(back.number, 3);
        // A damaged magic word: damaged data.
        let mut damaged = bytes.clone();
        damaged[0] ^= 1;
        assert_eq!(read(&damaged, &mut status, &mut back), Code::DataCorrupted);
        assert_eq!(status.sqlstate(), "XX001");
        // A buffer shorter than a header: the caller's misuse.
        assert_eq!(
            read(&bytes[..bytes.len() - 1], &mut status, &mut back),
            Code::InvalidArgument
        );
        assert_eq!(status.sqlstate(), "XX000");
        // A packed body too short for its counts: damaged data.
        let mut chunk = vec![0_u8; 64];
        let code = tess_spill_unpack(
            [0_u8; 4].as_ptr(),
            4,
            chunk.as_mut_ptr(),
            chunk.len(),
            &raw mut status,
        );
        assert_eq!(code, Code::DataCorrupted);
        assert_eq!(status.sqlstate(), "XX001");
    }
}

/// The rows of Datum columns written to a chunk of columns through the
/// entry point: a by-value column's Datums, and by-reference values copied
/// as datumGetSize counts them (a varlena of either header, an external
/// pointer of its tag, a C string with its terminator, a fixed length);
/// the call stops at the chunk's capacity and at the values' room, and
/// refuses a varlena it cannot size.
/// The `len` bytes of values at `offset`.
fn at(values: &[u8], offset: u64, len: usize) -> &[u8] {
    &values[offset as usize..offset as usize + len]
}

#[test]
fn rows_of_datums_append_to_a_chunk_of_columns() {
    use tessera_capi::c::{DatumColumn, Mask, tess_spill_columns_append, tess_spill_columns_init};

    let short: Vec<u8> = [&[(6 << 1) | 1][..], b"hello"].concat();
    let long: Vec<u8> = [&(12_u32 << 2).to_le_bytes()[..], b"abcdefgh"].concat();
    let external: Vec<u8> = [&[0x01, 18][..], &[7; 16]].concat();
    let unknown: Vec<u8> = vec![0x01, 9, 0, 0];
    let text = b"tessera\0".to_vec();
    let fixed: Vec<u8> = (0..16).collect();
    let words = [11_u64, 22, 33, 44];
    let varlenas = [
        short.as_ptr() as u64,
        long.as_ptr() as u64,
        0,
        external.as_ptr() as u64,
    ];
    let strings = [text.as_ptr() as u64; 4];
    let fixeds = [fixed.as_ptr() as u64; 4];
    let no_nulls = [false; 4];
    let varlena_nulls = [false, false, true, false];
    let column = |values: &[u64; 4], isnull: &[bool; 4]| DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 4,
        ..DatumColumn::EMPTY
    };
    let columns = [
        column(&words, &no_nulls),
        column(&varlenas, &varlena_nulls),
        column(&strings, &no_nulls),
        column(&fixeds, &no_nulls),
    ];
    let byvals = [true, false, false, false];
    let typlens: [i16; 4] = [8, -1, -2, 16];
    // A chunk of three rows of five words: the columns and a word of the
    // caller's.
    let len = 16 + 8 * 3 * (1 + 5);
    let mut chunk = vec![0_u64; len / 8];
    let mut status = Status::new();
    let lane = |chunk: &[u64], lane: usize, place: usize| chunk[2 + 3 * lane + place];
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        let mut capacity = 0;
        let code = tess_spill_columns_init(
            chunk.as_mut_ptr().cast(),
            len,
            5,
            &raw mut capacity,
            &raw mut status,
        );
        assert_eq!((code, capacity), (Code::Ok, 3));
        let mut word = 0b1111_u64;
        let mut rows = Mask {
            nrows: 4,
            bits: &raw mut word,
        };
        let mut values = vec![0_u8; 256];
        let (mut used, mut appended, mut need) = (0_usize, 0, 0_usize);
        let call = |chunk: &mut Vec<u64>,
                    rows: &mut Mask,
                    values: &mut Vec<u8>,
                    room: usize,
                    used: &mut usize,
                    appended: &mut i32,
                    need: &mut usize,
                    status: &mut Status| {
            tess_spill_columns_append(
                chunk.as_mut_ptr().cast(),
                len,
                4,
                columns.as_ptr(),
                byvals.as_ptr(),
                typlens.as_ptr(),
                rows,
                values.as_mut_ptr(),
                room,
                used,
                appended,
                need,
                status,
            )
        };
        let code = call(
            &mut chunk,
            &mut rows,
            &mut values,
            256,
            &mut used,
            &mut appended,
            &mut need,
            &mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        // Three rows fit the chunk; the fourth's values (its external
        // pointer of 18 bytes, the string's 8, the fixed 16) wait.
        assert_eq!((appended, need, word), (3, 24 + 8 + 16, 0b1000));
        assert_eq!(
            [lane(&chunk, 1, 0), lane(&chunk, 1, 1), lane(&chunk, 1, 2)],
            [11, 22, 33]
        );
        assert_eq!(lane(&chunk, 0, 2), 0b10, "the varlena of row 2 is NULL");
        assert_eq!(at(&values, lane(&chunk, 2, 0), short.len()), &short[..]);
        assert_eq!(at(&values, lane(&chunk, 2, 1), long.len()), &long[..]);
        assert_eq!(lane(&chunk, 2, 2), 0);
        assert_eq!(at(&values, lane(&chunk, 3, 1), text.len()), &text[..]);
        assert_eq!(at(&values, lane(&chunk, 4, 2), 16), &fixed[..]);
        assert!(lane(&chunk, 2, 1) % 8 == 0 && used % 8 == 0);
        // A chunk with room, values without: the row stays, its need told.
        let code = tess_spill_columns_init(
            chunk.as_mut_ptr().cast(),
            len,
            5,
            &raw mut capacity,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok);
        used = 0;
        let code = call(
            &mut chunk,
            &mut rows,
            &mut values,
            40,
            &mut used,
            &mut appended,
            &mut need,
            &mut status,
        );
        assert_eq!(
            (code, appended, need, used, word),
            (Code::Ok, 0, 48, 0, 0b1000)
        );
        let code = call(
            &mut chunk,
            &mut rows,
            &mut values,
            48,
            &mut used,
            &mut appended,
            &mut need,
            &mut status,
        );
        assert_eq!((code, appended, need, used, word), (Code::Ok, 1, 0, 48, 0));
        assert_eq!(
            at(&values, lane(&chunk, 2, 0), external.len()),
            &external[..]
        );
        // An external varlena of an unknown tag has no size.
        let varlenas = [unknown.as_ptr() as u64; 4];
        let columns = [column(&varlenas, &no_nulls)];
        let mut first = 1_u64;
        let mut one = Mask {
            nrows: 4,
            bits: &raw mut first,
        };
        used = 0;
        let code = tess_spill_columns_append(
            chunk.as_mut_ptr().cast(),
            len,
            1,
            columns.as_ptr(),
            [false].as_ptr(),
            [-1_i16].as_ptr(),
            &raw mut one,
            values.as_mut_ptr(),
            256,
            &raw mut used,
            &raw mut appended,
            &raw mut need,
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
    }
}

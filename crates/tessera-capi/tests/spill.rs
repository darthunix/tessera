//! The status of a spilled block read back through the C entry points:
//! damaged bytes report SQLSTATE XX001, a misuse of the call XX000.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use tessera_capi::c::{
    Code, SpillHeader, SpillWeights, Status, tess_spill_header_read, tess_spill_header_size,
    tess_spill_header_write, tess_spill_unpack, tess_table_spill_add_bytes, tess_table_spill_evict,
    tess_table_spill_flags, tess_table_spill_init, tess_table_spill_split, tess_table_spill_splits,
    tess_table_spill_words,
};

/// The rule of a split through its entry point: past two thirds of what
/// the limit leaves, with bits left and fewer than nine tenths of the
/// level's rows; weights below zero refused.
#[test]
fn the_split_entry_point_weighs_room_bits_and_keys() {
    let split = |room: f64, key: f64, size: u64, rows: u64, bits: u32| {
        let mut status = Status::new();
        let mut split = false;
        // SAFETY: a local flag and status.
        let code = unsafe {
            tess_table_spill_splits(
                room,
                key,
                size,
                100,
                1000,
                rows,
                100,
                bits,
                &raw mut split,
                &raw mut status,
            )
        };
        (code, split)
    };
    assert_eq!(split(2.0 / 3.0, 0.9, 601, 89, 3), (Code::Ok, true));
    assert_eq!(
        split(2.0 / 3.0, 0.9, 600, 89, 3),
        (Code::Ok, false),
        "the room"
    );
    assert_eq!(
        split(2.0 / 3.0, 0.9, 601, 90, 3),
        (Code::Ok, false),
        "one key"
    );
    assert_eq!(
        split(2.0 / 3.0, 0.0, 601, 100, 3),
        (Code::Ok, true),
        "no rule of one key"
    );
    assert_eq!(
        split(2.0 / 3.0, 0.9, 601, 89, 31),
        (Code::Ok, false),
        "the bits"
    );
    assert_eq!(split(-1.0, 0.9, 601, 89, 3).0, Code::InvalidArgument);
    assert_eq!(
        split(2.0 / 3.0, 0.9, 601, 89, 33).0,
        Code::InvalidArgument,
        "bits past the hash"
    );
}

/// A process's own words and a shared table's take the same steps through
/// the entry points: the largest partitions in memory past the limit,
/// then those in memory under a quarter of the records, one a check when
/// the weights say so.
#[test]
fn the_spill_entry_points_choose_by_the_weights() {
    let weights = SpillWeights {
        start: 1.0,
        target: 1.0,
        spilled: 0.0,
        reserve: 0,
        resident: 0.25,
        per_check: 0,
    };
    let records = [10_u64, 10, 70, 10];
    for shared in [false, true] {
        let mut status = Status::new();
        let mut nwords = 0;
        let mut in_force = 0;
        let mut over = false;
        // SAFETY: local buffers of the declared sizes throughout this test.
        unsafe {
            let code = tess_table_spill_words(4, &raw mut nwords, &raw mut status);
            assert_eq!(code, Code::Ok, "{}", status.message());
            let mut words = vec![0_u64; nwords];
            let at = words.as_mut_ptr();
            let code = tess_table_spill_init(at, nwords, shared, 100, &raw mut status);
            assert_eq!(code, Code::Ok, "{}", status.message());
            let code =
                tess_table_spill_split(at, nwords, shared, 4, &raw mut in_force, &raw mut status);
            assert_eq!((code, in_force), (Code::Ok, 4), "{}", status.message());
            for (partition, bytes) in [(0, 30), (1, 50), (2, 40)] {
                let code = tess_table_spill_add_bytes(
                    at,
                    nwords,
                    shared,
                    bytes,
                    partition,
                    &raw mut over,
                    &raw mut status,
                );
                assert_eq!(code, Code::Ok, "{}", status.message());
            }
            assert!(over, "{shared}: 120 bytes past a budget of 100");
            let evict = |weights: &SpillWeights, memory, evicted| {
                let mut status = Status::new();
                let mut partition = -2;
                let code = tess_table_spill_evict(
                    at,
                    nwords,
                    shared,
                    weights,
                    memory,
                    100,
                    records.as_ptr(),
                    records.len(),
                    100,
                    evicted,
                    &raw mut partition,
                    &raw mut status,
                );
                assert_eq!(code, Code::Ok, "{}", status.message());
                partition
            };
            let one = SpillWeights {
                per_check: 1,
                ..weights
            };
            assert_eq!(evict(&one, 120, 1), -1, "{shared}: one a check");
            assert_eq!(evict(&weights, 120, 0), 1, "{shared}: the largest");
            assert_eq!(evict(&weights, 120, 1), 2, "{shared}: the next");
            // Partitions 0 and 3 keep 20 of 100 records.
            assert_eq!(evict(&weights, 0, 0), 0, "{shared}: under a quarter");
            assert_eq!(evict(&weights, 0, 0), 3, "{shared}: every one");
            assert_eq!(evict(&weights, 120, 0), -1, "{shared}: none left");
            let negative = SpillWeights {
                start: -1.0,
                ..weights
            };
            let mut partition = 0;
            let code = tess_table_spill_evict(
                at,
                nwords,
                shared,
                &raw const negative,
                0,
                100,
                std::ptr::null(),
                0,
                0,
                0,
                &raw mut partition,
                &raw mut status,
            );
            assert_eq!(code, Code::InvalidArgument, "{shared}: a weight below zero");
            drop(words);
        }
    }
}

/// The flags of a partition without a place for one of them: refused,
/// and the other not written, as every call checks its outputs first.
#[test]
fn the_flags_check_both_outputs_first() {
    let mut status = Status::new();
    let mut nwords = 0;
    let mut in_force = 0;
    let mut on_disk = true;
    // SAFETY: local buffers of the declared sizes throughout this test.
    unsafe {
        let code = tess_table_spill_words(4, &raw mut nwords, &raw mut status);
        assert_eq!(code, Code::Ok, "{}", status.message());
        let mut words = vec![0_u64; nwords];
        let at = words.as_mut_ptr();
        let code = tess_table_spill_init(at, nwords, true, 100, &raw mut status);
        assert_eq!(code, Code::Ok, "{}", status.message());
        let code = tess_table_spill_split(at, nwords, true, 4, &raw mut in_force, &raw mut status);
        assert_eq!(code, Code::Ok, "{}", status.message());
        let code = tess_table_spill_flags(
            at,
            nwords,
            0,
            &raw mut on_disk,
            std::ptr::null_mut(),
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
        assert!(on_disk, "the flag on disk was written");
    }
}

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

/// The `len` bytes of values that the word `reference` names: a
/// reference to chunk 0, which is 1 above 32 bits of the byte.
fn at(values: &[u8], reference: u64, len: usize) -> &[u8] {
    assert_eq!(reference >> 32, 1, "{reference:#x} is no reference");
    let offset = (reference & u64::from(u32::MAX)) as usize;
    &values[offset..offset + len]
}

/// The rows of Datum columns written to a chunk of columns through the
/// entry point: a by-value column's Datums, and by-reference values copied
/// as datumGetSize counts them (a varlena of either header, an external
/// pointer of its tag, a C string with its terminator, a fixed length);
/// the call stops at the chunk's capacity and at the values' room, and
/// refuses a varlena it cannot size.
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

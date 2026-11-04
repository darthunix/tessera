//! The status of a spilled block read back through the C entry points:
//! damaged bytes report SQLSTATE XX001, a misuse of the call XX000.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use tessera_capi::c::{
    Code, SpillHeader, SpillLevel, SpillPlan, SpillShape, SpillWeights, Status,
    tess_spill_expected, tess_spill_header_read, tess_spill_header_size, tess_spill_header_write,
    tess_spill_pack, tess_spill_plan, tess_spill_unpack, tess_table_spill_add_bytes,
    tess_table_spill_evict, tess_table_spill_evictions, tess_table_spill_flags,
    tess_table_spill_init, tess_table_spill_partitions, tess_table_spill_records,
    tess_table_spill_split, tess_table_spill_splits, tess_table_spill_start,
    tess_table_spill_take_alone, tess_table_spill_take_file, tess_table_spill_words,
};
use tessera_kernels::table::{
    CHUNK_HEADER, KEY_SLOT, RECORD_HASH_OFFSET, RECORD_HEADER, RECORD_NEXT_OFFSET,
    RECORD_SIZE_OFFSET,
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

/// Spill words that are null or not aligned to 8 are refused at the C
/// boundary, shared or a process's own, before any word is read; words
/// of another count than the partitions take are refused too, before a
/// slice of that count is made.
#[test]
fn the_spill_words_refuse_null_and_misaligned_words() {
    let mut status = Status::new();
    let mut nwords = 0;
    // SAFETY: a local status and count.
    let code = unsafe { tess_table_spill_words(4, &raw mut nwords, &raw mut status) };
    assert_eq!(code, Code::Ok, "{}", status.message());
    let mut words = vec![0_u64; nwords + 1];
    for shared in [true, false] {
        words.fill(7);
        let at = words.as_mut_ptr();
        let misaligned = at.cast::<u8>().wrapping_add(4).cast::<u64>();
        for (base, count) in [
            (std::ptr::null_mut(), nwords),
            (misaligned, nwords),
            (at, nwords + 1),
            (at, usize::MAX / 8),
        ] {
            // SAFETY: the words of the vector, which the refused calls do
            // not read.
            let code = unsafe { tess_table_spill_init(base, count, shared, 100, &raw mut status) };
            assert_eq!(
                code,
                Code::InvalidArgument,
                "shared {shared}, {count} words"
            );
        }
        assert!(words.iter().all(|&word| word == 7), "a refused call wrote");
        // SAFETY: the words of the vector, as many as the count.
        let code = unsafe { tess_table_spill_init(at, nwords, shared, 100, &raw mut status) };
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(words[nwords], 7, "a word past the count written");
    }
}

/// The partitions in force, the starts of the rounds, the files and the
/// partitions taken once each, and the partitions sent to disk, through
/// their entry points over shared words.
#[test]
fn the_rounds_take_each_file_and_partition_once() {
    let weights = SpillWeights {
        start: 1.0,
        target: 1.0,
        spilled: 0.0,
        reserve: 0,
        resident: 0.0,
        per_check: 1,
    };
    let mut status = Status::new();
    let mut nwords = 0;
    let mut in_force = 0;
    let mut count = 7_u32;
    let mut evictions = 7_u64;
    // SAFETY: local buffers of the declared sizes throughout this test.
    unsafe {
        let code = tess_table_spill_words(4, &raw mut nwords, &raw mut status);
        assert_eq!(code, Code::Ok, "{}", status.message());
        let mut words = vec![0_u64; nwords];
        let at = words.as_mut_ptr();
        let code = tess_table_spill_init(at, nwords, true, 100, &raw mut status);
        assert_eq!(code, Code::Ok, "{}", status.message());
        let code = tess_table_spill_partitions(at, nwords, &raw mut count, &raw mut status);
        assert_eq!((code, count), (Code::Ok, 0), "whole");
        let code = tess_table_spill_split(at, nwords, true, 4, &raw mut in_force, &raw mut status);
        assert_eq!(code, Code::Ok, "{}", status.message());
        let code = tess_table_spill_partitions(at, nwords, &raw mut count, &raw mut status);
        assert_eq!((code, count), (Code::Ok, 4), "split");
        let starts: Vec<u32> = (0..5)
            .map(|_| {
                let mut start = 9;
                let code = tess_table_spill_start(at, nwords, &raw mut start, &raw mut status);
                assert_eq!(code, Code::Ok, "{}", status.message());
                start
            })
            .collect();
        assert_eq!(starts, [0, 1, 2, 3, 0]);
        let take = |partition, outer| {
            let mut status = Status::new();
            let mut file = 9;
            let code = tess_table_spill_take_file(
                at,
                nwords,
                partition,
                outer,
                &raw mut file,
                &raw mut status,
            );
            (code, file)
        };
        assert_eq!(take(1, false), (Code::Ok, 0));
        assert_eq!(take(1, false), (Code::Ok, 1));
        assert_eq!(take(1, true), (Code::Ok, 0), "the outer files apart");
        assert_eq!(take(4, true), (Code::Ok, 0), "the resident rows");
        assert_eq!(take(5, true).0, Code::InvalidArgument);
        let alone = |partition| {
            let mut status = Status::new();
            let mut taken = false;
            let code =
                tess_table_spill_take_alone(at, nwords, partition, &raw mut taken, &raw mut status);
            (code, taken)
        };
        assert_eq!(alone(3), (Code::Ok, true));
        assert_eq!(alone(3), (Code::Ok, false), "taken once");
        assert_eq!(alone(4).0, Code::InvalidArgument);
        let code = tess_table_spill_evictions(at, nwords, &raw mut evictions, &raw mut status);
        assert_eq!((code, evictions), (Code::Ok, 0));
        let mut over = false;
        let code =
            tess_table_spill_add_bytes(at, nwords, true, 120, 2, &raw mut over, &raw mut status);
        assert_eq!((code, over), (Code::Ok, true), "{}", status.message());
        let mut partition = -1;
        let code = tess_table_spill_evict(
            at,
            nwords,
            true,
            &raw const weights,
            0,
            0,
            std::ptr::null(),
            0,
            0,
            0,
            &raw mut partition,
            &raw mut status,
        );
        assert_eq!((code, partition), (Code::Ok, 2), "{}", status.message());
        let code = tess_table_spill_evictions(at, nwords, &raw mut evictions, &raw mut status);
        assert_eq!((code, evictions), (Code::Ok, 1));
        drop(words);
    }
}

/// A chunk of `count` records of `len` bytes, as the hash table lays them
/// out: its used mark, then for each record a hash, a next link, NULL
/// bits and the length in words, and then the words of `word`.
fn records(count: usize, len: usize, word: impl Fn(usize, usize) -> u32) -> Vec<u8> {
    let mut bytes = vec![0_u8; CHUNK_HEADER + count * len];
    let total = bytes.len() as u64;
    bytes[..CHUNK_HEADER].copy_from_slice(&total.to_ne_bytes());
    for record in 0..count {
        let base = CHUNK_HEADER + record * len;
        for lane in 0..len / 4 {
            let value = match lane * 4 {
                RECORD_NEXT_OFFSET => 12345,
                RECORD_SIZE_OFFSET => (len / 8) as u32,
                _ => word(record, lane),
            };
            bytes[base + lane * 4..base + lane * 4 + 4].copy_from_slice(&value.to_ne_bytes());
        }
    }
    bytes
}

/// A chunk of records packed and unpacked through the entry points comes
/// back as it was, its next links cleared; one that packing would not
/// shorten packs into nothing.
#[test]
fn a_chunk_of_records_packs_and_unpacks_through_c() {
    // A record of one key and a payload of 16 bytes: its hash and its key.
    const HASH: usize = RECORD_HASH_OFFSET / 4;
    const KEY: usize = RECORD_HEADER / 4;
    let original = records(
        200,
        RECORD_HEADER + KEY_SLOT + 16,
        |record, lane| match lane {
            HASH => (record as u32).wrapping_mul(2_654_435_761),
            KEY => 1_000 + record as u32,
            _ => 0,
        },
    );
    let mut status = Status::new();
    let mut out = vec![0_u8; original.len()];
    let mut packed = 0;
    let mut back = vec![0_u8; original.len()];
    // SAFETY: local buffers of the declared sizes throughout this test.
    unsafe {
        let code = tess_spill_pack(
            original.as_ptr(),
            original.len(),
            out.as_mut_ptr(),
            out.len(),
            &raw mut packed,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert!(packed > 0 && packed < original.len(), "{packed} bytes");
        let code = tess_spill_unpack(
            out.as_ptr(),
            packed,
            back.as_mut_ptr(),
            back.len(),
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        let mut expected = original.clone();
        for record in 0..200 {
            let at = CHUNK_HEADER + record * 40 + RECORD_NEXT_OFFSET;
            expected[at..at + 4].fill(0);
        }
        assert_eq!(back, expected);
        // One record of the shortest length: no lane narrows, and the
        // packed header would make it longer.
        let one = records(1, RECORD_HEADER + KEY_SLOT, |_, _| u32::MAX);
        let code = tess_spill_pack(
            one.as_ptr(),
            one.len(),
            out.as_mut_ptr(),
            out.len(),
            &raw mut packed,
            &raw mut status,
        );
        assert_eq!((code, packed), (Code::Ok, 0), "{}", status.message());
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

/// The records of a partition past the partitions in force are refused
/// also by a call that adds none and asks for no count.
#[test]
fn the_records_refuse_a_partition_past_the_split() {
    let mut status = Status::new();
    let mut nwords = 0;
    let mut in_force = 0;
    let mut records = 0;
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
        let code = tess_table_spill_records(at, nwords, 3, 7, &raw mut records, &raw mut status);
        assert_eq!((code, records), (Code::Ok, 7), "{}", status.message());
        let code =
            tess_table_spill_records(at, nwords, 3, 0, std::ptr::null_mut(), &raw mut status);
        assert_eq!(code, Code::Ok, "{}", status.message());
        for (added, counted) in [(0, false), (0, true), (1, false)] {
            let into = if counted {
                &raw mut records
            } else {
                std::ptr::null_mut()
            };
            let code = tess_table_spill_records(at, nwords, 4, added, into, &raw mut status);
            assert_eq!(
                code,
                Code::InvalidArgument,
                "{added} added, counted {counted}"
            );
        }
    }
}

/// Weights that are not finite are refused by the rule of eviction and
/// by the rule of a split, as weights below zero are.
#[test]
fn weights_that_are_not_finite_are_refused() {
    let weights = SpillWeights {
        start: 1.0,
        target: 1.0,
        spilled: 0.0,
        reserve: 0,
        resident: 0.0,
        per_check: 0,
    };
    let mut status = Status::new();
    let mut nwords = 0;
    let mut in_force = 0;
    // SAFETY: local buffers of the declared sizes throughout this test.
    unsafe {
        let code = tess_table_spill_words(4, &raw mut nwords, &raw mut status);
        assert_eq!(code, Code::Ok, "{}", status.message());
        let mut words = vec![0_u64; nwords];
        let at = words.as_mut_ptr();
        let code = tess_table_spill_init(at, nwords, false, 100, &raw mut status);
        assert_eq!(code, Code::Ok, "{}", status.message());
        let code = tess_table_spill_split(at, nwords, false, 4, &raw mut in_force, &raw mut status);
        assert_eq!(code, Code::Ok, "{}", status.message());
        let wrong = [
            SpillWeights {
                start: f64::NAN,
                ..weights
            },
            SpillWeights {
                target: f64::INFINITY,
                ..weights
            },
            SpillWeights {
                spilled: f64::NEG_INFINITY,
                ..weights
            },
            SpillWeights {
                resident: f64::NAN,
                ..weights
            },
        ];
        for wrong in &wrong {
            let mut partition = 7;
            let code = tess_table_spill_evict(
                at,
                nwords,
                false,
                wrong,
                200,
                100,
                std::ptr::null(),
                0,
                0,
                0,
                &raw mut partition,
                &raw mut status,
            );
            assert_eq!((code, partition), (Code::InvalidArgument, 7), "{wrong:?}");
        }
        for (room, key) in [(f64::NAN, 0.9), (2.0 / 3.0, f64::INFINITY)] {
            let mut split = false;
            let code = tess_table_spill_splits(
                room,
                key,
                601,
                100,
                1000,
                89,
                100,
                3,
                &raw mut split,
                &raw mut status,
            );
            assert_eq!(code, Code::InvalidArgument, "{room} and {key}");
        }
    }
}

/// The plan of a level through its entry point: a level of 4 to 1024
/// partitions that expects 10 MiB in 4 MiB gets 8 partitions of chunks of
/// 32 KiB, counted by 50 words, the first 3 bits, and room for a level
/// below; one that wants 16 partitions at least, of records of 4 KiB,
/// gets 16 of chunks of four records past the most. What no node
/// gives is refused, SQLSTATE XX000, and nothing is written then: a shift
/// past which the least partitions do not fit in the hash, expected bytes
/// below zero or not a number, a least chunk not a multiple of 8, given
/// partitions no power of two, a null shape, level or plan.
#[test]
fn the_plan_of_a_level_refuses_what_no_node_gives() {
    let shape = SpillShape {
        reserve: 48 << 10,
        share: 16,
        min_chunk: 8 << 10,
        max_chunk: 1 << 20,
        record: 40,
        min_partitions: 4,
        max_partitions: 1024,
    };
    let level = SpillLevel {
        expected: 10e6,
        limit: 4 << 20,
        shift: 0,
        at_least: 0,
        partitions: 0,
    };
    let call = |shape: *const SpillShape, level: *const SpillLevel, out: *mut SpillPlan| {
        let mut status = Status::new();
        // SAFETY: a local status; the others null or locals.
        let code = unsafe { tess_spill_plan(shape, level, out, &raw mut status) };
        (code, status.sqlstate().to_owned())
    };
    let mut plan = SpillPlan::default();
    assert_eq!(
        call(&raw const shape, &raw const level, &raw mut plan).0,
        Code::Ok
    );
    assert_eq!(
        (plan.partitions, plan.chunk_len, plan.words),
        (8, 32 << 10, 50)
    );
    assert_eq!((plan.shift, plan.below, plan.deeper), (0, 3, true));
    // Sixteen at least, of chunks of four records of 4 KiB past the most.
    let wide = SpillShape {
        reserve: 0,
        min_chunk: 8,
        max_chunk: 1024,
        record: 4096,
        ..shape
    };
    let wanted = SpillLevel {
        expected: 0.0,
        at_least: 16,
        ..level
    };
    assert_eq!(
        call(&raw const wide, &raw const wanted, &raw mut plan).0,
        Code::Ok
    );
    assert_eq!(
        (plan.partitions, plan.chunk_len, plan.words),
        (16, 16392, 90)
    );
    let given = SpillLevel {
        shift: 3,
        partitions: 16,
        ..level
    };
    assert_eq!(
        call(&raw const shape, &raw const given, &raw mut plan).0,
        Code::Ok
    );
    assert_eq!((plan.partitions, plan.shift, plan.below), (16, 3, 7));
    // After 28 bits, 8 partitions take three, and no level fits below.
    let last = SpillLevel { shift: 28, ..level };
    assert_eq!(
        call(&raw const shape, &raw const last, &raw mut plan).0,
        Code::Ok
    );
    assert_eq!((plan.partitions, plan.below, plan.deeper), (8, 31, false));

    let untouched = SpillPlan {
        chunk_len: 7,
        ..SpillPlan::default()
    };
    let refused = [
        (shape, SpillLevel { shift: 31, ..level }),
        (
            shape,
            SpillLevel {
                expected: -1.0,
                ..level
            },
        ),
        (
            shape,
            SpillLevel {
                expected: f64::NAN,
                ..level
            },
        ),
        (
            shape,
            SpillLevel {
                partitions: 48,
                ..level
            },
        ),
        (
            SpillShape {
                min_chunk: 4100,
                ..shape
            },
            level,
        ),
    ];
    for (shape, level) in refused {
        let mut plan = untouched;
        let (code, sqlstate) = call(&raw const shape, &raw const level, &raw mut plan);
        assert_eq!((code, sqlstate.as_str()), (Code::InvalidArgument, "XX000"));
        assert_eq!(plan, untouched, "nothing written");
    }
    let nowhere = std::ptr::null_mut();
    assert_eq!(
        call(std::ptr::null(), &raw const level, &raw mut plan).0,
        Code::InvalidArgument
    );
    assert_eq!(
        call(&raw const shape, std::ptr::null(), &raw mut plan).0,
        Code::InvalidArgument
    );
    assert_eq!(
        call(&raw const shape, &raw const level, nowhere).0,
        Code::InvalidArgument
    );

    // Twice 1000 bytes, or 30 rows at 100 bytes a row, for 2 participants.
    assert_eq!(tess_spill_expected(1000.0, 10.0, 30.0, 2), 6000.0);
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

/// A chunk of columns written through its entry points reads back
/// through them: its pair of blocks checked by kind and number, the chunk
/// checked once against its values, and windows of Datums, a varlena as a
/// pointer to its bytes in the values. Another count of words or rows, or
/// a word that refers past the values, is damaged data; a window past the
/// rows or a block past the pair a wrong call.
#[test]
fn a_chunk_of_columns_reads_back_through_its_entry_points() {
    use tessera_capi::c::{
        DatumColumn, Mask, tess_spill_columns_append, tess_spill_columns_check,
        tess_spill_columns_init, tess_spill_columns_pair, tess_spill_columns_window,
    };

    // Short varlenas: a header byte of the length, 1 + row, and row bytes.
    let texts: Vec<Vec<u8>> = (0..5_u8)
        .map(|row| [vec![((1 + row) << 1) | 1], vec![b'a' + row; row.into()]].concat())
        .collect();
    let words: Vec<u64> = (0..5).map(|row| 100 + row).collect();
    let varlenas: Vec<u64> = texts.iter().map(|text| text.as_ptr() as u64).collect();
    let word_nulls = [false, true, false, false, false];
    let text_nulls = [false, false, false, true, false];
    let column = |values: &[u64], isnull: &[bool]| DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 5,
        ..DatumColumn::EMPTY
    };
    let columns = [column(&words, &word_nulls), column(&varlenas, &text_nulls)];
    let byvals = [true, false];
    let typlens: [i16; 2] = [8, -1];
    let len = 16 + 8 * 5 * (1 + 2);
    let mut chunk = vec![0_u64; len / 8];
    let mut values = vec![0_u8; 128];
    let mut status = Status::new();
    let (mut used, mut appended, mut need) = (0_usize, 0, 0_usize);
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        let mut capacity = 0;
        let code = tess_spill_columns_init(
            chunk.as_mut_ptr().cast(),
            len,
            2,
            &raw mut capacity,
            &raw mut status,
        );
        assert_eq!((code, capacity), (Code::Ok, 5));
        let mut word = 0b1_1111_u64;
        let mut rows = Mask {
            nrows: 5,
            bits: &raw mut word,
        };
        let code = tess_spill_columns_append(
            chunk.as_mut_ptr().cast(),
            len,
            2,
            columns.as_ptr(),
            byvals.as_ptr(),
            typlens.as_ptr(),
            &raw mut rows,
            values.as_mut_ptr(),
            values.len(),
            &raw mut used,
            &raw mut appended,
            &raw mut need,
            &raw mut status,
        );
        assert_eq!((code, appended), (Code::Ok, 5), "{}", status.message());
    }
    let check = |chunk: &[u64], words: i32, rows: i64, values_len: usize| {
        let mut status = Status::new();
        // SAFETY: a local chunk of `len` bytes, two flags and lengths, and
        // the values, of `values_len` bytes at most.
        let code = unsafe {
            tess_spill_columns_check(
                chunk.as_ptr().cast(),
                len,
                words,
                rows,
                2,
                byvals.as_ptr(),
                typlens.as_ptr(),
                values.as_ptr(),
                values_len.min(values.len()),
                &raw mut status,
            )
        };
        (code, status.sqlstate().to_owned())
    };
    // A window of the first `ncolumns` columns, by the flags of the chunk's.
    let window =
        |ncolumns: i32, first: usize, out: &mut [[u64; 4]; 3], isnull: &mut [[bool; 4]; 3]| {
            let mut status = Status::new();
            let outs: Vec<*mut u64> = out.iter_mut().map(|lane| lane.as_mut_ptr()).collect();
            let nulls: Vec<*mut bool> = isnull.iter_mut().map(|lane| lane.as_mut_ptr()).collect();
            let flags = [true, false, true];
            // SAFETY: the chunk and values above, `ncolumns` windows of 4
            // entries each.
            unsafe {
                tess_spill_columns_window(
                    chunk.as_ptr().cast(),
                    len,
                    ncolumns,
                    flags.as_ptr(),
                    values.as_ptr(),
                    first,
                    4,
                    outs.as_ptr(),
                    nulls.as_ptr(),
                    &raw mut status,
                )
            }
        };
    assert_eq!(check(&chunk, 2, 5, used).0, Code::Ok);
    assert_eq!(check(&chunk, 2, -1, used).0, Code::Ok);
    let (mut outs, mut nulls) = ([[0_u64; 4]; 3], [[true; 4]; 3]);
    assert_eq!(window(2, 1, &mut outs, &mut nulls), Code::Ok);
    assert_eq!(outs[0], [0, 102, 103, 104]);
    assert_eq!(nulls[0], [true, false, false, false]);
    let (out, isnull) = (outs[1], nulls[1]);
    assert_eq!(isnull, [false, false, true, false]);
    assert_eq!(out[2], 0, "a NULL is no pointer");
    for (at, row) in [(0, 1), (1, 2), (3, 4)] {
        let base = values.as_ptr() as u64;
        assert!(out[at] >= base && out[at] < base + used as u64, "row {row}");
        let start = (out[at] - base) as usize;
        let text = &texts[row];
        assert_eq!(&values[start..start + text.len()], &text[..], "row {row}");
    }
    let wrong = (Code::InvalidArgument, "XX000".to_owned());
    let damaged = (Code::DataCorrupted, "XX001".to_owned());
    assert_eq!(check(&chunk, 3, 5, used), damaged, "words");
    assert_eq!(check(&chunk, 2, 4, used), damaged, "rows");
    assert_eq!(check(&chunk, 2, 5, 8), damaged, "values cut short");
    assert_eq!(check(&chunk, 2, -2, used), wrong, "rows below -1");
    assert_eq!(
        window(2, 2, &mut outs, &mut nulls),
        Code::InvalidArgument,
        "past the rows"
    );
    assert_eq!(
        window(3, 0, &mut outs, &mut nulls),
        Code::InvalidArgument,
        "past the words"
    );
    assert_eq!(
        window(0, 2, &mut outs, &mut nulls),
        Code::InvalidArgument,
        "no columns, past the rows"
    );
    assert_eq!(window(0, 1, &mut outs, &mut nulls), Code::Ok, "no columns");

    let pair = |kind: u32, at: i32, number: u32| {
        let header = SpillHeader {
            kind,
            number: 2,
            len: 8,
            ..SpillHeader::default()
        };
        let mut status = Status::new();
        // SAFETY: a local header and status.
        let code =
            unsafe { tess_spill_columns_pair(&raw const header, at, number, &raw mut status) };
        (code, status.sqlstate().to_owned())
    };
    assert_eq!(pair(2, 0, 2).0, Code::Ok);
    assert_eq!(pair(3, 1, 2).0, Code::Ok);
    assert_eq!(pair(3, 0, 2), damaged, "the rows first");
    assert_eq!(pair(2, 0, 3), damaged, "another number");
    assert_eq!(pair(2, 2, 2), wrong, "past the pair");
    let mut status = Status::new();
    // SAFETY: a null header, refused.
    let code = unsafe { tess_spill_columns_pair(std::ptr::null(), 0, 2, &raw mut status) };
    assert_eq!(code, Code::InvalidArgument);
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
        // An external varlena of an unknown tag has no size, and an
        // expanded object's pointer (read-only and read-write) is not
        // copied: it means nothing outside its process. A non-NULL Datum
        // of 0 points nowhere, and a type of length -3 has no rule of size.
        let expanded_ro: Vec<u8> = [&[0x01, 2][..], &[7; 8]].concat();
        let expanded_rw: Vec<u8> = [&[0x01, 3][..], &[7; 8]].concat();
        let expanded = "an expanded object, not flattened";
        let cases = [
            (unknown.as_ptr() as u64, -1, "an external varlena of tag 9"),
            (expanded_ro.as_ptr() as u64, -1, expanded),
            (expanded_rw.as_ptr() as u64, -1, expanded),
            (0, -1, "a null by-reference value"),
            (short.as_ptr() as u64, -3, "a type of length -3"),
        ];
        // A plain value comes first in the row, so nothing of it may stay
        // either.
        for (datum, typlen, reason) in cases {
            let plain = [short.as_ptr() as u64; 4];
            let varlenas = [datum; 4];
            let columns = [column(&plain, &no_nulls), column(&varlenas, &no_nulls)];
            let mut first = 1_u64;
            let mut one = Mask {
                nrows: 4,
                bits: &raw mut first,
            };
            used = 0;
            let code = tess_spill_columns_append(
                chunk.as_mut_ptr().cast(),
                len,
                2,
                columns.as_ptr(),
                [false, false].as_ptr(),
                [-1_i16, typlen].as_ptr(),
                &raw mut one,
                values.as_mut_ptr(),
                256,
                &raw mut used,
                &raw mut appended,
                &raw mut need,
                &raw mut status,
            );
            assert_eq!(code, Code::InvalidArgument, "{reason}");
            assert_eq!(used, 0, "{reason}");
            // The host's own reason, not the spill's rule that met it.
            assert!(status.message().contains(reason), "{}", status.message());
        }
    }
}

/// A value refused at a row past the largest number an error of the
/// spill keeps, 2^23 - 1, reports its own reason: the row before it,
/// NULL with a Datum that points nowhere, is never read.
#[test]
#[cfg_attr(miri, ignore = "eight million rows")]
fn a_refused_value_past_two_to_the_23_rows_reports_its_own_reason() {
    use tessera_capi::c::{DatumColumn, Mask, tess_spill_columns_append, tess_spill_columns_init};

    let nrows = (1_usize << 23) + 1;
    let expanded: Vec<u8> = [&[0x01, 2][..], &[7; 8]].concat();
    let mut datums = vec![0_u64; nrows];
    let mut nulls = vec![false; nrows];
    datums[nrows - 1] = expanded.as_ptr() as u64;
    datums[nrows - 2] = 1;
    nulls[nrows - 2] = true;
    let columns = [DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: datums.as_ptr(),
        isnull: nulls.as_ptr(),
        nrows: nrows as i32,
        ..DatumColumn::EMPTY
    }];
    let mut bits = vec![0_u64; nrows.div_ceil(64)];
    bits[(nrows - 1) / 64] = 1 << ((nrows - 1) % 64);
    let mut rows = Mask {
        nrows: nrows as i32,
        bits: bits.as_mut_ptr(),
    };
    let len = 16 + 8 * 2 * 4;
    let mut chunk = vec![0_u64; len / 8];
    let mut values = vec![0_u8; 256];
    let (mut used, mut appended, mut need) = (0_usize, 0, 0_usize);
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing
    // else, throughout this test.
    unsafe {
        let mut capacity = 0;
        let code = tess_spill_columns_init(
            chunk.as_mut_ptr().cast(),
            len,
            1,
            &raw mut capacity,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        let code = tess_spill_columns_append(
            chunk.as_mut_ptr().cast(),
            len,
            1,
            columns.as_ptr(),
            [false].as_ptr(),
            [-1_i16].as_ptr(),
            &raw mut rows,
            values.as_mut_ptr(),
            values.len(),
            &raw mut used,
            &raw mut appended,
            &raw mut need,
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
    }
    assert!(
        status
            .message()
            .contains("an expanded object, not flattened"),
        "{}",
        status.message()
    );
}

/// A file of a set kept in memory, written through the set's entry
/// points: the regions the writer asks for grow as C grows them.
struct CSet {
    writer: relspill::set::Writer,
    buffer: Vec<u8>,
    scratch: Vec<u8>,
    entries: Vec<Vec<relspill::set::Entry>>,
    lists: Vec<tessera_capi::c::SpillList>,
    file: Vec<u8>,
}

impl CSet {
    fn new(partitions: usize, buffer_len: usize) -> Self {
        Self::bounded(partitions, buffer_len, 1 << 20, 1 << 20)
    }

    /// A set whose chunks take at most `max_chunk` body bytes and chunks
    /// of values `max_values`.
    fn bounded(partitions: usize, buffer_len: usize, max_chunk: u64, max_values: u64) -> Self {
        use tessera_capi::c::tess_spill_set_init;

        let mut set = Self {
            writer: relspill::set::Writer::default(),
            buffer: vec![0; buffer_len],
            scratch: Vec::new(),
            entries: vec![Vec::new(); partitions],
            lists: (0..partitions)
                .map(|_| tessera_capi::c::SpillList {
                    entries: std::ptr::null_mut(),
                    count: 0,
                    slots: 0,
                })
                .collect(),
            file: Vec::new(),
        };
        let mut status = Status::new();
        // SAFETY: a local writer and status.
        let code = unsafe {
            tess_spill_set_init(
                &raw mut set.writer,
                partitions as i32,
                1,
                77,
                max_chunk,
                max_values,
                buffer_len,
                true,
                &raw mut status,
            )
        };
        assert_eq!(code, Code::Ok, "{}", status.message());
        set
    }

    /// Serve a step: grow a region, or write its ranges and report them;
    /// whether the call is done.
    fn serve(&mut self, step: &tessera_capi::c::SpillStep, partition: usize, body: &[u8]) -> bool {
        use tessera_capi::c::{
            REGION_BODY, REGION_BUFFER, REGION_LIST, REGION_SCRATCH, STEP_DONE, STEP_LIST,
            STEP_SCRATCH, tess_spill_set_written,
        };

        match step.kind {
            STEP_SCRATCH => {
                self.scratch.resize(step.need as usize, 0);
                return false;
            }
            STEP_LIST => {
                self.entries[partition].resize(step.need as usize, relspill::set::Entry::default());
                let list = &mut self.lists[partition];
                list.entries = self.entries[partition].as_mut_ptr();
                list.slots = step.need;
                return false;
            }
            _ => {}
        }
        for range in &step.ranges[..step.nranges as usize] {
            let list;
            let from: &[u8] = match range.region {
                REGION_BUFFER => &self.buffer,
                REGION_SCRATCH => &self.scratch,
                REGION_BODY => body,
                REGION_LIST => {
                    list = self.entries[range.partition as usize]
                        .iter()
                        .flat_map(|entry| [entry.offset, entry.stored])
                        .flat_map(u64::to_ne_bytes)
                        .collect::<Vec<u8>>();
                    &list
                }
                other => panic!("a range of region {other}"),
            };
            let bytes = &from[range.start as usize..(range.start + range.len) as usize];
            let at = range.at as usize;
            if self.file.len() < at + bytes.len() {
                self.file.resize(at + bytes.len(), 0);
            }
            self.file[at..at + bytes.len()].copy_from_slice(bytes);
        }
        if step.nranges > 0 {
            let mut status = Status::new();
            // SAFETY: the set's writer and a local status.
            let code = unsafe { tess_spill_set_written(&raw mut self.writer, &raw mut status) };
            assert_eq!(code, Code::Ok, "{}", status.message());
        }
        step.kind == STEP_DONE
    }

    /// One call of `tess_spill_set_write`, its code, status and step.
    fn call(
        &mut self,
        nlists: i32,
        partition: i32,
        kind: u32,
        body: &[u8],
    ) -> (Code, Status, tessera_capi::c::SpillStep) {
        use tessera_capi::c::{SpillStep, tess_spill_set_write};

        let mut status = Status::new();
        let mut step = SpillStep::default();
        // SAFETY: the set's regions and lists, each list's entries the
        // vector's, and a local body, step and status.
        let code = unsafe {
            tess_spill_set_write(
                &raw mut self.writer,
                self.buffer.as_mut_ptr(),
                self.buffer.len(),
                self.scratch.as_mut_ptr(),
                self.scratch.len(),
                self.lists.as_mut_ptr(),
                nlists,
                partition,
                kind,
                3,
                body.as_ptr(),
                body.len(),
                &raw mut step,
                &raw mut status,
            )
        };
        (code, status, step)
    }

    /// Write a block of `kind` to `partition`: where it starts.
    fn write(&mut self, partition: usize, kind: u32, body: &[u8]) -> u64 {
        loop {
            let nlists = self.lists.len() as i32;
            let (code, status, step) = self.call(nlists, partition as i32, kind, body);
            assert_eq!(code, Code::Ok, "{}", status.message());
            if self.serve(&step, partition, body) {
                return step.offset;
            }
        }
    }

    fn finish(&mut self) {
        use tessera_capi::c::{SpillStep, tess_spill_set_finish};

        loop {
            let mut status = Status::new();
            let mut step = SpillStep::default();
            // SAFETY: as for `call`.
            let code = unsafe {
                tess_spill_set_finish(
                    &raw mut self.writer,
                    self.buffer.as_mut_ptr(),
                    self.buffer.len(),
                    self.lists.as_mut_ptr(),
                    self.lists.len() as i32,
                    &raw mut step,
                    &raw mut status,
                )
            };
            assert_eq!(code, Code::Ok, "{}", status.message());
            if self.serve(&step, 0, &[]) {
                return;
            }
        }
    }

    /// The blocks and bytes written, the partitions with blocks, and the
    /// longest values written.
    fn stats(&self) -> (u64, u64, i32, u64) {
        use tessera_capi::c::tess_spill_set_stats;

        let (mut blocks, mut bytes, mut files, mut values) = (0, 0, 0, 0);
        let mut status = Status::new();
        // SAFETY: the set's writer and local outputs.
        let code = unsafe {
            tess_spill_set_stats(
                &raw const self.writer,
                &raw mut blocks,
                &raw mut bytes,
                &raw mut files,
                &raw mut values,
                &raw mut status,
            )
        };
        assert_eq!(code, Code::Ok, "{}", status.message());
        (blocks, bytes, files, values)
    }
}

/// A shared set written through its entry points: values, a chunk of
/// records longer than the buffer packed aside, a list that grows, and a
/// finish that lays the lists and the trailer out after the blocks; each
/// block's header reads back where its list says, and a dropped
/// partition no longer counts.
#[test]
fn a_set_writes_through_its_entry_points() {
    use tessera_capi::c::tess_spill_set_drop;

    let mut set = CSet::new(3, 256);
    let values = vec![9_u8; 16];
    for _ in 0..20 {
        set.write(1, 2, &values);
    }
    let chunk = records(1000, 40, |record, lane| (record * lane) as u32);
    let at = set.write(2, 1, &chunk);
    set.write(0, 2, &values);
    assert!(set.scratch.len() >= chunk.len(), "packed aside");
    set.finish();
    let (blocks, bytes, files, _) = set.stats();
    assert_eq!((blocks, files), (22, 3));
    let word = |at: usize| u64::from_ne_bytes(set.file[at..at + 8].try_into().unwrap());
    let trailer = set.file.len() - 32;
    assert_eq!(
        [
            word(trailer),
            word(trailer + 8),
            word(trailer + 16),
            word(trailer + 24)
        ],
        [0x5445_5353_5452_4149, bytes, 3, 77]
    );
    for (partition, list) in set.lists.iter().enumerate() {
        assert_eq!(word(bytes as usize + 8 * partition), list.count);
        for entry in &set.entries[partition][..list.count as usize] {
            let block = &set.file[entry.offset as usize..(entry.offset + entry.stored) as usize];
            let mut header = SpillHeader::default();
            let mut status = Status::new();
            // SAFETY: a block of the file and local outputs.
            let code = unsafe {
                tess_spill_header_read(
                    block.as_ptr().cast(),
                    block.len(),
                    77,
                    1 << 20,
                    &raw mut header,
                    &raw mut status,
                )
            };
            assert_eq!(code, Code::Ok, "{}", status.message());
            assert_eq!(header.partition as usize, partition);
            if partition == 2 {
                assert_eq!((entry.offset, header.len), (at, chunk.len() as u64));
                assert!(header.packed > 0, "the chunk of records packs");
            }
        }
    }
    let mut status = Status::new();
    // SAFETY: the set's writer and lists, and a local status.
    let code = unsafe {
        tess_spill_set_drop(
            &raw mut set.writer,
            set.lists.as_mut_ptr(),
            3,
            1,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!((set.lists[1].count, set.stats().2), (0, 2));
}

/// Each misuse of a set's entry points is refused with its reason: lists
/// of another count, an unknown kind, a null writer, a block after the
/// finish, and ranges reported where none wait.
#[test]
fn the_set_entry_points_refuse_what_no_set_takes() {
    use tessera_capi::c::{tess_spill_set_layout, tess_spill_set_written};

    let mut set = CSet::new(2, 256);
    let values = vec![1_u8; 8];
    let (code, status, _) = set.call(3, 0, 2, &values);
    assert_eq!(code, Code::InvalidArgument);
    assert!(status.message().contains("3 block lists for 2 partitions"));
    let (code, status, _) = set.call(2, 0, 9, &values);
    assert_eq!(code, Code::InvalidArgument);
    assert!(status.message().contains("unknown kind 9"));
    let (code, status, _) = set.call(2, 2, 2, &values);
    assert_eq!(code, Code::InvalidArgument);
    assert!(status.message().contains("partition 2 is past the 2"));
    let mut status = Status::new();
    // SAFETY: a null writer, refused, and a local status.
    let code = unsafe { tess_spill_set_written(std::ptr::null_mut(), &raw mut status) };
    assert_eq!(code, Code::InvalidArgument);
    assert!(status.message().contains("a null writer"));
    let mut status = Status::new();
    // SAFETY: the set's writer and a local status.
    let code = unsafe { tess_spill_set_written(&raw mut set.writer, &raw mut status) };
    assert_eq!(code, Code::InvalidArgument);
    assert!(status.message().contains("no ranges"));
    set.write(0, 2, &values);
    set.finish();
    let (code, status, _) = set.call(2, 0, 2, &values);
    assert_eq!(code, Code::InvalidArgument);
    assert!(status.message().contains("finished takes no more blocks"));
    assert_eq!(
        (0..17)
            .map(|what| tess_spill_set_layout(what))
            .collect::<Vec<_>>(),
        [
            168, 16, 24, 8, 32, 128, 32, 224, 72, 32, 48, 44, 32, 24, 32, 28, 0
        ]
    );
}

/// One step of a reader through its entry points: read what it asks
/// into the page or the block buffer from `file`, or give what it found.
fn read_step(
    step: &tessera_capi::c::SpillReadStep,
    file: &[u8],
    page: &mut [relspill::set::Entry],
    block: &mut [u8],
) -> bool {
    use tessera_capi::c::{READ_BLOCK, READ_PAGE};

    let bytes = &file[step.at as usize..(step.at + step.len) as usize];
    match step.kind {
        READ_PAGE => {
            for (entry, pair) in page.iter_mut().zip(bytes.chunks(16)) {
                let word = |at: usize| {
                    pair.get(at..at + 8)
                        .map_or(0, |word| u64::from_ne_bytes(word.try_into().unwrap()))
                };
                *entry = relspill::set::Entry {
                    offset: word(0),
                    stored: word(8),
                };
            }
            true
        }
        READ_BLOCK => {
            block[..bytes.len()].copy_from_slice(bytes);
            true
        }
        _ => false,
    }
}

/// A set's partitions read back through the reader's entry points: the
/// own list, and the file as another participant reads it with a page of
/// two entries; a seek to a block by its place; each block's header and
/// body as written.
#[test]
fn a_set_reads_back_through_its_entry_points() {
    use relspill::set::{Entry, Reader};
    use tessera_capi::c::{
        READ_END, READ_FOUND, READ_HEADER, READ_READY, SpillReadStep, tess_spill_reader_body,
        tess_spill_reader_file, tess_spill_reader_next, tess_spill_reader_open,
        tess_spill_reader_own, tess_spill_reader_seek,
    };

    let mut set = CSet::new(2, 256);
    let mut places = Vec::new();
    for len in [16_usize, 400, 24] {
        let body = vec![len as u8; len];
        places.push(set.write(1, 2, &body));
    }
    // A chunk of records, which goes packed, in partition 0.
    let chunk = records(64, 40, |record, lane| (record * 7 + lane) as u32);
    set.write(0, 1, &chunk);
    set.finish();
    let list = &set.entries[1][..set.lists[1].count as usize];

    // Each block of a reader, by its entry points, then a seek to the
    // second and its block again.
    let read_all = |reader: &mut Reader, page: &mut Vec<Entry>, block: &mut Vec<u8>| {
        let mut numbers = Vec::new();
        loop {
            let mut step = SpillReadStep::default();
            let mut status = Status::new();
            // SAFETY: the reader, page and block of this test.
            let code = unsafe {
                tess_spill_reader_next(
                    reader,
                    page.as_ptr(),
                    page.len(),
                    block.as_ptr(),
                    block.len(),
                    &raw mut step,
                    &raw mut status,
                )
            };
            assert_eq!(code, Code::Ok, "{}", status.message());
            if read_step(&step, &set.file, page, block) {
                continue;
            }
            if step.kind == READ_END {
                return numbers;
            }
            assert_eq!(step.kind, READ_HEADER);
            let mut body = vec![0_u8; step.header.len as usize];
            // SAFETY: as above, and a local body.
            let code = unsafe {
                tess_spill_reader_body(
                    reader,
                    block.as_ptr(),
                    block.len(),
                    body.as_mut_ptr(),
                    body.len(),
                    &raw mut status,
                )
            };
            assert_eq!(code, Code::Ok, "{}", status.message());
            assert!(body.iter().all(|&byte| byte == body.len() as u8));
            numbers.push(body.len());
        }
    };
    let seek = |reader: &mut Reader, page: &mut Vec<Entry>, block: &mut Vec<u8>, at: u64| loop {
        let mut step = SpillReadStep::default();
        let mut status = Status::new();
        // SAFETY: the reader and page of this test.
        let code = unsafe {
            tess_spill_reader_seek(
                reader,
                page.as_ptr(),
                page.len(),
                at,
                &raw mut step,
                &raw mut status,
            )
        };
        assert_eq!(code, Code::Ok, "{}", status.message());
        if step.kind == READ_FOUND {
            return;
        }
        assert!(read_step(&step, &set.file, page, block));
    };

    // The readers take the longest values written as their bound.
    let values = set.stats().3;
    assert_eq!(values, 400);
    let mut own = Reader::default();
    let mut largest = 0;
    let mut status = Status::new();
    // SAFETY: a local reader, the set's list, a local output.
    let code = unsafe {
        tess_spill_reader_own(
            &raw mut own,
            2,
            1,
            1,
            77,
            1 << 20,
            values,
            list.as_ptr(),
            list.len() as u64,
            &raw mut largest,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    let mut page = list.to_vec();
    let mut block = vec![0_u8; largest as usize];
    assert_eq!(read_all(&mut own, &mut page, &mut block), [16, 400, 24]);
    seek(&mut own, &mut page, &mut block, places[1]);
    assert_eq!(read_all(&mut own, &mut page, &mut block), [400, 24]);

    let mut other = Reader::default();
    // SAFETY: a local reader.
    let code = unsafe {
        tess_spill_reader_file(
            &raw mut other,
            2,
            1,
            1,
            77,
            1 << 20,
            values,
            set.file.len() as u64,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    let mut page = vec![Entry::default(); 2];
    let mut block = Vec::new();
    loop {
        let mut step = SpillReadStep::default();
        // SAFETY: the reader and page of this test.
        let code = unsafe {
            tess_spill_reader_open(
                &raw mut other,
                page.as_ptr(),
                page.len(),
                &raw mut step,
                &raw mut status,
            )
        };
        assert_eq!(code, Code::Ok, "{}", status.message());
        if step.kind == READ_READY {
            block = vec![0; step.largest as usize];
            break;
        }
        assert!(read_step(&step, &set.file, &mut page, &mut block));
    }
    assert_eq!(read_all(&mut other, &mut page, &mut block), [16, 400, 24]);
    seek(&mut other, &mut page, &mut block, places[2]);
    assert_eq!(read_all(&mut other, &mut page, &mut block), [24]);

    // The chunk of records reads back unpacked, its links cleared.
    let list = &set.entries[0][..set.lists[0].count as usize];
    let mut records = Reader::default();
    // SAFETY: a local reader, the set's list, a local output.
    let code = unsafe {
        tess_spill_reader_own(
            &raw mut records,
            2,
            1,
            0,
            77,
            1 << 20,
            values,
            list.as_ptr(),
            list.len() as u64,
            &raw mut largest,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    let mut page = list.to_vec();
    let mut block = vec![0_u8; largest as usize];
    let mut step = SpillReadStep::default();
    loop {
        // SAFETY: the reader, page and block of this test.
        let code = unsafe {
            tess_spill_reader_next(
                &raw mut records,
                page.as_ptr(),
                page.len(),
                block.as_ptr(),
                block.len(),
                &raw mut step,
                &raw mut status,
            )
        };
        assert_eq!(code, Code::Ok, "{}", status.message());
        if !read_step(&step, &set.file, &mut page, &mut block) {
            break;
        }
    }
    assert_eq!(step.kind, READ_HEADER);
    assert!(step.header.packed > 0, "the chunk of records packs");
    let mut body = vec![0_u8; step.header.len as usize];
    // SAFETY: as above, and a local body.
    let code = unsafe {
        tess_spill_reader_body(
            &raw mut records,
            block.as_ptr(),
            block.len(),
            body.as_mut_ptr(),
            body.len(),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    let mut expected = chunk.clone();
    for record in 0..64 {
        let at = CHUNK_HEADER + record * 40 + RECORD_NEXT_OFFSET;
        expected[at..at + 4].fill(0);
    }
    assert_eq!(body, expected);
}

/// Each kind of block keeps to its own bound through the entry points: a
/// chunk of values longer than the chunk bound is written and read back,
/// one past the values bound is refused by the writer, and readers of the
/// own list and of the file take a block past the values they are given
/// as damaged data.
#[test]
fn the_entry_points_keep_each_kind_to_its_bound() {
    use relspill::set::{Entry, Reader};
    use tessera_capi::c::{
        READ_HEADER, READ_READY, SpillReadStep, tess_spill_reader_file, tess_spill_reader_next,
        tess_spill_reader_open, tess_spill_reader_own,
    };

    let mut set = CSet::bounded(1, 256, 512, 2048);
    set.write(0, 2, &[7; 1024]);
    let (code, status, _) = set.call(1, 0, 2, &[7; 2056]);
    assert_eq!(code, Code::InvalidArgument);
    assert!(
        status.message().contains("2056 bytes"),
        "{}",
        status.message()
    );
    let (code, status, _) = set.call(1, 0, 1, &[0; 520]);
    assert_eq!(code, Code::InvalidArgument);
    assert!(
        status.message().contains("520 bytes"),
        "{}",
        status.message()
    );
    set.finish();
    assert_eq!(set.stats().3, 1024);
    let list = set.entries[0][..set.lists[0].count as usize].to_vec();

    // The first block's header, or the code that refused it.
    let header = |reader: &mut Reader, page: &mut Vec<Entry>, block: &mut Vec<u8>| loop {
        let mut step = SpillReadStep::default();
        let mut status = Status::new();
        // SAFETY: the reader, page and block of this test.
        let code = unsafe {
            tess_spill_reader_next(
                reader,
                page.as_ptr(),
                page.len(),
                block.as_ptr(),
                block.len(),
                &raw mut step,
                &raw mut status,
            )
        };
        if code != Code::Ok || step.kind == READ_HEADER {
            return (code, step.header.len);
        }
        assert!(read_step(&step, &set.file, page, block));
    };
    for values in [1024, 1016] {
        let expected = if values == 1024 {
            (Code::Ok, 1024)
        } else {
            (Code::DataCorrupted, 0)
        };
        let mut own = Reader::default();
        let mut largest = 0;
        let mut status = Status::new();
        // SAFETY: a local reader, the set's list, a local output.
        let code = unsafe {
            tess_spill_reader_own(
                &raw mut own,
                1,
                1,
                0,
                77,
                512,
                values,
                list.as_ptr(),
                list.len() as u64,
                &raw mut largest,
                &raw mut status,
            )
        };
        assert_eq!(code, Code::Ok, "{}", status.message());
        let mut page = list.clone();
        let mut block = vec![0_u8; largest as usize];
        assert_eq!(header(&mut own, &mut page, &mut block), expected);

        let mut other = Reader::default();
        // SAFETY: a local reader.
        let code = unsafe {
            tess_spill_reader_file(
                &raw mut other,
                1,
                1,
                0,
                77,
                512,
                values,
                set.file.len() as u64,
                &raw mut status,
            )
        };
        assert_eq!(code, Code::Ok, "{}", status.message());
        let mut page = vec![Entry::default(); 2];
        let mut block = Vec::new();
        loop {
            let mut step = SpillReadStep::default();
            // SAFETY: the reader and page of this test.
            let code = unsafe {
                tess_spill_reader_open(
                    &raw mut other,
                    page.as_ptr(),
                    page.len(),
                    &raw mut step,
                    &raw mut status,
                )
            };
            assert_eq!(code, Code::Ok, "{}", status.message());
            if step.kind == READ_READY {
                block = vec![0; step.largest as usize];
                break;
            }
            assert!(read_step(&step, &set.file, &mut page, &mut block));
        }
        assert_eq!(header(&mut other, &mut page, &mut block), expected);
    }
}

/// Each misuse of a reader's entry points is refused with its reason.
#[test]
fn the_reader_entry_points_refuse_what_no_reader_takes() {
    use relspill::set::Reader;
    use tessera_capi::c::{
        SpillReadStep, tess_spill_reader_body, tess_spill_reader_file, tess_spill_reader_next,
        tess_spill_reader_open, tess_spill_reader_seek,
    };

    let mut status = Status::new();
    let mut step = SpillReadStep::default();
    // SAFETY: a null reader, refused, and local outputs.
    let code = unsafe {
        tess_spill_reader_next(
            std::ptr::null_mut(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            &raw mut step,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
    assert!(status.message().contains("a null reader"));
    let mut reader = Reader::default();
    // SAFETY: a local reader.
    let code = unsafe {
        tess_spill_reader_file(
            &raw mut reader,
            2,
            1,
            2,
            77,
            1 << 20,
            0,
            64,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
    assert!(status.message().contains("partition 2 is past the 2"));
    // SAFETY: a local reader.
    let code = unsafe {
        tess_spill_reader_file(
            &raw mut reader,
            2,
            1,
            0,
            77,
            1 << 20,
            0,
            16,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    // SAFETY: a page of no entries, refused, and a local step.
    let code = unsafe {
        tess_spill_reader_open(
            &raw mut reader,
            std::ptr::null(),
            0,
            &raw mut step,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
    assert!(status.message().contains("a page of 0 entries"));
    let page = [relspill::set::Entry::default(); 2];
    // SAFETY: a local page and step.
    let code = unsafe {
        tess_spill_reader_open(
            &raw mut reader,
            page.as_ptr(),
            2,
            &raw mut step,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::DataCorrupted, "{}", status.message());
    assert!(
        status
            .message()
            .contains("16 bytes has no list of its blocks")
    );
    // SAFETY: a local reader, a body with no header before it.
    let code = unsafe {
        tess_spill_reader_body(
            &raw mut reader,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
    assert!(status.message().contains("read after its header"));
    // SAFETY: a local reader, a seek with no block.
    let code = unsafe {
        tess_spill_reader_seek(
            &raw mut reader,
            page.as_ptr(),
            2,
            8,
            &raw mut step,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
    assert!(
        status.message().contains("no state"),
        "{}",
        status.message()
    );
}

/// Where a value of a PostgreSQL type ends in the bytes from its start,
/// as the C of `tess_spill_value_fits` tells (`tessera_test_spill_value`):
/// each value that ends at the last byte is taken, and refused one byte
/// shorter.
#[test]
fn a_value_ends_by_its_type() {
    use tessera_capi::c::value_ends;

    // A varlena of 16 bytes with a 4-byte header, one of 3 with a 1-byte
    // header, a compressed one of its 8 bytes of header alone, a TOAST
    // pointer to data on disk of 18 bytes, a C string, a fixed length.
    let four: Vec<u8> = [&(16_u32 << 2).to_le_bytes()[..], &[7; 12]].concat();
    let one = vec![3 << 1 | 1, 7, 7];
    let compressed: Vec<u8> = [&(8_u32 << 2 | 0x02).to_le_bytes()[..], &[0; 4]].concat();
    let external: Vec<u8> = [&[0x01, 18][..], &[0; 16]].concat();
    // A pointer to data on disk of a TOAST table of 64-bit chunk ids.
    let external8: Vec<u8> = [&[0x01, 4][..], &[0; 20]].concat();
    let string = b"tessera\0".to_vec();
    for (typlen, value) in [
        (-1, &four),
        (-1, &one),
        (-1, &compressed),
        (-1, &external),
        (-1, &external8),
        (-2, &string),
        (8, &vec![0; 8]),
    ] {
        assert!(value_ends(typlen, value), "{typlen} {value:?}");
        assert!(
            !value_ends(typlen, &value[..value.len() - 1]),
            "{typlen} {value:?} short"
        );
    }
    // A header that says fewer bytes than a header, a compressed value of
    // fewer than its 8 bytes of header, an external pointer of another
    // tag, no bytes, a lone 0x01, and a length no type has.
    let tiny = (3_u32 << 2).to_le_bytes();
    let short_compressed: Vec<u8> = [&(7_u32 << 2 | 0x02).to_le_bytes()[..], &[0; 4]].concat();
    let indirect: Vec<u8> = [&[0x01, 1][..], &[0; 16]].concat();
    for (typlen, value) in [
        (-1, &tiny[..]),
        (-1, &short_compressed[..]),
        (-1, &indirect[..]),
        (-1, &[][..]),
        (-1, &[0x01][..]),
        (0, &[0; 8][..]),
        (-3, &[0; 8][..]),
    ] {
        assert!(!value_ends(typlen, value), "{typlen} {value:?}");
    }
}

/// A TOAST pointer to data on disk is copied by its bytes whatever its
/// kind: one of 64-bit chunk ids, VARTAG_ONDISK_OID8, of 22 bytes, as one
/// of 32-bit ids of 18.
#[test]
fn both_kinds_of_a_pointer_to_data_on_disk_append() {
    use tessera_capi::c::{DatumColumn, Mask, tess_spill_columns_append, tess_spill_columns_init};

    let pointers: [Vec<u8>; 2] = [
        [&[0x01, 4][..], &[5; 20]].concat(),
        [&[0x01, 18][..], &[6; 16]].concat(),
    ];
    let datums: Vec<u64> = pointers.iter().map(|value| value.as_ptr() as u64).collect();
    let isnull = [false; 2];
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: datums.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 2,
        ..DatumColumn::EMPTY
    };
    let len = 16 + 8 * 2 * (1 + 1);
    let mut chunk = vec![0_u64; len / 8];
    let mut values = vec![0_u8; 64];
    let (mut capacity, mut used, mut appended, mut need) = (0, 0_usize, 0, 0_usize);
    let mut word = 0b11_u64;
    let mut rows = Mask {
        nrows: 2,
        bits: &raw mut word,
    };
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else.
    let code = unsafe {
        tess_spill_columns_init(
            chunk.as_mut_ptr().cast(),
            len,
            1,
            &raw mut capacity,
            &raw mut status,
        );
        tess_spill_columns_append(
            chunk.as_mut_ptr().cast(),
            len,
            1,
            &raw const column,
            [false].as_ptr(),
            [-1_i16].as_ptr(),
            &raw mut rows,
            values.as_mut_ptr(),
            values.len(),
            &raw mut used,
            &raw mut appended,
            &raw mut need,
            &raw mut status,
        )
    };
    assert_eq!((code, appended), (Code::Ok, 2), "{}", status.message());
    assert_eq!(used, 24 + 24);
    assert_eq!(&values[..22], &pointers[0][..]);
    assert_eq!(&values[24..42], &pointers[1][..]);
}

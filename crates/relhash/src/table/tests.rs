use super::testing::IntKeys;
use super::*;

/// The blocks of a table, which the threads of a test attach to.
struct Shared {
    index: *mut u8,
    len: usize,
    bases: Vec<*mut u8>,
    lens: Vec<usize>,
}
// SAFETY: every thread accesses the blocks only through tables.
unsafe impl Sync for Shared {}

impl Shared {
    fn table(&self) -> Table<'_> {
        // SAFETY: the blocks outlive the borrow and are accessed only
        // through tables; the chunks are all linked before any thread
        // probes.
        unsafe {
            Table::attach(
                self.index,
                self.len,
                Chunks::new(&self.bases, &self.lens).unwrap(),
            )
        }
        .unwrap()
    }
}

/// Four threads fill a filter together, each adding a stripe of the
/// rows of a batch of 200 that crosses every word of its mask: the
/// filter holds what one plain add of every row gives, word for word.
#[test]
fn four_threads_fill_a_filter_as_one_add_fills_it() {
    let hashes: Vec<u32> = (0..200).map(murmurhash32).collect();
    let mut all_rows = vec![u64::MAX; 4];
    all_rows[3] = (1 << 8) - 1;
    let nwords = bloom::words_for(200).unwrap();
    let mut alone = vec![0; nwords];
    let rows = RowMaskView::try_new(200, &all_rows).unwrap();
    bloom::add(&mut alone, &hashes, &rows).unwrap();
    let mut together = vec![0_u64; nwords];
    // An address, which a thread may take where a pointer may not go.
    let words = together.as_mut_ptr() as usize;
    std::thread::scope(|scope| {
        for thread in 0..4 {
            let hashes = &hashes;
            scope.spawn(move || {
                let mut stripe = [0_u64; 4];
                for row in (thread..200).step_by(4) {
                    stripe[row / 64] |= 1 << (row % 64);
                }
                let rows = RowMaskView::try_new(200, &stripe).unwrap();
                // SAFETY: the words are aligned, live through the
                // scope, and every thread changes them atomically.
                unsafe { bloom::add_atomic(words as *mut u64, nwords, hashes, &rows) }.unwrap();
            });
        }
    });
    assert_eq!(together, alone);
}

#[test]
fn one_of_four_threads_builds_a_shared_filter_every_key_passes() {
    let config = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 0,
    };
    let count = 200;
    let mut table = OwnedTable::new(&config, count, 4096).unwrap();
    let keys: Vec<i32> = (0..count as i32).map(|key| key * 7).collect();
    let hashes: Vec<u32> = keys.iter().map(|&key| murmurhash32(key as u32)).collect();
    let column = IntKeys(&keys);
    let mut all_rows = vec![u64::MAX; 4];
    all_rows[3] = (1 << (count - 192)) - 1;
    let mut pending_words = all_rows.clone();
    let mut pending = RowMask::try_new(count as usize, &mut pending_words).unwrap();
    let mut offsets = vec![0; count as usize];
    let inserted = table
        .insert(
            None,
            &mut Batch::new(&hashes, &column, &mut pending, &mut offsets).unwrap(),
        )
        .unwrap();
    assert_eq!(inserted, count as usize);
    assert!(table.chunks() > 1, "the records span chunks");
    // Each block's pointer and length from one borrow, the last one
    // taken of it: a later borrow would retire the pointer.
    let (index, len) = {
        let words = table.index_words();
        (words.as_mut_ptr().cast(), words.len() * 8)
    };
    let (bases, lens) = (0..table.chunks())
        .map(|chunk| {
            let words = table.chunk_words(chunk);
            (words.as_mut_ptr().cast::<u8>(), words.len() * 8)
        })
        .unzip();
    let shared = Shared {
        index,
        len,
        bases,
        lens,
    };
    let mut words = vec![0; bloom::shared_words_for(count).unwrap()];
    let filter = bloom::SharedFilter::from_mut(&mut words).unwrap();
    filter.init();
    let rows = RowMaskView::try_new(count as usize, &all_rows).unwrap();
    let mut found_words = [0; 4];
    let mut found = RowMask::try_new(count as usize, &mut found_words).unwrap();
    assert!(bloom::probe_shared(&filter, &hashes, &rows, &mut found).is_err());
    let built: usize = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    let table = shared.table();
                    let built = table.try_build_bloom(&filter).unwrap();
                    while !filter.ready() {
                        std::thread::yield_now();
                    }
                    let mut found_words = [0; 4];
                    let mut found = RowMask::try_new(count as usize, &mut found_words).unwrap();
                    bloom::probe_shared(&filter, &hashes, &rows, &mut found).unwrap();
                    assert_eq!(found_words, all_rows.as_slice(), "a key was rejected");
                    usize::from(built)
                })
            })
            .collect();
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .sum()
    });
    assert_eq!(built, 1);
}

/// Chunks of words the tests of partitioning add as they fill, each
/// read and written only through its pointer, taken once.
struct Blocks {
    _storage: Vec<Vec<u64>>,
    bases: Vec<*mut u8>,
    lens: Vec<usize>,
}

impl Blocks {
    fn new() -> Self {
        Self {
            _storage: Vec::new(),
            bases: Vec::new(),
            lens: Vec::new(),
        }
    }

    fn add(&mut self, len: usize) -> u32 {
        // A vector, not a box: moving a box would retag its memory as
        // unique and retire the pointer taken here.
        let mut block = vec![0_u64; len / 8];
        let base = block.as_mut_ptr().cast::<u8>();
        // SAFETY: the block is aligned to 8, `len` bytes, used only here.
        unsafe { init_chunk(base, len) }.unwrap();
        self._storage.push(block);
        self.bases.push(base);
        self.lens.push(len);
        (self.bases.len() - 1) as u32
    }

    fn chunks(&self) -> Chunks<'_> {
        // SAFETY: the blocks live as long as `self` and are accessed
        // only through tables and `records`.
        unsafe { Chunks::new(&self.bases, &self.lens) }.unwrap()
    }

    /// The (hash, next, key, payload) of every record of a chunk of
    /// records of one int4 key and one payload word.
    fn records(&self, chunk: u32) -> Vec<(u32, u32, i64, u64)> {
        let base = self.bases[chunk as usize].cast::<u64>();
        // SAFETY: the used mark and the records below it lie in the block.
        unsafe {
            let used = base.read() as usize;
            (CHUNK_HEADER..used)
                .step_by(32)
                .map(|byte| {
                    let record = base.add(byte / 8);
                    let header = record.read();
                    (
                        header as u32,
                        (header >> 32) as u32,
                        record.add(2).read() as i64,
                        record.add(3).read(),
                    )
                })
                .collect()
        }
    }
}

const PARTITION_CONFIG: TableConfig<'static> = TableConfig {
    keys: &[KeyKind::Int32],
    payload_size: 8,
};

/// 200 rows, the key's square as payload.
fn partition_rows() -> (Vec<i32>, Vec<u32>, Vec<u8>) {
    let keys: Vec<i32> = (0..200).collect();
    let hashes = keys.iter().map(|&key| murmurhash32(key as u32)).collect();
    let payload = keys
        .iter()
        .flat_map(|&key| (key as u64 * key as u64).to_ne_bytes())
        .collect();
    (keys, hashes, payload)
}

/// Every row once, each in a chunk of its hash's partition, whole.
fn check_partitioned(blocks: &Blocks, owner: &[u32], shift: u32, keys: &[i32]) {
    let mut seen = vec![false; keys.len()];
    for (chunk, &partition) in owner.iter().enumerate() {
        if partition == u32::MAX {
            continue;
        }
        for (hash, next, key, payload) in blocks.records(chunk as u32) {
            assert_eq!(
                (hash >> shift) & 3,
                partition,
                "record in another partition"
            );
            assert_eq!(hash, murmurhash32(key as u32));
            assert_eq!(payload, (key * key) as u64);
            assert_eq!(next, 0);
            assert!(!seen[key as usize], "key {key} twice");
            seen[key as usize] = true;
        }
    }
    assert!(seen.iter().all(|&seen| seen), "a row is missing");
}

#[test]
fn a_chunk_splits_into_the_chunks_of_its_partitions() {
    let (keys, hashes, payload) = partition_rows();
    let column = IntKeys(&keys);
    let mut blocks = Blocks::new();
    let source = blocks.add(CHUNK_HEADER + 200 * 32);
    let mut words = vec![u64::MAX, u64::MAX, u64::MAX, (1 << 8) - 1];
    let mut pending = RowMask::try_new(200, &mut words).unwrap();
    let mut offsets = vec![0; 200];
    append_to(
        &PARTITION_CONFIG,
        blocks.chunks(),
        source as usize,
        Some(&payload),
        &mut Batch::new(&hashes, &column, &mut pending, &mut offsets).unwrap(),
    )
    .unwrap();
    let len = CHUNK_HEADER + 8 * 32;
    let mut current: Vec<u32> = (0..4).map(|_| blocks.add(len)).collect();
    let mut owner = vec![u32::MAX, 0, 1, 2, 3];
    let mut from = CHUNK_HEADER;
    let mut copied = 0;
    let mut new_offsets = [0; 16];
    let mut split_hashes = [0; 16];
    loop {
        let partitions = Partitions {
            shift: 9,
            chunks: &current,
        };
        let split = split_to(
            &PARTITION_CONFIG,
            blocks.chunks(),
            &partitions,
            source as usize,
            &mut from,
            &mut new_offsets,
            &mut split_hashes,
        )
        .unwrap();
        for (&offset, &hash) in new_offsets.iter().zip(&split_hashes).take(split.count) {
            assert_eq!(owner[(offset >> UNIT_BITS) as usize], (hash >> 9) & 3);
        }
        copied += split.count;
        match split.full {
            Some(partition) => {
                current[partition as usize] = blocks.add(len);
                owner.push(partition);
            }
            None if split.count == 0 => break,
            None => {}
        }
    }
    assert_eq!(copied, 200);
    assert_eq!(from, CHUNK_HEADER + 200 * 32);
    check_partitioned(&blocks, &owner, 9, &keys);
}

#[test]
fn partitions_past_the_hash_or_the_chunks_are_refused() {
    let mut blocks = Blocks::new();
    // Four chunks for partitions and a fifth, the source of a split.
    for _ in 0..5 {
        blocks.add(CHUNK_HEADER + 256 * 32);
    }
    let many = vec![0; 2 * MAX_PARTITIONS];
    let cases: [(u32, &[u32], &str); 6] = [
        (0, &[], "power of two"),
        (0, &[0, 1, 2], "power of two"),
        (0, &many, "power of two"),
        (31, &[0, 1, 2, 3], "past the 32 bits"),
        (0, &[0, 1, 2, 9], "does not exist"),
        (32, &[0], "past the 32 bits"),
    ];
    // A lookup by partition into a table over the same chunks.
    let mut table = OwnedTable::new(&PARTITION_CONFIG, 64, CHUNK_HEADER + 256 * 32).unwrap();
    for _ in 0..4 {
        table.add_chunk().unwrap();
    }
    let keys = [1, 2, 3];
    let column = IntKeys(&keys);
    let (hashes, all) = key_batch(&keys);
    for (shift, chunks, message) in cases {
        let mut from = CHUNK_HEADER;
        let error = split_to(
            &PARTITION_CONFIG,
            blocks.chunks(),
            &Partitions { shift, chunks },
            4,
            &mut from,
            &mut [0; 4],
            &mut [0; 4],
        )
        .unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
        assert_eq!(from, CHUNK_HEADER);
        let (mut pending_words, mut inserted_words) = (all.clone(), vec![0; 1]);
        let error = table
            .table_mut()
            .unwrap()
            .find_or_insert_partitioned(
                &Partitions { shift, chunks },
                &mut Batch::new(
                    &hashes,
                    &column,
                    &mut RowMask::try_new(3, &mut pending_words).unwrap(),
                    &mut [0; 3],
                )
                .unwrap(),
                &mut RowMask::try_new(3, &mut inserted_words).unwrap(),
            )
            .unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
        assert_eq!(pending_words, all, "{message}: a row resolved");
        assert_eq!(table.table().unwrap().stats().records, 0);
    }
    let mut from = CHUNK_HEADER;
    let error = split_to(
        &PARTITION_CONFIG,
        blocks.chunks(),
        &Partitions {
            shift: 0,
            chunks: &[0, 1],
        },
        1,
        &mut from,
        &mut [0; 4],
        &mut [0; 4],
    )
    .unwrap_err();
    assert!(error.to_string().contains("into itself"), "{error}");
}

/// A split of no records would answer as the end of its chunk does;
/// one into a partition's chunk, or of records of another size, would
/// copy what it must not. Each fails, and nothing moves.
#[test]
fn a_split_that_cannot_be_done_is_refused() {
    let (keys, hashes, payload) = partition_rows();
    let column = IntKeys(&keys);
    let mut blocks = Blocks::new();
    let source = blocks.add(CHUNK_HEADER + 200 * 32);
    let mut words = vec![u64::MAX, u64::MAX, u64::MAX, (1 << 8) - 1];
    let mut pending = RowMask::try_new(200, &mut words).unwrap();
    let mut offsets = vec![0; 200];
    append_to(
        &PARTITION_CONFIG,
        blocks.chunks(),
        source as usize,
        Some(&payload),
        &mut Batch::new(&hashes, &column, &mut pending, &mut offsets).unwrap(),
    )
    .unwrap();
    let targets: Vec<u32> = (0..4).map(|_| blocks.add(CHUNK_HEADER + 64 * 32)).collect();
    let wider = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 16,
    };
    let cases: [(&TableConfig<'_>, usize, u32, &str); 3] = [
        (&PARTITION_CONFIG, 0, targets[0], "at least one record"),
        (&PARTITION_CONFIG, 16, source, "into itself"),
        (&wider, 16, targets[0], "record"),
    ];
    for (config, capacity, first, message) in cases {
        let chunks = [first, targets[1], targets[2], targets[3]];
        let mut from = CHUNK_HEADER;
        let error = split_to(
            config,
            blocks.chunks(),
            &Partitions {
                shift: 9,
                chunks: &chunks,
            },
            source as usize,
            &mut from,
            &mut vec![0; capacity],
            &mut vec![0; capacity],
        )
        .unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
        assert_eq!(from, CHUNK_HEADER, "{message}");
        for &target in &targets {
            assert!(blocks.records(target).is_empty(), "{message}: copied");
        }
    }
}

/// Keys and their hashes as a batch's key column.
fn key_batch(keys: &[i32]) -> (Vec<u32>, Vec<u64>) {
    let hashes = keys.iter().map(|&key| murmurhash32(key as u32)).collect();
    let mut words = vec![0; keys.len().div_ceil(64)];
    for row in 0..keys.len() {
        words[row / 64] |= 1 << (row % 64);
    }
    (hashes, words)
}

#[test]
fn groups_go_to_the_chunks_of_their_partitions() {
    let config = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 8,
    };
    // Records of 32 bytes, 8 to a chunk: the partitions fill.
    let mut table = OwnedTable::new(&config, 1024, CHUNK_HEADER + 8 * 32).unwrap();
    let mut current: Vec<u32> = (0..4).map(|_| table.add_chunk().unwrap() as u32).collect();
    let mut owner: Vec<u32> = (0..4).collect();
    let mut first = std::collections::HashMap::new();
    for batch in 0..3 {
        let keys: Vec<i32> = (0..100).map(|row| (batch * 100 + row) % 97).collect();
        let column = IntKeys(&keys);
        let (hashes, all) = key_batch(&keys);
        let mut pending_words = all.clone();
        let mut offsets = vec![0; keys.len()];
        // The first row of each key the table lacks makes its record.
        let mut new = std::collections::HashSet::new();
        let expected: Vec<usize> = (0..keys.len())
            .filter(|&row| !first.contains_key(&keys[row]) && new.insert(keys[row]))
            .collect();
        let mut made = Vec::new();
        loop {
            let mut inserted_words = vec![0; all.len()];
            let mut pending = RowMask::try_new(keys.len(), &mut pending_words).unwrap();
            let mut inserted = RowMask::try_new(keys.len(), &mut inserted_words).unwrap();
            table
                .table_mut()
                .unwrap()
                .find_or_insert_partitioned(
                    &Partitions {
                        shift: 3,
                        chunks: &current,
                    },
                    &mut Batch::new(&hashes, &column, &mut pending, &mut offsets).unwrap(),
                    &mut inserted,
                )
                .unwrap();
            made.extend(
                (0..keys.len()).filter(|row| inserted_words[row / 64] >> (row % 64) & 1 == 1),
            );
            if pending_words.iter().all(|&word| word == 0) {
                break;
            }
            let mut full = [false; 4];
            for row in 0..keys.len() {
                if pending_words[row / 64] >> (row % 64) & 1 == 1 {
                    full[((hashes[row] >> 3) & 3) as usize] = true;
                }
            }
            for partition in 0..4 {
                if full[partition] {
                    current[partition] = table.add_chunk().unwrap() as u32;
                    owner.push(partition as u32);
                }
            }
        }
        made.sort_unstable();
        assert_eq!(made, expected, "the inserted rows of batch {batch}");
        let reader = table.table().unwrap();
        for (row, &key) in keys.iter().enumerate() {
            let record = reader.record(offsets[row]).unwrap();
            assert_eq!(record.keys[0], i64::from(key));
            assert_eq!(
                owner[(offsets[row] >> UNIT_BITS) as usize],
                (hashes[row] >> 3) & 3,
                "a group outside its partition"
            );
            assert_eq!(*first.entry(key).or_insert(offsets[row]), offsets[row]);
        }
    }
    assert_eq!(table.table().unwrap().stats().records, 97);
}

/// The states of a group: flags, count, sum, min, max.
fn states(table: &OwnedTable, offset: u32) -> [i64; 5] {
    let reader = table.table().unwrap();
    let record = reader.record(offset).unwrap();
    let mut out = [0; 5];
    for (index, word) in out.iter_mut().enumerate() {
        *word = i64::from_ne_bytes(record.payload[8 * index..8 * index + 8].try_into().unwrap());
    }
    out
}

/// A lookup by partition stops every row once the records reach half
/// the buckets: the row that needs a record past them, and every row
/// after it, those of keys the table holds too.
#[test]
fn half_the_buckets_stops_every_row_of_a_lookup_by_partition() {
    // 1024 buckets take 512 records; 600 keys, then 6 the table holds.
    let mut table = OwnedTable::new(&PARTITION_CONFIG, 16, CHUNK_HEADER + 1000 * 32).unwrap();
    assert_eq!(table.table().unwrap().stats().buckets, 1024);
    let chunks: Vec<u32> = (0..4).map(|_| table.add_chunk().unwrap() as u32).collect();
    let keys: Vec<i32> = (0..600).chain(0..6).collect();
    let column = IntKeys(&keys);
    let (hashes, mut pending_words) = key_batch(&keys);
    let mut inserted_words = vec![0; pending_words.len()];
    let mut offsets = vec![0; keys.len()];
    let resolved = table
        .table_mut()
        .unwrap()
        .find_or_insert_partitioned(
            &Partitions {
                shift: 3,
                chunks: &chunks,
            },
            &mut Batch::new(
                &hashes,
                &column,
                &mut RowMask::try_new(keys.len(), &mut pending_words).unwrap(),
                &mut offsets,
            )
            .unwrap(),
            &mut RowMask::try_new(keys.len(), &mut inserted_words).unwrap(),
        )
        .unwrap();
    assert_eq!(resolved, 512);
    let bit = |words: &[u64], row: usize| words[row / 64] >> (row % 64) & 1 == 1;
    for row in 0..keys.len() {
        assert_eq!(bit(&pending_words, row), row >= 512, "row {row} pending");
        assert_eq!(bit(&inserted_words, row), row < 512, "row {row} inserted");
    }
    assert_eq!(table.table().unwrap().stats().records, 512);
}

/// A table of the keys, each with the states `of` gives it.
fn grouped(keys: &[i32], of: impl Fn(i32) -> [i64; 5], capacity: u64) -> (OwnedTable, Vec<u32>) {
    let config = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 40,
    };
    let mut table = OwnedTable::new(&config, capacity, CHUNK_HEADER + 16 * 56).unwrap();
    let column = IntKeys(keys);
    let (hashes, mut pending_words) = key_batch(keys);
    let mut inserted_words = vec![0; pending_words.len()];
    let mut offsets = vec![0; keys.len()];
    table
        .find_or_insert(
            &mut Batch::new(
                &hashes,
                &column,
                &mut RowMask::try_new(keys.len(), &mut pending_words).unwrap(),
                &mut offsets,
            )
            .unwrap(),
            &mut RowMask::try_new(keys.len(), &mut inserted_words).unwrap(),
        )
        .unwrap();
    for (row, &key) in keys.iter().enumerate() {
        let mut writer = table.table_mut().unwrap();
        let payload = writer.payload_mut(offsets[row]).unwrap();
        for (index, word) in of(key).iter().enumerate() {
            payload[8 * index..8 * index + 8].copy_from_slice(&word.to_ne_bytes());
        }
    }
    (table, offsets)
}

/// A merge stops where a new group finds no room in its chunk, and
/// where the records reach half the buckets, with `*from` on the first
/// group not merged; with a chunk and a larger index it goes on to the
/// end.
#[test]
fn a_merge_stops_for_a_chunk_or_a_larger_index() {
    let states = |key: i32| [0b1110, 1, i64::from(key), 0, 0];
    let (mut into, _) = grouped(&(0..10).collect::<Vec<_>>(), states, 16);
    let (mut from, _) = grouped(&(0..600).collect::<Vec<_>>(), states, 1024);
    let record = into.table().unwrap().record_size();
    let mut chunk = into.chunks() - 1;
    let (mut stops, mut total) = (Vec::new(), 0);
    for source_chunk in 0..from.chunks() {
        let words = from.chunk_words(source_chunk).to_vec();
        let source = into.add_chunk_copy(&words).unwrap();
        let (mut at, mut here) = (CHUNK_HEADER, 0);
        loop {
            let (merged, stop) = into
                .table_mut()
                .unwrap()
                .merge(source, &mut at, chunk, add_first)
                .unwrap();
            here += merged;
            assert_eq!(at, CHUNK_HEADER + here * record, "on the first not merged");
            stops.push(stop);
            match stop {
                CombineStop::Done => break,
                CombineStop::ChunkFull => chunk = into.add_chunk().unwrap(),
                CombineStop::IndexFull => {
                    let records = into.table().unwrap().stats().records;
                    assert_eq!(records, 512, "half of 1024 buckets");
                    // A regrow links every record of every chunk it is
                    // given: as the grouping does, the source is hidden.
                    let used = into.chunk_words(source)[0];
                    into.chunk_words(source)[0] = CHUNK_HEADER as u64;
                    into.regrow(records * 2).unwrap();
                    into.chunk_words(source)[0] = used;
                }
            }
        }
        // A source merged is done with, as the grouping frees it.
        into.chunk_words(source)[0] = CHUNK_HEADER as u64;
        total += here;
    }
    assert_eq!(total, 600);
    assert!(stops.contains(&CombineStop::ChunkFull));
    assert_eq!(
        stops
            .iter()
            .filter(|&&stop| stop == CombineStop::IndexFull)
            .count(),
        1
    );
    assert_eq!(into.table().unwrap().stats().records, 600);
}

/// The states of each key's record, found by its keys.
fn states_of(table: &mut OwnedTable, keys: &[i32]) -> Vec<[i64; 5]> {
    let column = IntKeys(keys);
    let (hashes, mut pending_words) = key_batch(keys);
    let mut inserted_words = vec![0; pending_words.len()];
    let mut offsets = vec![0; keys.len()];
    table
        .find_or_insert(
            &mut Batch::new(
                &hashes,
                &column,
                &mut RowMask::try_new(keys.len(), &mut pending_words).unwrap(),
                &mut offsets,
            )
            .unwrap(),
            &mut RowMask::try_new(keys.len(), &mut inserted_words).unwrap(),
        )
        .unwrap();
    assert!(
        inserted_words.iter().all(|&word| word == 0),
        "a key is missing"
    );
    offsets
        .iter()
        .map(|&offset| states(table, offset))
        .collect()
}

/// A rule that adds the first words of two payloads.
fn add_first(incoming: &[u8], ours: &mut [u8]) -> Result<()> {
    let theirs = i64::from_ne_bytes(incoming[..8].try_into()?);
    let sum = i64::from_ne_bytes(ours[..8].try_into()?) + theirs;
    ours[..8].copy_from_slice(&sum.to_ne_bytes());
    Ok(())
}

#[test]
fn a_merge_by_a_rule_changes_known_keys_and_copies_new_ones() {
    let (mut into, _) = grouped(
        &(0..10).collect::<Vec<_>>(),
        |key| [key.into(), 0, 0, 0, 0],
        64,
    );
    let theirs = |key: i32| [i64::from(key) * 100, 7, 7, 7, 7];
    let (mut from, _) = grouped(&(5..15).collect::<Vec<_>>(), theirs, 64);
    let words = from.chunk_words(0).to_vec();
    let source = into.add_chunk_copy(&words).unwrap();
    let chunk = into.add_chunk().unwrap();
    let mut at = CHUNK_HEADER;
    let (merged, stop) = into
        .table_mut()
        .unwrap()
        .merge(source, &mut at, chunk, add_first)
        .unwrap();
    assert_eq!((merged, stop), (10, CombineStop::Done));
    // Keys 5 to 9 add theirs to ours; keys 10 to 14 are whole copies.
    let expected: Vec<[i64; 5]> = (0..15)
        .map(|key| match key {
            0..5 => [key.into(), 0, 0, 0, 0],
            5..10 => [i64::from(key) * 101, 0, 0, 0, 0],
            _ => theirs(key),
        })
        .collect();
    assert_eq!(states_of(&mut into, &(0..15).collect::<Vec<_>>()), expected);
    assert_eq!(into.table().unwrap().stats().records, 15);
}

#[test]
fn a_failing_rule_stops_the_merge_with_its_own_error() {
    let (mut into, _) = grouped(&[1, 2, 3], |key| [key.into(), 0, 0, 0, 0], 64);
    let (mut from, _) = grouped(&[1, 2, 3], |_| [10, 0, 0, 0, 0], 64);
    let words = from.chunk_words(0).to_vec();
    let source = into.add_chunk_copy(&words).unwrap();
    let chunk = into.add_chunk().unwrap();
    let mut at = CHUNK_HEADER;
    let mut calls = 0;
    let error = into
        .table_mut()
        .unwrap()
        .merge(source, &mut at, chunk, |incoming, ours| {
            calls += 1;
            if calls == 2 {
                anyhow::bail!("the rule refuses the second record");
            }
            add_first(incoming, ours)
        })
        .unwrap_err();
    assert!(error.to_string().contains("refuses the second"), "{error}");
    let firsts: Vec<i64> = states_of(&mut into, &[1, 2, 3])
        .iter()
        .map(|states| states[0])
        .collect();
    assert_eq!(firsts, [11, 2, 3], "the first merged");
    let record = into.table().unwrap().record_size();
    assert_eq!(at, CHUNK_HEADER + record, "on the record the rule refused");
}

#[test]
fn a_merge_refuses_a_record_found_in_its_source() {
    let (mut into, _) = grouped(&[7], |_| [1, 0, 0, 0, 0], 64);
    let (mut from, _) = grouped(&[7], |_| [2, 0, 0, 0, 0], 64);
    let words = from.chunk_words(0).to_vec();
    let source = into.add_chunk_copy(&words).unwrap();
    let chunk = into.add_chunk().unwrap();
    // The bucket of key 7 is damaged to name the record of the source,
    // which a merge reads: its payload would be the rule's both.
    let (hashes, _) = key_batch(&[7]);
    let layout = into.table().unwrap().layout;
    let at = layout.buckets_offset + (hashes[0] >> layout.bucket_shift) as usize * 4;
    let index = into.index_words();
    let mut bytes = index[at / 8].to_ne_bytes();
    bytes[at % 8..at % 8 + 4].copy_from_slice(
        &header::Place {
            chunk: source,
            byte: CHUNK_HEADER,
        }
        .reference()
        .to_ne_bytes(),
    );
    index[at / 8] = u64::from_ne_bytes(bytes);
    let mut from_byte = CHUNK_HEADER;
    let error = into
        .table_mut()
        .unwrap()
        .merge(source, &mut from_byte, chunk, add_first)
        .unwrap_err();
    assert!(error.to_string().contains("which is merged"), "{error}");
}

#[test]
fn a_damaged_count_is_bounded_by_the_places_of_the_chunks() {
    let config = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 8,
    };
    let mut table = OwnedTable::new(&config, 10, 4096).unwrap();
    table.add_chunk().unwrap();
    table.add_chunk().unwrap();
    // The header counts more records than the chunks could ever hold:
    // a walk takes for its bound the places a reference can name in
    // the two chunks.
    table.index_words()[NRECORDS / 8] = u64::MAX;
    let table = table.table().unwrap();
    let access = Access::new(&table.region, &table.layout);
    assert_eq!(access.records(), 2 << UNIT_BITS);
}

#[test]
fn misaligned_or_odd_blocks_are_refused() {
    let config = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 0,
    };
    let mut words = vec![0_u64; index_size(&config, 1000).unwrap().div_ceil(8) + 2];
    let len = words.len() * 8 - 16;
    let base = words.as_mut_ptr().cast::<u8>();
    // SAFETY: `base + 4` and `base + 8` with `len` bytes lie inside the
    // vector, which nothing else uses meanwhile.
    unsafe {
        let misaligned = base.add(4);
        assert!(TableMut::create(misaligned, len, &config, 1000, Chunks::none()).is_err());
        assert!(Table::attach(misaligned, len, Chunks::none()).is_err());
        let odd = TableMut::create(base, len - 4, &config, 1000, Chunks::none());
        assert!(odd.unwrap_err().to_string().contains("multiple of 8"));
        let short = index_size(&config, 1000).unwrap() - 8;
        let short = TableMut::create(base, short, &config, 1000, Chunks::none());
        assert!(short.unwrap_err().to_string().contains("smaller than"));
        assert!(TableMut::create(base.add(8), len, &config, 1000, Chunks::none()).is_ok());
    }
    // Blocks whose used mark reads as an empty chunk's, so that only
    // the check of the chunk refuses them: one aligned, one 4 bytes in.
    let mut block = vec![0_u64; 8];
    let block_base = block.as_mut_ptr().cast::<u8>();
    // SAFETY: every base and length lies inside the vector, which
    // nothing else uses meanwhile.
    unsafe {
        block_base.cast::<u64>().write(CHUNK_HEADER as u64);
        let misaligned = block_base.add(32);
        misaligned
            .add(4)
            .cast::<u64>()
            .write_unaligned(CHUNK_HEADER as u64);
        let misaligned = misaligned.add(4);
        let cases = [
            (misaligned, 24, "misaligned"),
            (block_base, 28, "not a multiple of 8"),
            (block_base, 0, "shorter than its used mark"),
            (core::ptr::null_mut(), 24, "null"),
        ];
        for (base, len, what) in cases {
            let bases = [base];
            let lens = [len];
            // The array is taken as it is; the chunk, when a call
            // writes or walks it.
            let chunks = Chunks::new(&bases, &lens).unwrap();
            assert!(chunks.check_all().is_err(), "{what}");
            let appended = append_one(chunks);
            assert!(
                appended.is_err_and(|error| error.to_string().contains("not aligned to 8")),
                "an append to a chunk {what}"
            );
        }
        assert!(
            Chunks::new(&[block_base], &[32, 32]).is_err(),
            "mismatched arrays"
        );
        let bases = [block_base];
        let lens = [32];
        let chunks = Chunks::new(&bases, &lens).unwrap();
        chunks.check_all().unwrap();
        assert_eq!(append_one(chunks).unwrap(), 1);
        assert!(init_chunk(block_base, 20).is_err());
        init_chunk(block_base, 32).unwrap();
    }
    assert_eq!(block[0], CHUNK_HEADER as u64);
}

/// Append the row of key 7 to chunk 0 of `chunks`, in a table of an
/// int4 key and no payload, records of 24 bytes: the rows appended.
fn append_one(chunks: Chunks<'_>) -> Result<usize> {
    let keys = [7];
    let column = IntKeys(&keys[..]);
    let (hashes, mut words) = key_batch(&keys);
    let mut pending = RowMask::try_new(1, &mut words)?;
    let mut offsets = [0];
    let config = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 0,
    };
    append_to(
        &config,
        chunks,
        0,
        None,
        &mut Batch::new(&hashes, &column, &mut pending, &mut offsets)?,
    )
}

#[test]
fn chunks_past_the_limits_are_refused() {
    // One block as long as the longest chunk and a word more, and as
    // many chunks as a table has and one more, each the block's used
    // mark alone.
    let mut words = vec![0_u64; (MAX_CHUNK_LEN + 8) / 8];
    let base = words.as_mut_ptr().cast::<u8>();
    let bases = vec![base; MAX_CHUNKS + 1];
    let lens = vec![CHUNK_HEADER; MAX_CHUNKS + 1];
    // SAFETY: every base is the vector's start, valid for the lengths
    // given; only a chunk of the block alone is written.
    unsafe {
        base.cast::<u64>().write(CHUNK_HEADER as u64);
        // A chunk longer than 1 MiB has places that no reference can
        // name: refused when a call starts on it.
        let one = [base];
        let too_long = [MAX_CHUNK_LEN + 8];
        let longest = Chunks::new(&one, &too_long).unwrap();
        assert!(longest.check_all().is_err());
        assert!(
            append_one(longest).is_err_and(|error| error.to_string().contains("not aligned to 8")),
            "an append to a chunk longer than 1 MiB"
        );
        assert!(init_chunk(base, MAX_CHUNK_LEN + 8).is_err());
        let long = [MAX_CHUNK_LEN];
        let longest = Chunks::new(&one, &long).unwrap();
        longest.check_all().unwrap();
        assert_eq!(append_one(longest).unwrap(), 1);
        assert!(Chunks::new(&bases[..MAX_CHUNKS], &lens[..MAX_CHUNKS]).is_ok());
        assert!(
            Chunks::new(&bases, &lens).is_err(),
            "more chunks than a reference can number"
        );
    }
}

/// PostgreSQL's `murmurhash32`, as the int4 kernel hashes a key: any
/// hash would do, and this one keeps the rows of the tests in the
/// partitions they always fell into.
fn murmurhash32(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h
}

//! The table's concurrent protocol under loom: participants append records
//! to chunks of their own, then link them into one index at once, each
//! with a compare-and-swap of its bucket's head, and a probe that reads a
//! head with acquire sees the whole record.
//!
//! The model is a second [`Region`] over loom's atomics, with the orderings
//! of [`order`] that [`super::region::RawRegion`] runs with, and the table's
//! own generic code runs over it. The header's fields and the buckets are
//! loom atomics, one per field or bucket; the chunks are plain bytes, and
//! every access to a record or to a chunk's used mark goes through a loom
//! cell of it first, so that loom reports a read not ordered after its
//! writing. A shared Bloom filter gets the same treatment, and a shared
//! build runs the phases of [`super::phases`] over a model of PostgreSQL's
//! `Barrier`. Run with `make rust-loom`.

use std::cell::UnsafeCell as StdUnsafeCell;
use std::sync::atomic::{AtomicU8, Ordering as Plain};

use ::loom::cell::UnsafeCell;
use ::loom::sync::Arc;
use ::loom::sync::atomic::{AtomicU32, AtomicU64};
use ::loom::thread;
use anyhow::Result;
use core::sync::atomic::Ordering;
use tessera_core::{ColumnView, RowMask, RowMaskView};

use super::bloom::{self, FilterRead, FilterShared};
use super::exclusive::{Cursor, scan};
use super::header::{
    CHUNK_HEADER, HEADER_SIZE, Header, KeyKind, Layout, NRECORDS, TableConfig, chunk_layout,
};
use super::phases::{Action, Counters, Participant};
use super::record::Access;
use super::region::{Region, order};
use super::shared_spill::{Spill, Words};
use super::{batch, index_size, init};

/// The orderings of the bucket heads: the region's, or relaxed ones that
/// break publication, for the test that the model notices.
#[derive(Clone, Copy)]
struct Heads {
    load: Ordering,
    cas: Ordering,
    cas_failed: Ordering,
}

const HEADS: Heads = Heads {
    load: order::LOAD,
    cas: order::CAS,
    cas_failed: order::CAS_FAILED,
};

const RELAXED_HEADS: Heads = Heads {
    load: Ordering::Relaxed,
    cas: Ordering::Relaxed,
    cas_failed: Ordering::Relaxed,
};

/// A chunk of the model: its bytes, a loom cell for its used mark and one
/// per record it may hold.
struct LoomChunk {
    bytes: StdUnsafeCell<Box<[u64]>>,
    used: UnsafeCell<()>,
    records: Vec<UnsafeCell<()>>,
}

/// An index of loom atomics and chunks of tracked bytes.
struct LoomRegion {
    len: usize,
    record_size: usize,
    heads: Heads,
    /// The header's 8-byte and 4-byte fields; `widths` records which of
    /// the two a 4-byte word was used as, which must never change.
    header64: Vec<AtomicU64>,
    header32: Vec<AtomicU32>,
    widths: Vec<AtomicU8>,
    buckets: Vec<AtomicU32>,
    chunks: Vec<LoomChunk>,
    chunk_len: usize,
}

// SAFETY: loom runs one thread at a time, and every access to the bytes of
// a chunk is announced to a loom cell first, which reports any two accesses
// that are not ordered; the slices lent out cover the records the table's
// contract gives the caller.
unsafe impl Sync for LoomRegion {}
// SAFETY: as for `Sync`.
unsafe impl Send for LoomRegion {}

impl LoomRegion {
    /// An index for `capacity` records of `CONFIG` and `nchunks` empty
    /// chunks of `records` records each.
    fn new(capacity: u64, nchunks: usize, records: usize, heads: Heads) -> Self {
        let len = index_size(&CONFIG, capacity).unwrap();
        let record_size = chunk_layout(&CONFIG).unwrap().record_size;
        let chunk_len = CHUNK_HEADER + records * record_size;
        let chunks = (0..nchunks)
            .map(|_| {
                let mut words = vec![0; chunk_len / 8].into_boxed_slice();
                words[0] = CHUNK_HEADER as u64;
                LoomChunk {
                    bytes: StdUnsafeCell::new(words),
                    used: UnsafeCell::new(()),
                    records: (0..records).map(|_| UnsafeCell::new(())).collect(),
                }
            })
            .collect();
        Self {
            len,
            record_size,
            heads,
            header64: (0..HEADER_SIZE / 8).map(|_| AtomicU64::new(0)).collect(),
            header32: (0..HEADER_SIZE / 4).map(|_| AtomicU32::new(0)).collect(),
            widths: (0..HEADER_SIZE / 4).map(|_| AtomicU8::new(0)).collect(),
            buckets: (0..(len - HEADER_SIZE) / 4)
                .map(|_| AtomicU32::new(0))
                .collect(),
            chunks,
            chunk_len,
        }
    }

    /// Note that the header word at `offset` is used `width` bytes wide.
    fn width(&self, offset: usize, width: u8) {
        for word in offset / 4..(offset + usize::from(width)) / 4 {
            let seen = self.widths[word].swap(width, Plain::Relaxed);
            assert!(
                seen == 0 || seen == width,
                "header byte {offset} read as {width} bytes and as {seen}"
            );
        }
    }

    fn header64(&self, offset: usize) -> &AtomicU64 {
        assert!(offset + 8 <= HEADER_SIZE && offset.is_multiple_of(8));
        self.width(offset, 8);
        &self.header64[offset / 8]
    }

    fn header32(&self, offset: usize) -> &AtomicU32 {
        assert!(offset + 4 <= HEADER_SIZE && offset.is_multiple_of(4));
        self.width(offset, 4);
        &self.header32[offset / 4]
    }

    fn bucket(&self, offset: usize) -> &AtomicU32 {
        assert!(offset >= HEADER_SIZE, "a bucket lies past the header");
        &self.buckets[(offset - HEADER_SIZE) / 4]
    }

    /// The loom cells of the records `len` bytes at `byte` of a chunk touch.
    fn cells(&self, chunk: usize, byte: usize, len: usize) -> &[UnsafeCell<()>] {
        assert!(byte >= CHUNK_HEADER && byte + len <= self.chunk_len);
        let first = (byte - CHUNK_HEADER) / self.record_size;
        let last = (byte + len - 1 - CHUNK_HEADER) / self.record_size;
        &self.chunks[chunk].records[first..=last]
    }

    fn address(&self, chunk: usize, byte: usize) -> *mut u8 {
        // SAFETY: the byte lies within the chunk's words, as `cells` or the
        // used mark's word checks.
        unsafe {
            (*self.chunks[chunk].bytes.get())
                .as_mut_ptr()
                .cast::<u8>()
                .add(byte)
        }
    }

    fn read(&self, chunk: usize, byte: usize, len: usize) -> *const u8 {
        for cell in self.cells(chunk, byte, len) {
            cell.with(|_| ());
        }
        self.address(chunk, byte)
    }

    fn write(&self, chunk: usize, byte: usize, len: usize) -> *mut u8 {
        for cell in self.cells(chunk, byte, len) {
            cell.with_mut(|_| ());
        }
        self.address(chunk, byte)
    }
}

impl Region for LoomRegion {
    fn len(&self) -> usize {
        self.len
    }

    fn store_u32(&self, offset: usize, value: u32) {
        self.header32(offset).store(value, order::STORE);
    }

    fn load_u32_relaxed(&self, offset: usize) -> u32 {
        self.header32(offset).load(order::RELAXED)
    }

    fn load_u64_relaxed(&self, offset: usize) -> u64 {
        self.header64(offset).load(order::RELAXED)
    }

    fn load_u64(&self, offset: usize) -> u64 {
        self.header64(offset).load(order::LOAD)
    }

    fn store_u64(&self, offset: usize, value: u64) {
        self.header64(offset).store(value, order::STORE);
    }

    fn fetch_add_u64(&self, offset: usize, delta: u64) -> u64 {
        self.header64(offset).fetch_add(delta, order::ADD)
    }

    /// The model's buckets are created zero and a model index is made
    /// once, so there is nothing to clear; a thousand stores would each be
    /// a branch of the model.
    unsafe fn zero_u32(&self, offset: usize, count: usize) {
        assert_eq!(offset, HEADER_SIZE, "only the buckets are cleared");
        assert_eq!(count, self.buckets.len(), "the buckets are cleared whole");
    }

    unsafe fn load_u32_in(&self, offset: usize) -> u32 {
        self.bucket(offset).load(self.heads.load)
    }

    unsafe fn cas_u32_in(&self, offset: usize, current: u32, new: u32) -> Result<u32, u32> {
        self.bucket(offset)
            .compare_exchange(current, new, self.heads.cas, self.heads.cas_failed)
    }

    fn prefetch(&self, _offset: usize) {}

    fn chunks(&self) -> usize {
        self.chunks.len()
    }

    fn chunk_len(&self, chunk: usize) -> usize {
        assert!(chunk < self.chunks.len());
        self.chunk_len
    }

    unsafe fn chunk_used(&self, chunk: usize) -> u64 {
        self.chunks[chunk].used.with(|_| ());
        // SAFETY: the used mark is the chunk's first word.
        unsafe { self.address(chunk, 0).cast::<u64>().read() }
    }

    unsafe fn set_chunk_used(&self, chunk: usize, used: u64) {
        self.chunks[chunk].used.with_mut(|_| ());
        // SAFETY: as for `chunk_used`.
        unsafe { self.address(chunk, 0).cast::<u64>().write(used) }
    }

    type Spot = (usize, usize);

    unsafe fn spot(&self, chunk: usize, byte: usize) -> (usize, usize) {
        assert!(chunk < self.chunks.len() && byte <= self.chunk_len);
        (chunk, byte)
    }

    fn advance((chunk, byte): (usize, usize), bytes: usize) -> (usize, usize) {
        (chunk, byte + bytes)
    }

    unsafe fn record(&self, (chunk, byte): (usize, usize), len: usize) -> &[u8] {
        // SAFETY: within the chunk, the cells checked.
        unsafe { core::slice::from_raw_parts(self.read(chunk, byte, len), len) }
    }

    unsafe fn record_mut(&self, (chunk, byte): (usize, usize), len: usize) -> &mut [u8] {
        // SAFETY: as in `record`.
        unsafe { core::slice::from_raw_parts_mut(self.write(chunk, byte, len), len) }
    }

    unsafe fn load_next(&self, (chunk, byte): (usize, usize)) -> u32 {
        // SAFETY: four bytes of a record, aligned to 4.
        unsafe { self.read(chunk, byte, 4).cast::<u32>().read() }
    }

    unsafe fn store_next(&self, (chunk, byte): (usize, usize), value: u32) {
        // SAFETY: as in `load_next`.
        unsafe { self.write(chunk, byte, 4).cast::<u32>().write(value) }
    }

    fn prefetch_record(&self, _spot: (usize, usize)) {}
}

/// Every row hashes to one bucket, so that the participants race for its
/// head.
const HASH: u32 = 0x5a5a_5a5a;
const CONFIG: TableConfig<'static> = TableConfig {
    keys: &[KeyKind::Int32],
    payload_size: 8,
};

/// A table of `nchunks` chunks of `records` records over the model, its
/// index made by this thread.
fn table(nchunks: usize, records: usize, heads: Heads) -> Result<(Arc<LoomRegion>, Layout)> {
    let region = LoomRegion::new(records as u64, nchunks, records, heads);
    // SAFETY: the region is new and this thread alone has it.
    let layout = unsafe { init(&region, &CONFIG, records as u64) }?;
    Ok((Arc::new(region), layout))
}

/// The payload of a key: its value, so that a reader can check it.
fn payload(keys: &[i32]) -> Vec<u8> {
    keys.iter()
        .flat_map(|key| i64::from(*key).to_ne_bytes())
        .collect()
}

/// Append `keys` to a chunk; the rows appended are returned.
fn append(region: &LoomRegion, layout: &Layout, chunk: usize, keys: &[i32]) -> Result<usize> {
    let nrows = keys.len();
    let hashes = vec![HASH; nrows];
    let columns = [ColumnView::try_new(keys, None)?];
    let mut pending_bits = [(1u64 << nrows) - 1];
    let mut pending = RowMask::try_new(nrows, &mut pending_bits)?;
    let mut offsets = vec![0; nrows];
    let payload = payload(keys);
    batch::append(
        region,
        layout,
        chunk,
        &hashes,
        &columns[..],
        Some(&payload),
        &mut pending,
        &mut offsets,
    )
}

/// Link a chunk's records from its start; the count linked is returned.
fn link(region: &LoomRegion, layout: &Layout, chunk: usize) -> Result<usize> {
    let mut from = CHUNK_HEADER;
    batch::link(region, layout, chunk, &mut from)
}

/// Link a chunk counting the records whose keys were there already.
fn link_counting(region: &LoomRegion, layout: &Layout, chunk: usize) -> Result<(usize, usize)> {
    let mut from = CHUNK_HEADER;
    batch::link_counting::<_, true>(region, layout, chunk, &mut from)
}

/// Probe for `keys`: the reference of each one's record, 0 for none.
fn probe(region: &LoomRegion, layout: &Layout, keys: &[i32]) -> Result<Vec<u32>> {
    let nrows = keys.len();
    let hashes = vec![HASH; nrows];
    let columns = [ColumnView::try_new(keys, None)?];
    let all = [(1u64 << nrows) - 1];
    let rows = RowMaskView::try_new(nrows, &all)?;
    let mut matches = vec![0; nrows];
    let mut found_bits = [0];
    let mut found = RowMask::try_new(nrows, &mut found_bits)?;
    batch::probe(
        region,
        layout,
        &hashes,
        &columns[..],
        &rows,
        &mut matches,
        &mut found,
    )?;
    Ok((0..nrows)
        .map(|row| {
            if found_bits[0] >> row & 1 == 1 {
                matches[row]
            } else {
                0
            }
        })
        .collect())
}

/// Check the record at `offset` holds `key` and its payload.
fn check_record(region: &LoomRegion, layout: &Layout, offset: u32, key: i32) -> Result<()> {
    let mut access = Access::new(region, layout);
    let view = access.locate(offset)?;
    assert_eq!(view.keys(), [i64::from(key)]);
    assert_eq!(view.payload(), payload(&[key]));
    Ok(())
}

/// The records of the one bucket's chain, and those a walk visits.
fn chain_and_walk(region: &LoomRegion, layout: &Layout) -> Result<(usize, usize)> {
    let mut access = Access::new(region, layout);
    let mut offset = access.head(HASH);
    let mut chain = 0;
    while offset != 0 {
        chain += 1;
        offset = access.locate(offset)?.next();
    }
    let mut out = [0; 8];
    let walked = scan(region, layout, &mut Cursor::start(), &mut out)?;
    Ok((chain, walked))
}

/// Participants append their shares to chunks of their own and link them
/// at once; then every key is found, and the count, the chain and a walk
/// agree.
fn concurrent_links(shares: &'static [&'static [i32]], duplicates: usize) {
    ::loom::model(move || {
        let total: usize = shares.iter().map(|keys| keys.len()).sum();
        let most = shares.iter().map(|keys| keys.len()).max().unwrap();
        let (region, layout) = table(shares.len(), most, HEADS).unwrap();
        let threads: Vec<_> = shares
            .iter()
            .enumerate()
            .map(|(chunk, keys)| {
                let (region, layout) = (region.clone(), layout);
                thread::spawn(move || {
                    assert_eq!(append(&region, &layout, chunk, keys).unwrap(), keys.len());
                    link_counting(&region, &layout, chunk).unwrap()
                })
            })
            .collect();
        let mut found = 0;
        for (thread, keys) in threads.into_iter().zip(shares) {
            let (linked, repeated) = thread.join().unwrap();
            assert_eq!(linked, keys.len());
            found += repeated;
        }
        // Of two records of one key, exactly the one linked later finds
        // the other, whatever the interleaving.
        assert_eq!(found, duplicates, "duplicates counted");
        assert_eq!(region.load_u64(NRECORDS), total as u64);
        for keys in shares {
            for (key, offset) in keys.iter().zip(probe(&region, &layout, keys).unwrap()) {
                assert_ne!(offset, 0, "key {key} not found");
                check_record(&region, &layout, offset, *key).unwrap();
            }
        }
        assert_eq!(chain_and_walk(&region, &layout).unwrap(), (total, total));
    });
}

#[test]
fn two_participants_link_their_chunks_into_one_bucket() {
    concurrent_links(&[&[1], &[2]], 0);
}

#[test]
fn two_participants_link_two_records_each() {
    concurrent_links(&[&[1, 2], &[3, 4]], 0);
}

#[test]
fn three_participants_link_into_one_bucket() {
    concurrent_links(&[&[1], &[2], &[3]], 0);
}

#[test]
fn two_participants_link_one_key_and_one_counts_it() {
    concurrent_links(&[&[1], &[1]], 1);
}

#[test]
fn a_key_shared_by_two_participants_counts_once() {
    concurrent_links(&[&[1, 2], &[2, 3]], 1);
}

#[test]
fn three_records_of_one_key_count_twice() {
    concurrent_links(&[&[5, 5], &[5]], 2);
}

/// One participant appends and links a key while another probes until it
/// finds it, then reads the record: the head's acquire must order the
/// reading after the writing.
fn published_record_is_seen_whole(heads: Heads) {
    ::loom::model(move || {
        let (region, layout) = table(1, 1, heads).unwrap();
        let writer = {
            let (region, layout) = (region.clone(), layout);
            thread::spawn(move || {
                assert_eq!(append(&region, &layout, 0, &[7]).unwrap(), 1);
                assert_eq!(link(&region, &layout, 0).unwrap(), 1);
            })
        };
        let offset = loop {
            let offset = probe(&region, &layout, &[7]).unwrap()[0];
            if offset != 0 {
                break offset;
            }
            thread::yield_now();
        };
        check_record(&region, &layout, offset, 7).unwrap();
        writer.join().unwrap();
    });
}

#[test]
fn a_probe_sees_a_published_record_whole() {
    published_record_is_seen_whole(HEADS);
}

#[test]
#[should_panic(expected = "Causality violation")]
fn relaxed_heads_let_a_probe_read_an_unwritten_record() {
    published_record_is_seen_whole(RELAXED_HEADS);
}

/// A shared Bloom filter of loom atomics: the state word and the words,
/// with the orderings of [`order`] or, for the test that the model
/// notices, a relaxed state.
struct LoomFilter {
    state: AtomicU64,
    words: Vec<AtomicU64>,
    publish: Ordering,
    ready: Ordering,
}

impl LoomFilter {
    fn new(records: u64, relaxed: bool) -> Self {
        let nwords = bloom::words_for(records).unwrap();
        Self {
            state: AtomicU64::new(0),
            words: (0..nwords).map(|_| AtomicU64::new(0)).collect(),
            publish: if relaxed {
                Ordering::Relaxed
            } else {
                order::STORE
            },
            ready: if relaxed {
                Ordering::Relaxed
            } else {
                order::LOAD
            },
        }
    }
}

impl FilterRead for LoomFilter {
    fn nwords(&self) -> usize {
        self.words.len()
    }

    fn load(&self, word: usize) -> u64 {
        self.words[word].load(order::RELAXED)
    }
}

impl FilterShared for LoomFilter {
    fn or(&self, word: usize, mask: u64) {
        self.words[word].fetch_or(mask, order::RELAXED);
    }

    fn claim(&self) -> bool {
        self.state
            .compare_exchange(0, 1, order::CAS, order::CAS_FAILED)
            .is_ok()
    }

    fn publish(&self) {
        self.state.store(2, self.publish);
    }

    fn ready(&self) -> bool {
        self.state.load(self.ready) == 2
    }
}

const FILTERED: [i32; 3] = [11, 22, 33];

/// A table of the filtered keys, built by this thread alone.
fn filtered_table() -> (Arc<LoomRegion>, Layout) {
    let (region, layout) = table(1, FILTERED.len(), HEADS).unwrap();
    assert_eq!(
        append(&region, &layout, 0, &FILTERED).unwrap(),
        FILTERED.len()
    );
    assert_eq!(link(&region, &layout, 0).unwrap(), FILTERED.len());
    (region, layout)
}

/// Check every key of the table against a ready filter: none may be
/// rejected.
fn every_key_passes(filter: &LoomFilter) {
    let hashes = [HASH; FILTERED.len()];
    let all = [(1u64 << FILTERED.len()) - 1];
    let rows = RowMaskView::try_new(FILTERED.len(), &all).unwrap();
    let mut found_bits = [0];
    let mut found = RowMask::try_new(FILTERED.len(), &mut found_bits).unwrap();
    bloom::probe_ready(filter, &hashes, &rows, &mut found).unwrap();
    assert_eq!(found_bits, all, "the filter rejected a key of the table");
}

/// Wait until the filter is ready, then check every key.
fn wait_and_check(filter: &LoomFilter) {
    while !filter.ready() {
        thread::yield_now();
    }
    every_key_passes(filter);
}

#[test]
fn two_participants_race_to_build_the_filter_and_one_does() {
    ::loom::model(|| {
        let (region, layout) = filtered_table();
        let filter = Arc::new(LoomFilter::new(FILTERED.len() as u64, false));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let (region, layout, filter) = (region.clone(), layout, filter.clone());
                thread::spawn(move || {
                    let built = bloom::try_build(&*region, &layout, &*filter).unwrap();
                    wait_and_check(&filter);
                    built
                })
            })
            .collect();
        let built = threads
            .into_iter()
            .map(|thread| usize::from(thread.join().unwrap()))
            .sum::<usize>();
        assert_eq!(built, 1);
    });
}

/// One participant builds while another, which does not want to, waits
/// for the filter: a probe before it is ready fails instead of letting
/// rows through or rejecting them, and after it every key passes.
fn a_reader_sees_the_built_filter(relaxed: bool) {
    ::loom::model(move || {
        let (region, layout) = filtered_table();
        let filter = Arc::new(LoomFilter::new(FILTERED.len() as u64, relaxed));
        let builder = {
            let (region, layout, filter) = (region.clone(), layout, filter.clone());
            thread::spawn(move || assert!(bloom::try_build(&*region, &layout, &*filter).unwrap()))
        };
        let hashes = [HASH; FILTERED.len()];
        let all = [(1u64 << FILTERED.len()) - 1];
        let rows = RowMaskView::try_new(FILTERED.len(), &all).unwrap();
        let mut found_bits = [0];
        let mut found = RowMask::try_new(FILTERED.len(), &mut found_bits).unwrap();
        if bloom::probe_ready(&*filter, &hashes, &rows, &mut found).is_ok() {
            assert_eq!(found_bits, all, "the filter rejected a key of the table");
        }
        wait_and_check(&filter);
        builder.join().unwrap();
    });
}

#[test]
fn a_reader_sees_the_filter_whole_once_it_is_ready() {
    a_reader_sees_the_built_filter(false);
}

#[test]
#[should_panic(expected = "rejected a key")]
fn a_relaxed_state_lets_a_reader_see_an_unfilled_filter() {
    a_reader_sees_the_built_filter(true);
}

/// PostgreSQL's `Barrier` (`storage/ipc/barrier.c`) over loom: the same
/// counts, phase and election, a mutex for its spinlock and a condition
/// variable for its own. `skip` makes arrivals in one phase return at
/// once, for the test that the model notices a missing wait.
struct LoomBarrier {
    state: ::loom::sync::Mutex<BarrierState>,
    released: ::loom::sync::Condvar,
    skip: Option<u32>,
}

struct BarrierState {
    participants: u32,
    arrived: u32,
    phase: u32,
    elected: u32,
}

impl LoomBarrier {
    fn new(skip: Option<u32>) -> Self {
        Self {
            state: ::loom::sync::Mutex::new(BarrierState {
                participants: 0,
                arrived: 0,
                phase: 0,
                elected: 0,
            }),
            released: ::loom::sync::Condvar::new(),
            skip,
        }
    }

    fn attach(&self) -> u32 {
        let mut state = self.state.lock().unwrap();
        state.participants += 1;
        state.phase
    }

    fn arrive_and_wait(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        let next = state.phase + 1;
        let skipping = self.skip == Some(state.phase);
        state.arrived += 1;
        if state.arrived == state.participants {
            state.arrived = 0;
            state.phase = next;
            state.elected = next;
            drop(state);
            self.released.notify_all();
            return true;
        }
        if skipping {
            return false;
        }
        loop {
            if state.phase == next {
                if state.elected != next {
                    state.elected = next;
                    return true;
                }
                return false;
            }
            state = self.released.wait(state).unwrap();
        }
    }

    fn detach(&self, arrive: bool) -> bool {
        let mut state = self.state.lock().unwrap();
        state.participants -= 1;
        let release = (arrive || state.participants > 0) && state.arrived == state.participants;
        if release {
            state.arrived = 0;
            state.phase += 1;
        }
        let last = state.participants == 0;
        drop(state);
        if release {
            self.released.notify_all();
        }
        last
    }
}

/// The build counters over loom atomics.
struct LoomCounters {
    records: AtomicU64,
    null_columns: AtomicU64,
    chunks: AtomicU64,
}

impl Counters for LoomCounters {
    fn add_records(&self, rows: u64) {
        self.records.fetch_add(rows, order::RELAXED);
    }
    fn records(&self) -> u64 {
        self.records.load(order::RELAXED)
    }
    fn add_null_columns(&self, bits: u64) {
        self.null_columns.fetch_or(bits, order::RELAXED);
    }
    fn null_columns(&self) -> u64 {
        self.null_columns.load(order::RELAXED)
    }
    fn next_chunk(&self) -> u64 {
        self.chunks.fetch_add(1, order::RELAXED)
    }
}

/// A shared build: the inner side's keys, which the participants take
/// one at a time as a parallel scan hands out pages, a chunk per
/// participant, the index the elected one makes, the counters, the barrier
/// and the frees.
struct Build {
    keys: &'static [i32],
    next_key: ::loom::sync::atomic::AtomicUsize,
    region: LoomRegion,
    counters: LoomCounters,
    barrier: LoomBarrier,
    frees: ::loom::sync::atomic::AtomicUsize,
}

impl Build {
    fn new(keys: &'static [i32], participants: usize, skip: Option<u32>) -> Self {
        Self {
            keys,
            next_key: ::loom::sync::atomic::AtomicUsize::new(0),
            region: LoomRegion::new(keys.len() as u64, participants, keys.len(), HEADS),
            counters: LoomCounters {
                records: AtomicU64::new(0),
                null_columns: AtomicU64::new(0),
                chunks: AtomicU64::new(0),
            },
            barrier: LoomBarrier::new(skip),
            frees: ::loom::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The table as a participant attaches to it: the layout its header
    /// holds, which the elected one wrote.
    fn layout(&self) -> Layout {
        Header::load(&self.region)
            .validate(self.region.len())
            .unwrap()
    }

    /// Take keys of the inner side until none is left and append each to
    /// this participant's chunk, then report how many.
    fn build(&self, chunk: usize) {
        let layout = chunk_layout(&CONFIG).unwrap();
        let mut appended = 0;
        loop {
            let index = self.next_key.fetch_add(1, order::RELAXED);
            let Some(&key) = self.keys.get(index) else {
                break;
            };
            appended += append(&self.region, &layout, chunk, &[key]).unwrap();
        }
        self.counters.add_records(appended as u64);
    }

    /// Run participant `chunk`, which appends to the chunk of its number,
    /// checking that its probes find every key.
    fn participate(&self, chunk: usize) {
        let mut participant = Participant::new();
        let mut reply = 0;
        loop {
            let action = participant.next(&self.counters, reply).unwrap();
            reply = 0;
            match action {
                Action::Attach => reply = self.barrier.attach(),
                Action::ArriveAndWait => reply = u32::from(self.barrier.arrive_and_wait()),
                Action::Build => self.build(chunk),
                // No spill in the model: nothing to write.
                Action::Flush | Action::Outer => {}
                Action::Allocate | Action::Load => panic!("a build got {action:?}"),
                Action::Size => {
                    // SAFETY: the elected one alone uses the index now.
                    unsafe { init(&self.region, &CONFIG, self.counters.records()) }.unwrap();
                }
                Action::Link => {
                    let layout = self.layout();
                    link(&self.region, &layout, chunk).unwrap();
                }
                Action::Probe => {
                    let layout = self.layout();
                    let found = probe(&self.region, &layout, self.keys).unwrap();
                    for (key, offset) in self.keys.iter().zip(found) {
                        assert_ne!(offset, 0, "key {key} was lost");
                        check_record(&self.region, &layout, offset, *key).unwrap();
                    }
                }
                Action::ArriveAndDetach => reply = u32::from(self.barrier.detach(true)),
                Action::Detach => {
                    self.barrier.detach(false);
                }
                Action::Free => {
                    self.frees.fetch_add(1, order::RELAXED);
                    return;
                }
                Action::Done => return,
            }
        }
    }
}

/// `participants` build one table of `keys` and probe it; exactly one
/// frees it.
fn shared_build(participants: usize, keys: &'static [i32], skip: Option<u32>, preemptions: usize) {
    let mut model = ::loom::model::Builder::new();
    model.preemption_bound = Some(preemptions);
    model.max_branches = 100_000;
    model.check(move || {
        let build = Arc::new(Build::new(keys, participants, skip));
        let threads: Vec<_> = (0..participants)
            .map(|chunk| {
                let build = build.clone();
                thread::spawn(move || build.participate(chunk))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            build.frees.load(order::RELAXED),
            1,
            "the table is freed once"
        );
    });
}

#[test]
fn two_participants_append_size_link_and_probe() {
    shared_build(2, &[1, 2, 3], None, 3);
}

#[test]
fn three_participants_attach_at_any_phase() {
    shared_build(3, &[1, 2, 3], None, 2);
}

#[test]
#[should_panic(expected = "does not hold a table")]
fn linking_before_the_index_is_made_breaks_the_table() {
    shared_build(2, &[1, 2, 3], Some(super::phases::SIZE), 2);
}

/// The words of a shared spill as loom atomics, for `capacity`
/// partitions and the resident ones.
#[derive(Debug)]
struct LoomWords {
    words: Vec<AtomicU64>,
    capacity: usize,
}

/// The shared state of a spill over loom atomics.
type LoomSpill = Spill<LoomWords>;

/// A cleared spill state for `capacity` partitions and a budget.
fn loom_spill(capacity: usize, budget: u64) -> LoomSpill {
    let words = (0..super::shared_spill::words_for(capacity).unwrap())
        .map(|_| AtomicU64::new(0))
        .collect();
    let spill = Spill::over(LoomWords { words, capacity });
    spill.init(budget);
    spill
}

impl Words for LoomWords {
    fn load(&self, index: usize) -> u64 {
        self.words[index].load(order::LOAD)
    }
    fn store(&self, index: usize, value: u64) {
        self.words[index].store(value, order::STORE);
    }
    fn fetch_add(&self, index: usize, delta: u64) -> u64 {
        self.words[index].fetch_add(delta, order::ADD)
    }
    fn fetch_sub(&self, index: usize, delta: u64) -> u64 {
        self.words[index].fetch_sub(delta, order::ADD)
    }
    fn fetch_or(&self, index: usize, bits: u64) -> u64 {
        self.words[index].fetch_or(bits, order::CAS)
    }
    fn compare_exchange(&self, index: usize, current: u64, new: u64) -> Result<u64, u64> {
        self.words[index].compare_exchange(current, new, order::CAS, order::CAS_FAILED)
    }
    fn capacity(&self) -> usize {
        self.capacity
    }
}

#[test]
fn participants_past_the_budget_agree_on_one_split() {
    ::loom::model(|| {
        let spill = Arc::new(loom_spill(8, 100));
        let threads: Vec<_> = [4_u32, 8]
            .into_iter()
            .map(|wanted| {
                let spill = spill.clone();
                thread::spawn(move || {
                    let over = spill.add_bytes(60, None).unwrap();
                    // Whoever sees the budget passed splits; the first holds.
                    if over || spill.partitions() == 0 {
                        spill.split(wanted).unwrap()
                    } else {
                        spill.partitions()
                    }
                })
            })
            .collect();
        let seen: Vec<u32> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let partitions = spill.partitions();
        assert!(partitions == 4 || partitions == 8);
        assert!(
            seen.iter().all(|&p| p == partitions),
            "{seen:?} split apart"
        );
    });
}

#[test]
fn a_partition_goes_to_disk_once() {
    ::loom::model(|| {
        let spill = Arc::new(loom_spill(4, 10));
        spill.split(4).unwrap();
        spill.add_bytes(30, Some(1)).unwrap();
        spill.add_bytes(20, Some(2)).unwrap();
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let spill = spill.clone();
                thread::spawn(move || spill.evict_largest())
            })
            .collect();
        let marked: Vec<u32> = threads
            .into_iter()
            .filter_map(|t| t.join().unwrap())
            .collect();
        assert!(marked.contains(&1), "the largest went to disk");
        assert!(
            marked.len() < 2 || marked[0] != marked[1],
            "one partition marked twice"
        );
        assert!(spill.on_disk(1).unwrap());
    });
}

/// A round over one partition: its inner rows in files, which the
/// participants take one at a time and load into chunks of their own, the
/// index the elected one makes, the barrier and the frees.
struct Round {
    files: &'static [&'static [i32]],
    keys: usize,
    spill: LoomSpill,
    region: LoomRegion,
    barrier: LoomBarrier,
    frees: ::loom::sync::atomic::AtomicUsize,
    probes: ::loom::sync::atomic::AtomicUsize,
}

impl Round {
    fn new(files: &'static [&'static [i32]], participants: usize, skip: Option<u32>) -> Self {
        let keys = files.iter().map(|file| file.len()).sum();
        let spill = loom_spill(1, 0);
        spill.split(1).unwrap();
        Self {
            files,
            keys,
            spill,
            region: LoomRegion::new(keys as u64, participants, keys, HEADS),
            barrier: LoomBarrier::new(skip),
            frees: ::loom::sync::atomic::AtomicUsize::new(0),
            probes: ::loom::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn layout(&self) -> Layout {
        Header::load(&self.region)
            .validate(self.region.len())
            .unwrap()
    }

    /// Participant `chunk` of the round, loading into the chunk of its
    /// number; its probe finds every key of every file.
    fn participate(&self, chunk: usize) {
        let mut participant = Participant::new();
        let mut reply = 0;
        loop {
            let action = participant.round_step(reply).unwrap();
            reply = 0;
            match action {
                Action::Attach => reply = self.barrier.attach(),
                Action::ArriveAndWait => reply = u32::from(self.barrier.arrive_and_wait()),
                Action::Allocate => {
                    // SAFETY: the elected one alone uses the index now.
                    unsafe { init(&self.region, &CONFIG, self.keys as u64) }.unwrap();
                }
                Action::Load => {
                    let layout = self.layout();
                    let mut loaded = false;
                    loop {
                        let file = self.spill.take_file(0, false).unwrap() as usize;
                        let Some(keys) = self.files.get(file) else {
                            break;
                        };
                        append(&self.region, &layout, chunk, keys).unwrap();
                        loaded = true;
                    }
                    if loaded {
                        link(&self.region, &layout, chunk).unwrap();
                    }
                }
                Action::Probe => {
                    let layout = self.layout();
                    for file in self.files {
                        let found = probe(&self.region, &layout, file).unwrap();
                        for (key, offset) in file.iter().zip(found) {
                            assert_ne!(offset, 0, "key {key} was not loaded");
                            check_record(&self.region, &layout, offset, *key).unwrap();
                        }
                    }
                    self.probes.fetch_add(1, order::RELAXED);
                }
                Action::ArriveAndDetach => reply = u32::from(self.barrier.detach(true)),
                Action::Detach => {
                    self.barrier.detach(false);
                }
                Action::Free => {
                    self.frees.fetch_add(1, order::RELAXED);
                    return;
                }
                Action::Done => return,
                other => panic!("a round got {other:?}"),
            }
        }
    }
}

/// `participants` join a round over a partition of `files` at any time;
/// every file is loaded once, the probes see every key, one frees it.
fn round(
    participants: usize,
    files: &'static [&'static [i32]],
    skip: Option<u32>,
    preemptions: usize,
) {
    let mut model = ::loom::model::Builder::new();
    model.preemption_bound = Some(preemptions);
    model.max_branches = 100_000;
    model.check(move || {
        let round = Arc::new(Round::new(files, participants, skip));
        let threads: Vec<_> = (0..participants)
            .map(|chunk| {
                let round = round.clone();
                thread::spawn(move || round.participate(chunk))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            round.frees.load(order::RELAXED),
            1,
            "the round is freed once"
        );
        assert!(round.probes.load(order::RELAXED) >= 1, "someone probed");
    });
}

#[test]
fn two_participants_load_a_partition_and_probe_it() {
    round(2, &[&[1, 2], &[3]], None, 3);
}

#[test]
fn three_participants_join_a_round_at_any_phase() {
    round(3, &[&[1], &[2], &[3]], None, 2);
}

#[test]
#[should_panic(expected = "was not loaded")]
fn probing_before_every_file_is_loaded_misses_keys() {
    round(2, &[&[1], &[2]], Some(super::phases::ROUND_LOAD), 2);
}

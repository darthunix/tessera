//! The table's concurrent protocol under loom: insertions from several
//! threads reserve room with a compare-and-swap of the used mark, write
//! their records and publish each with a compare-and-swap of its bucket's
//! head, and a probe that reads a head with acquire sees the whole record.
//!
//! The model is a second [`Region`] over loom's atomics, with the orderings
//! of [`order`] that [`super::region::RawRegion`] runs with, and the table's
//! own generic code runs over it. The header's fields and the buckets are
//! loom atomics, one per field or bucket; the records are plain bytes, and
//! every access to a record goes through a loom cell of that record first,
//! so that loom reports a read of a record that is not ordered after its
//! writing. A shared Bloom filter gets the same treatment: its state word
//! and words are loom atomics, and the tests check that a participant
//! that reads the state ready sees every bit the builder set. Run with
//! `make rust-loom`.

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
use super::exclusive::{self, Cursor, scan};
use super::header::{CHUNK_USED, HEADER_SIZE, Header, KeyKind, Layout, NRECORDS, TableConfig};
use super::phases::{Action, Counters, Participant};
use super::record::Access;
use super::region::{Region, order};
use super::{batch, init, region_size};

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

/// A region of loom atomics and tracked record bytes.
struct LoomRegion {
    len: usize,
    buckets_offset: usize,
    record_size: usize,
    heads: Heads,
    /// The header's 8-byte and 4-byte fields; `widths` records which of
    /// the two a 4-byte word was used as, which must never change.
    header64: Vec<AtomicU64>,
    header32: Vec<AtomicU32>,
    widths: Vec<AtomicU8>,
    buckets: Vec<AtomicU32>,
    /// One loom cell per record, which tracks the accesses to its bytes.
    records: Vec<UnsafeCell<()>>,
    bytes: StdUnsafeCell<Box<[u64]>>,
}

// SAFETY: loom runs one thread at a time, and every access to `bytes` is
// announced to the loom cell of its record first, which reports any two
// accesses that are not ordered; the slices lent out cover the records
// the table's contract gives the caller.
unsafe impl Sync for LoomRegion {}
// SAFETY: as for `Sync`.
unsafe impl Send for LoomRegion {}

impl LoomRegion {
    fn new(layout: &Layout, heads: Heads) -> Self {
        let len = layout.region_len;
        let nrecords = (layout.buckets_offset - HEADER_SIZE) / layout.record_size;
        Self {
            len,
            buckets_offset: layout.buckets_offset,
            record_size: layout.record_size,
            heads,
            header64: (0..HEADER_SIZE / 8).map(|_| AtomicU64::new(0)).collect(),
            header32: (0..HEADER_SIZE / 4).map(|_| AtomicU32::new(0)).collect(),
            widths: (0..HEADER_SIZE / 4).map(|_| AtomicU8::new(0)).collect(),
            buckets: (0..layout.nbuckets).map(|_| AtomicU32::new(0)).collect(),
            records: (0..nrecords).map(|_| UnsafeCell::new(())).collect(),
            bytes: StdUnsafeCell::new(vec![0; len / 8].into_boxed_slice()),
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

    fn bucket(&self, offset: usize) -> Option<&AtomicU32> {
        (offset >= self.buckets_offset).then(|| &self.buckets[(offset - self.buckets_offset) / 4])
    }

    /// The loom cells of the records `len` bytes at `offset` touch.
    fn cells(&self, offset: usize, len: usize) -> &[UnsafeCell<()>] {
        assert!(offset >= HEADER_SIZE && offset + len <= self.buckets_offset);
        let first = (offset - HEADER_SIZE) / self.record_size;
        let last = (offset + len - 1 - HEADER_SIZE) / self.record_size;
        &self.records[first..=last]
    }

    fn read(&self, offset: usize, len: usize) -> *const u8 {
        for cell in self.cells(offset, len) {
            cell.with(|_| ());
        }
        // SAFETY: the offset is within the boxed words, checked by `cells`.
        unsafe { (*self.bytes.get()).as_ptr().cast::<u8>().add(offset) }
    }

    fn write(&self, offset: usize, len: usize) -> *mut u8 {
        for cell in self.cells(offset, len) {
            cell.with_mut(|_| ());
        }
        // SAFETY: as in `read`.
        unsafe { (*self.bytes.get()).as_mut_ptr().cast::<u8>().add(offset) }
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

    fn cas_u64(&self, offset: usize, current: u64, new: u64) -> Result<u64, u64> {
        self.header64(offset)
            .compare_exchange(current, new, order::CAS, order::CAS_FAILED)
    }

    fn fetch_add_u64(&self, offset: usize, delta: u64) -> u64 {
        self.header64(offset).fetch_add(delta, order::ADD)
    }

    unsafe fn bytes_mut(&self, offset: usize, len: usize) -> &mut [u8] {
        // SAFETY: the bytes lie in the records, and the caller's contract
        // keeps other accesses away, which the cells check.
        unsafe { core::slice::from_raw_parts_mut(self.write(offset, len), len) }
    }

    /// The model's buckets are created zero and a model table never
    /// grows, so there is nothing to clear; a thousand stores would each
    /// be a branch of the model.
    unsafe fn zero_u32(&self, offset: usize, count: usize) {
        assert_eq!(offset, self.buckets_offset, "only the buckets are cleared");
        assert_eq!(count, self.buckets.len(), "the buckets are cleared once");
    }

    unsafe fn load_u32_in(&self, offset: usize) -> u32 {
        match self.bucket(offset) {
            Some(bucket) => bucket.load(self.heads.load),
            // SAFETY: four bytes of a record, aligned to 4.
            None => unsafe { self.read(offset, 4).cast::<u32>().read() },
        }
    }

    fn prefetch(&self, _offset: usize) {}

    unsafe fn store_u32_in(&self, offset: usize, value: u32) {
        match self.bucket(offset) {
            Some(bucket) => bucket.store(value, order::STORE),
            // SAFETY: as in `load_u32_in`.
            None => unsafe { self.write(offset, 4).cast::<u32>().write(value) },
        }
    }

    unsafe fn cas_u32_in(&self, offset: usize, current: u32, new: u32) -> Result<u32, u32> {
        let bucket = self.bucket(offset).expect("only bucket heads are swapped");
        bucket.compare_exchange(current, new, self.heads.cas, self.heads.cas_failed)
    }

    unsafe fn bytes_in(&self, offset: usize, len: usize) -> &[u8] {
        // SAFETY: as in `bytes_mut`.
        unsafe { core::slice::from_raw_parts(self.read(offset, len), len) }
    }

    unsafe fn bytes_mut_in(&self, offset: usize, len: usize) -> &mut [u8] {
        // SAFETY: as in `bytes_mut`.
        unsafe { core::slice::from_raw_parts_mut(self.write(offset, len), len) }
    }
}

/// Every row hashes to one bucket, so that the threads race for its head.
const HASH: u32 = 0x5a5a_5a5a;
const CONFIG: TableConfig<'static> = TableConfig {
    keys: &[KeyKind::Int32],
    payload_size: 8,
};

/// A table of room for `capacity` records over the model.
fn table(capacity: u64, heads: Heads) -> Result<(Arc<LoomRegion>, Layout)> {
    let len = region_size(&CONFIG, capacity)?;
    let layout = Header::new(&CONFIG, capacity, len)?.validate(len)?;
    let region = LoomRegion::new(&layout, heads);
    // SAFETY: the region is new and this thread alone has it.
    let layout = unsafe { init(&region, &CONFIG, capacity) }?;
    Ok((Arc::new(region), layout))
}

/// The payload of a key: its value, so that a reader can check it.
fn payload(keys: &[i32]) -> Vec<u8> {
    keys.iter()
        .flat_map(|key| i64::from(*key).to_ne_bytes())
        .collect()
}

/// Insert `keys` as one batch; the rows inserted are returned.
fn insert(region: &LoomRegion, layout: &Layout, keys: &[i32]) -> Result<usize> {
    let nrows = keys.len();
    let hashes = vec![HASH; nrows];
    let columns = [ColumnView::try_new(keys, None)?];
    let mut pending_bits = [(1u64 << nrows) - 1];
    let mut pending = RowMask::try_new(nrows, &mut pending_bits)?;
    let mut offsets = vec![0; nrows];
    let payload = payload(keys);
    batch::insert(
        region,
        layout,
        &hashes,
        &columns[..],
        Some(&payload),
        &mut pending,
        &mut offsets,
    )
}

/// Probe for `keys`: the offset of each one's record, 0 for none.
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

/// Threads insert `batches` at once; then every key is found, and the
/// counters, the chain and a walk agree on the count.
fn concurrent_inserts(batches: &'static [&'static [i32]]) {
    ::loom::model(move || {
        let total: usize = batches.iter().map(|keys| keys.len()).sum();
        let (region, layout) = table(total as u64, HEADS).unwrap();
        let threads: Vec<_> = batches
            .iter()
            .map(|keys| {
                let (region, layout) = (region.clone(), layout);
                thread::spawn(move || insert(&region, &layout, keys).unwrap())
            })
            .collect();
        for (thread, keys) in threads.into_iter().zip(batches) {
            assert_eq!(thread.join().unwrap(), keys.len());
        }
        assert_eq!(region.load_u64(NRECORDS), total as u64);
        assert_eq!(
            region.load_u64(CHUNK_USED),
            (HEADER_SIZE + total * layout.record_size) as u64
        );
        for keys in batches {
            for (key, offset) in keys.iter().zip(probe(&region, &layout, keys).unwrap()) {
                assert_ne!(offset, 0, "key {key} not found");
                check_record(&region, &layout, offset, *key).unwrap();
            }
        }
        assert_eq!(chain_and_walk(&region, &layout).unwrap(), (total, total));
    });
}

#[test]
fn two_threads_insert_into_one_bucket() {
    concurrent_inserts(&[&[1], &[2]]);
}

#[test]
fn two_threads_reserve_and_publish_two_records_each() {
    concurrent_inserts(&[&[1, 2], &[3, 4]]);
}

#[test]
fn three_threads_insert_into_one_bucket() {
    concurrent_inserts(&[&[1], &[2], &[3]]);
}

#[test]
fn a_full_table_takes_one_record_of_two() {
    ::loom::model(|| {
        let (region, layout) = table(1, HEADS).unwrap();
        let threads: Vec<_> = [1, 2]
            .into_iter()
            .map(|key| {
                let (region, layout) = (region.clone(), layout);
                thread::spawn(move || insert(&region, &layout, &[key]).unwrap())
            })
            .collect();
        let inserted: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
        assert_eq!(inserted, 1);
        assert_eq!(region.load_u64(NRECORDS), 1);
        assert_eq!(region.load_u64(CHUNK_USED), layout.buckets_offset as u64);
        assert_eq!(chain_and_walk(&region, &layout).unwrap(), (1, 1));
    });
}

/// One thread inserts a key while another probes until it finds it, then
/// reads the record: the head's acquire must order the reading after the
/// writing.
fn published_record_is_seen_whole(heads: Heads) {
    ::loom::model(move || {
        let (region, layout) = table(1, heads).unwrap();
        let writer = {
            let (region, layout) = (region.clone(), layout);
            thread::spawn(move || assert_eq!(insert(&region, &layout, &[7]).unwrap(), 1))
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
    let (region, layout) = table(FILTERED.len() as u64, HEADS).unwrap();
    assert_eq!(insert(&region, &layout, &FILTERED).unwrap(), FILTERED.len());
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

/// Stage `keys` into a buffer of the table's records, outside the table.
fn staged(layout: &Layout, keys: &[i32]) -> Vec<u8> {
    let nrows = keys.len();
    let hashes = vec![HASH; nrows];
    let columns = [ColumnView::try_new(keys, None).unwrap()];
    let mut pending_bits = [(1u64 << nrows) - 1];
    let mut pending = RowMask::try_new(nrows, &mut pending_bits).unwrap();
    let payload = payload(keys);
    let mut buffer = vec![0; nrows * layout.record_size];
    let mut used = 0;
    let count = batch::stage(
        layout,
        &mut buffer,
        &mut used,
        &hashes,
        &columns[..],
        Some(&payload),
        &mut pending,
    )
    .unwrap();
    assert_eq!((count, used), (nrows, buffer.len()));
    buffer
}

/// Threads add their staged rows, or insert rows, into one bucket at once;
/// then every key is found whole and the counts agree.
fn concurrent_links(batches: &'static [(&'static [i32], bool)]) {
    ::loom::model(move || {
        let total: usize = batches.iter().map(|(keys, _)| keys.len()).sum();
        let (region, layout) = table(total as u64, HEADS).unwrap();
        let threads: Vec<_> = batches
            .iter()
            .map(|&(keys, stage)| {
                let (region, layout) = (region.clone(), layout);
                let buffer = stage.then(|| staged(&layout, keys));
                thread::spawn(move || match buffer {
                    Some(buffer) => {
                        let mut consumed = 0;
                        let added = batch::insert_staged(&*region, &layout, &buffer, &mut consumed)
                            .unwrap();
                        assert_eq!(consumed, buffer.len());
                        added
                    }
                    None => insert(&region, &layout, keys).unwrap(),
                })
            })
            .collect();
        for (thread, (keys, _)) in threads.into_iter().zip(batches) {
            assert_eq!(thread.join().unwrap(), keys.len());
        }
        assert_eq!(region.load_u64(NRECORDS), total as u64);
        for (keys, _) in batches {
            for (key, offset) in keys.iter().zip(probe(&region, &layout, keys).unwrap()) {
                assert_ne!(offset, 0, "key {key} not found");
                check_record(&region, &layout, offset, *key).unwrap();
            }
        }
        assert_eq!(chain_and_walk(&region, &layout).unwrap(), (total, total));
    });
}

#[test]
fn two_participants_add_their_staged_rows_to_one_bucket() {
    concurrent_links(&[(&[1, 2], true), (&[3, 4], true)]);
}

#[test]
fn staged_rows_join_rows_inserted_at_the_same_time() {
    concurrent_links(&[(&[1, 2], true), (&[3], false)]);
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
    staged: AtomicU64,
    null_columns: AtomicU64,
}

impl Counters for LoomCounters {
    fn add_staged(&self, rows: u64) {
        self.staged.fetch_add(rows, order::RELAXED);
    }
    fn staged(&self) -> u64 {
        self.staged.load(order::RELAXED)
    }
    fn add_null_columns(&self, bits: u64) {
        self.null_columns.fetch_or(bits, order::RELAXED);
    }
    fn null_columns(&self) -> u64 {
        self.null_columns.load(order::RELAXED)
    }
}

impl LoomRegion {
    /// Copy the header and the first `used` bytes of records of `from`,
    /// as a participant copies a table into a larger region before
    /// growing it.
    fn copy_table(&self, from: &LoomRegion, used: usize) {
        for word in 0..HEADER_SIZE / 4 {
            match from.widths[word].load(Plain::Relaxed) {
                8 if word % 2 == 0 => {
                    let value = from.header64(word * 4).load(order::RELAXED);
                    self.header64(word * 4).store(value, order::RELAXED);
                }
                4 => {
                    let value = from.header32(word * 4).load(order::RELAXED);
                    self.header32(word * 4).store(value, order::RELAXED);
                }
                _ => {}
            }
        }
        let len = used - HEADER_SIZE;
        if len > 0 {
            let source = from.read(HEADER_SIZE, len);
            let target = self.write(HEADER_SIZE, len);
            // SAFETY: both ranges lie in their regions' records, checked by
            // `read` and `write`, and do not overlap: they are two regions.
            unsafe { core::ptr::copy_nonoverlapping(source, target, len) };
        }
    }
}

/// A shared build: the inner side's keys, which the participants take
/// one at a time as a parallel scan hands out pages, the table's region
/// sized by the estimate and the one for every record, which of them is
/// the table, the counters, the barrier and the frees.
struct Build {
    keys: &'static [i32],
    next_key: ::loom::sync::atomic::AtomicUsize,
    regions: [LoomRegion; 2],
    lens: [usize; 2],
    estimate: u64,
    current: ::loom::sync::atomic::AtomicUsize,
    counters: LoomCounters,
    barrier: LoomBarrier,
    frees: ::loom::sync::atomic::AtomicUsize,
}

impl Build {
    fn new(keys: &'static [i32], estimate: u64, skip: Option<u32>) -> Self {
        let total = keys.len() as u64;
        let region = |capacity: u64| {
            let len = region_size(&CONFIG, capacity).unwrap();
            let layout = Header::new(&CONFIG, capacity, len)
                .unwrap()
                .validate(len)
                .unwrap();
            (LoomRegion::new(&layout, HEADS), len)
        };
        let (first, first_len) = region(estimate);
        let (second, second_len) = region(total);
        Self {
            keys,
            next_key: ::loom::sync::atomic::AtomicUsize::new(0),
            regions: [first, second],
            lens: [first_len, second_len],
            estimate,
            current: ::loom::sync::atomic::AtomicUsize::new(0),
            counters: LoomCounters {
                staged: AtomicU64::new(0),
                null_columns: AtomicU64::new(0),
            },
            barrier: LoomBarrier::new(skip),
            frees: ::loom::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The table as a participant attaches to it: the region the elected
    /// one published and the layout its header holds.
    fn table(&self) -> (&LoomRegion, Layout) {
        let region = &self.regions[self.current.load(order::RELAXED)];
        let layout = Header::load(region).validate(region.len()).unwrap();
        (region, layout)
    }

    /// Take keys of the inner side until none is left, inserting each and
    /// staging those the table has no room for: the staged records, if
    /// any, once their count is reported.
    fn build(&self) -> Option<Vec<u8>> {
        let mut staged = Vec::new();
        let (region, layout) = self.table();
        loop {
            let index = self.next_key.fetch_add(1, order::RELAXED);
            let Some(&key) = self.keys.get(index) else {
                break;
            };
            staged.extend(self.insert_or_stage(region, &layout, &[key]));
        }
        let rows = staged.len() / layout.record_size;
        self.counters.add_staged(rows as u64);
        (rows > 0).then_some(staged)
    }

    /// Insert `keys`, staging the rows the table has no room for: the
    /// staged records.
    fn insert_or_stage(&self, region: &LoomRegion, layout: &Layout, keys: &[i32]) -> Vec<u8> {
        let layout = *layout;
        let nrows = keys.len();
        let hashes = vec![HASH; nrows];
        let columns = [ColumnView::try_new(keys, None).unwrap()];
        let payload = payload(keys);
        let mut pending_bits = [(1u64 << nrows) - 1];
        let mut pending = RowMask::try_new(nrows, &mut pending_bits).unwrap();
        let mut offsets = vec![0; nrows];
        batch::insert(
            region,
            &layout,
            &hashes,
            &columns[..],
            Some(&payload),
            &mut pending,
            &mut offsets,
        )
        .unwrap();
        let left = pending.as_view().selected_count();
        if left == 0 {
            return Vec::new();
        }
        let mut buffer = vec![0; left * layout.record_size];
        let mut used = 0;
        let staged = batch::stage(
            &layout,
            &mut buffer,
            &mut used,
            &hashes,
            &columns[..],
            Some(&payload),
            &mut pending,
        )
        .unwrap();
        assert_eq!(staged, left);
        buffer
    }

    /// Copy the table into the region for every record and grow it there.
    fn grow(&self) {
        let (from, layout) = self.table();
        let used = from.load_u64(CHUNK_USED) as usize;
        self.regions[1].copy_table(from, used);
        exclusive::grow(&self.regions[1], &layout, self.lens[1]).unwrap();
        self.current.store(1, order::RELAXED);
    }

    /// Run one participant, checking that its probes find every key.
    fn participate(&self) {
        let all = self.keys;
        let mut participant = Participant::new();
        let mut staged = None;
        let mut reply = 0;
        loop {
            let action = participant.next(&self.counters, reply).unwrap();
            reply = 0;
            match action {
                Action::Attach => reply = self.barrier.attach(),
                Action::ArriveAndWait => reply = u32::from(self.barrier.arrive_and_wait()),
                Action::Allocate => {
                    // SAFETY: the elected one alone uses the region now.
                    unsafe { init(&self.regions[0], &CONFIG, self.estimate) }.unwrap();
                    self.current.store(0, order::RELAXED);
                }
                Action::Build => staged = self.build(),
                Action::Grow => self.grow(),
                Action::Link => {
                    if let Some(buffer) = staged.take() {
                        let (region, layout) = self.table();
                        let mut consumed = 0;
                        let added =
                            batch::insert_staged(region, &layout, &buffer, &mut consumed).unwrap();
                        assert_eq!(
                            added * layout.record_size,
                            buffer.len(),
                            "a staged row was lost"
                        );
                    }
                }
                Action::Probe => {
                    let (region, layout) = self.table();
                    for (key, offset) in all.iter().zip(probe(region, &layout, all).unwrap()) {
                        assert_ne!(offset, 0, "key {key} was lost");
                        check_record(region, &layout, offset, *key).unwrap();
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

/// `participants` build one table of `keys` sized for `estimate` records
/// and probe it; exactly one frees it.
fn shared_build(
    participants: usize,
    keys: &'static [i32],
    estimate: u64,
    skip: Option<u32>,
    preemptions: usize,
) {
    let mut model = ::loom::model::Builder::new();
    model.preemption_bound = Some(preemptions);
    model.max_branches = 100_000;
    model.check(move || {
        let build = Arc::new(Build::new(keys, estimate, skip));
        let threads: Vec<_> = (0..participants)
            .map(|_| {
                let build = build.clone();
                thread::spawn(move || build.participate())
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
        assert_eq!(
            build.current.load(order::RELAXED),
            usize::from(estimate < keys.len() as u64),
            "the table grew exactly when the estimate fell short"
        );
    });
}

#[test]
fn two_participants_build_a_table_the_estimate_covers() {
    shared_build(2, &[1, 2], 2, None, 3);
}

#[test]
fn two_participants_stage_grow_and_link_past_the_estimate() {
    shared_build(2, &[1, 2, 3], 1, None, 2);
}

#[test]
fn three_participants_attach_at_any_phase() {
    shared_build(3, &[1, 2, 3], 1, None, 2);
}

#[test]
#[should_panic(expected = "was lost")]
fn linking_without_waiting_for_the_growth_loses_rows() {
    shared_build(2, &[1, 2, 3], 1, Some(super::phases::GROW), 2);
}

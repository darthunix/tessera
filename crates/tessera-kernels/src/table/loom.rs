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
use super::exclusive::{Cursor, scan};
use super::header::{CHUNK_USED, HEADER_SIZE, Header, KeyKind, Layout, NRECORDS, TableConfig};
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

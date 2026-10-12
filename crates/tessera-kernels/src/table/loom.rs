//! The table's concurrent protocol under loom: participants append records
//! to chunks of their own, then link them into one index at once, each
//! with a compare-and-swap of its bucket's head, and a probe that reads a
//! head with acquire sees the whole record.
//!
//! The models run the table's own generic code over the memory of
//! `model.rs`, a second [`Region`] over loom's atomics, where loom reports
//! a read of a record not ordered after its writing. A shared Bloom filter
//! and the marks of a RIGHT or FULL join get the same treatment. Run with
//! `make rust-loom`.

use ::loom::sync::Arc;
use ::loom::sync::atomic::AtomicU64;
use ::loom::thread;
use anyhow::Result;
use core::sync::atomic::Ordering;
use tessera_core::{RowMask, RowMaskView};

use super::bloom::{self, FilterOr, FilterRead, FilterShared};
use super::exclusive::{Cursor, scan};
use super::header::{CHUNK_HEADER, Layout, NRECORDS, reference};
use super::marks::{self, Marks};
use super::model::{CONFIG, HASH, HEADS, Heads, LoomRegion, append, check_record, link, probe};
use super::record::Access;
use super::region::{Region, order};
use super::{Chunks, batch, init};

const RELAXED_HEADS: Heads = Heads {
    load: Ordering::Relaxed,
    cas: Ordering::Relaxed,
    cas_failed: Ordering::Relaxed,
    late_count: false,
};

/// The region's orderings, with the count of records raised only once the
/// record is in its bucket: the order the model once found wrong.
const LATE_COUNT: Heads = Heads {
    late_count: true,
    ..HEADS
};

/// A table of `nchunks` chunks of `records` records over the model, its
/// index made by this thread.
fn table(nchunks: usize, records: usize, heads: Heads) -> Result<(Arc<LoomRegion>, Layout)> {
    let region = LoomRegion::new(records as u64, nchunks, records, heads)?;
    // SAFETY: the region is new and this thread alone has it.
    let layout = unsafe { init(&region, &CONFIG, records as u64) }?;
    Ok((Arc::new(region), layout))
}

/// Link a chunk counting the records whose keys were there already.
fn link_counting(region: &LoomRegion, layout: &Layout, chunk: usize) -> Result<(usize, usize)> {
    let mut from = CHUNK_HEADER;
    batch::link_counting::<_, true>(region, layout, chunk, &mut from)
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

/// The count of records raised after the record is published: a probe
/// that finds the record before the count covers it takes its sound chain
/// for a loop, and the model reports it.
#[test]
#[should_panic(expected = "table chain is longer than its record count")]
fn a_count_raised_after_publishing_makes_a_sound_chain_a_loop() {
    published_record_is_seen_whole(LATE_COUNT);
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

impl FilterOr for LoomFilter {
    fn or(&self, word: usize, mask: u64) {
        self.words[word].fetch_or(mask, order::RELAXED);
    }
}

impl FilterShared for LoomFilter {
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

/// A word of loom atomics changed by an atomic OR or, for the tests that
/// the model notices, by a plain read and a write.
struct LoomWord {
    word: AtomicU64,
    plain: bool,
}

impl LoomWord {
    fn new(plain: bool) -> Self {
        Self {
            word: AtomicU64::new(0),
            plain,
        }
    }

    fn or(&self, mask: u64) {
        if self.plain {
            let old = self.word.load(order::RELAXED);
            self.word.store(old | mask, order::RELAXED);
        } else {
            self.word.fetch_or(mask, order::RELAXED);
        }
    }
}

/// A filter of one word, without a state word, that participants fill at
/// once, as a spilling join's filter of every inner row.
impl FilterRead for LoomWord {
    fn nwords(&self) -> usize {
        1
    }

    fn load(&self, _: usize) -> u64 {
        self.word.load(order::RELAXED)
    }
}

impl FilterOr for LoomWord {
    fn or(&self, _: usize, mask: u64) {
        LoomWord::or(self, mask);
    }
}

/// The marks of a chunk of up to 64 records, set by every participant.
impl Marks for LoomWord {
    unsafe fn set(&self, _: usize, _: usize, bit: u64) {
        self.or(bit);
    }

    unsafe fn word(&self, _: usize, _: usize) -> u64 {
        self.word.load(order::LOAD)
    }
}

/// Two hashes whose four bits lie in the one word of a filter of one word,
/// each with a bit the other lacks, so that the bits of either can be lost.
fn two_hashes() -> [u32; 2] {
    let (_, first) = bloom::place(1, 64);
    let second = (2..)
        .find(|&hash| {
            let (_, mask) = bloom::place(hash, 64);
            mask & !first != 0 && first & !mask != 0
        })
        .unwrap();
    [1, second]
}

/// Two participants add a key each to one word of a filter at once; once
/// both are done, as after the barrier that orders the additions before
/// the probes, the filter lets both keys through.
fn participants_add_to_one_word(plain: bool) {
    let hashes = two_hashes();
    ::loom::model(move || {
        let filter = Arc::new(LoomWord::new(plain));
        let threads: Vec<_> = hashes
            .iter()
            .map(|&hash| {
                let filter = filter.clone();
                thread::spawn(move || {
                    let one = [1];
                    let rows = RowMaskView::try_new(1, &one).unwrap();
                    let shift = bloom::shift_for(1).unwrap();
                    bloom::add_with(&*filter, shift, &[hash], &rows).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let words = [filter.load(0)];
        let both = [0b11];
        let rows = RowMaskView::try_new(2, &both).unwrap();
        let mut found_bits = [0];
        let mut found = RowMask::try_new(2, &mut found_bits).unwrap();
        bloom::probe(&words, &hashes, &rows, &mut found).unwrap();
        assert_eq!(found_bits, both, "the bits of a key were lost");
    });
}

#[test]
fn participants_add_to_one_word_and_lose_no_bit() {
    participants_add_to_one_word(false);
}

#[test]
#[should_panic(expected = "the bits of a key were lost")]
fn a_plain_read_and_write_loses_the_bits_of_a_filter() {
    participants_add_to_one_word(true);
}

/// Two participants mark records 0 and 1 of a chunk of three at once; once
/// both are done, as the last participant to leave the table walks it,
/// only record 2 is without a mark.
fn participants_mark_one_word(plain: bool) {
    const SIZE: usize = 24;
    const LEN: usize = CHUNK_HEADER + 3 * SIZE;
    ::loom::model(move || {
        let marks = Arc::new(LoomWord::new(plain));
        let threads: Vec<_> = (0..2)
            .map(|record| {
                let marks = marks.clone();
                thread::spawn(move || {
                    let one = [1];
                    let rows = RowMaskView::try_new(1, &one).unwrap();
                    let refs = [reference(0, CHUNK_HEADER + record * SIZE)];
                    marks::mark(&[LEN], SIZE, &refs, &rows, &*marks).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        // The walk reads the used mark of a chunk of three records.
        let mut chunk = [0_u64; LEN / 8];
        chunk[0] = LEN as u64;
        let bases = [chunk.as_mut_ptr().cast::<u8>()];
        // SAFETY: the chunk is this array, read only by the walk below.
        let chunks = unsafe { Chunks::new(&bases, &[LEN]) }.unwrap();
        let mut out = [0; 4];
        let count =
            marks::scan_unmarked(&chunks, SIZE, &mut Cursor::start(), Some(&*marks), &mut out)
                .unwrap();
        assert_eq!(
            out[..count],
            [reference(0, CHUNK_HEADER + 2 * SIZE)],
            "a mark was lost"
        );
    });
}

#[test]
fn participants_mark_one_word_and_lose_no_mark() {
    participants_mark_one_word(false);
}

#[test]
#[should_panic(expected = "a mark was lost")]
fn a_plain_read_and_write_loses_marks() {
    participants_mark_one_word(true);
}

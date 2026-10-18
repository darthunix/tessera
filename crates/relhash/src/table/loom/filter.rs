//! The models of a shared Bloom filter: the race to build it and the
//! readers that wait for it to be ready.

use crate::RowMask;
use ::loom::sync::Arc;
use ::loom::sync::atomic::AtomicU64;
use ::loom::thread;
use anyhow::Result;
use core::sync::atomic::Ordering;

use super::memory::{HEADS, LoomRegion};
use super::model::{HASH, append, link, table};
use crate::table::bloom::{self, BUILDING, FilterOr, FilterRead, FilterShared, NONE, READY};
use crate::table::header::Layout;
use crate::table::region::order;

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
            state: AtomicU64::new(NONE),
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
            .compare_exchange(NONE, BUILDING, order::CAS, order::CAS_FAILED)
            .is_ok()
    }

    fn publish(&self) {
        self.state.store(READY, self.publish);
    }

    fn ready(&self) -> bool {
        self.state.load(self.ready) == READY
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

/// Check every key of the table against the filter: an error for a filter
/// that is not ready, and a key it rejects fails the model.
fn probe_every_key(filter: &LoomFilter) -> Result<()> {
    let hashes = [HASH; FILTERED.len()];
    let all = [(1u64 << FILTERED.len()) - 1];
    let mut found_bits = all;
    let mut found = RowMask::try_new(FILTERED.len(), &mut found_bits)?;
    bloom::probe_ready(filter, &hashes, &mut found)?;
    assert_eq!(found_bits, all, "the filter rejected a key of the table");
    Ok(())
}

/// Wait until the filter is ready, then check every key.
fn wait_and_check(filter: &LoomFilter) {
    while !filter.ready() {
        thread::yield_now();
    }
    probe_every_key(filter).unwrap();
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
        // Before the filter is ready the reader probes the table alone.
        let _ = probe_every_key(&filter);
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

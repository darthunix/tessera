//! The models of the table: participants that append and link at once,
//! and probes that read what was published.

use ::loom::thread;
use anyhow::Result;
use core::sync::atomic::Ordering;

use super::memory::{HEADS, Heads, LoomRegion};
use super::model::{HASH, append, check_record, link, probe, table};
use crate::table::batch;
use crate::table::header::{CHUNK_HEADER, Layout, NRECORDS};
use crate::table::record::Access;
use crate::table::region::Region;
use crate::table::writer::{Cursor, scan};

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

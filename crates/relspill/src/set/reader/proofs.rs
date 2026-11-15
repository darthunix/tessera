//! The proofs by Kani of the reader's checks of a file's trailer, for
//! every value its words and the file's length may hold.

use super::{ENTRY_SIZE, Entry, ReadConfig, Reader, TRAILER_SIZE};
use crate::{Bounds, HEADER_SIZE};

/// Trust point `trailer`: whatever words a trailer holds and however long
/// the file, a reader refuses them or takes lists that start inside the
/// file and fill the bytes between it and the trailer exactly, with the
/// counts of the partitions and whole entries; nothing overflows.
#[kani::proof]
fn a_trailer_that_passes_puts_the_lists_before_it() {
    let partitions: u32 = kani::any();
    let config = ReadConfig {
        partitions,
        level: 0,
        partition: 0,
        fingerprint: kani::any(),
        bounds: Bounds::all(kani::any()),
    };
    let file_len: u64 = kani::any();
    // A file shorter than a trailer is refused before its words are read.
    kani::assume(file_len >= TRAILER_SIZE as u64);
    let Ok(mut reader) = Reader::file(config, file_len) else {
        return;
    };
    let first = Entry {
        offset: kani::any(),
        stored: kani::any(),
    };
    let second = Entry {
        offset: kani::any(),
        stored: kani::any(),
    };
    if reader.read_trailer(first, second).is_err() {
        return;
    }
    kani::cover!(reader.most > 0, "a trailer of lists with entries passes");
    let end = file_len - TRAILER_SIZE as u64;
    assert!(reader.lists_at <= end);
    assert!(reader.lists_at + 8 * u64::from(partitions) + ENTRY_SIZE as u64 * reader.most == end);
    assert!(first.offset == super::TRAILER_MAGIC);
    assert!(second.offset == u64::from(partitions) && second.stored == config.fingerprint);
}

/// A reader of `partitions` partitions, reading partition `partition`,
/// after a trailer that passed: its lists of `most` entries start at
/// `lists_at`.
fn after_trailer(partitions: u32, partition: u32, lists_at: u64, most: u64) -> Reader {
    let mut reader = Reader::made(
        ReadConfig {
            partitions,
            level: 0,
            partition,
            fingerprint: 0,
            bounds: Bounds::all(0),
        },
        super::COUNTS,
    );
    reader.lists_at = lists_at;
    reader.most = most;
    reader
}

/// Trust point `list-counts`: from any counts taken so far that keep to
/// the lists, a page of any counts that the reader passes keeps them so:
/// the partition's blocks and those before it are no more than the
/// blocks counted, and those no more than the lists hold. So the
/// partition's entries, which start past the counts and the entries
/// before it, end inside the lists. That they are the partition's own,
/// and not a neighbour's, is data, which the tests show.
#[kani::proof]
#[kani::unwind(3)]
fn counts_that_pass_keep_the_partition_s_entries_in_the_lists() {
    let partitions: u32 = kani::any();
    let partition: u32 = kani::any();
    kani::assume(partition < partitions);
    let mut reader = after_trailer(partitions, partition, kani::any(), kani::any());
    // Any counts taken before, within the lists.
    reader.cursor = kani::any();
    reader.total = kani::any();
    reader.before = kani::any();
    reader.count = kani::any();
    kani::assume(reader.cursor <= u64::from(partitions));
    let within = |reader: &Reader| {
        reader
            .before
            .checked_add(reader.count)
            .is_some_and(|sum| sum <= reader.total)
    };
    kani::assume(within(&reader) && reader.total <= reader.most);
    // A page of one entry, its two words the counts asked for, as many as
    // the partitions left: a step from any state, so that a page of any
    // length is a series of such steps.
    let page = [Entry {
        offset: kani::any(),
        stored: kani::any(),
    }];
    let take: u64 = kani::any();
    kani::assume(take <= 2 && take <= u64::from(partitions) - reader.cursor);
    reader.asked_len = 8 * take;
    let first = reader.cursor;
    if reader.read_counts(&page).is_err() {
        return;
    }
    kani::cover!(
        first <= u64::from(partition) && u64::from(partition) < reader.cursor && reader.count > 0,
        "a page that counts blocks of the partition passes"
    );
    assert!(reader.cursor <= u64::from(partitions));
    assert!(within(&reader) && reader.total <= reader.most);
}

/// Trust point `entry`: whatever an entry holds, one that the reader
/// passes names a block of a header at least, of the largest block at
/// most, that ends before the next block of its partition and before the
/// lists; nothing overflows.
#[kani::proof]
fn an_entry_that_passes_names_a_block_before_the_next() {
    let mut reader = after_trailer(1, 0, kani::any(), 0);
    reader.bounds = Bounds {
        chunk: kani::any(),
        values: kani::any(),
    };
    reader.phase = kani::any();
    reader.largest = kani::any();
    let before = reader.largest;
    let entry = Entry {
        offset: kani::any(),
        stored: kani::any(),
    };
    let end: u64 = kani::any();
    if reader.check_entry(&entry, end).is_err() {
        return;
    }
    kani::cover!(
        reader.phase < super::READY,
        "an entry passes as the lists are opened"
    );
    kani::cover!(
        reader.phase >= super::READY,
        "an entry passes after the open"
    );
    assert!(entry.stored >= HEADER_SIZE as u64);
    assert!(end <= reader.lists_at);
    assert!(entry.offset <= end && end - entry.offset >= entry.stored);
    assert!(reader.largest == before.max(entry.stored));
    // Once the open gave the block buffer, no entry is longer than it.
    assert!(reader.phase < super::READY || entry.stored <= before);
    let most = reader.bounds.largest().saturating_add(super::block_slack());
    assert!(reader.phase >= super::READY || entry.stored <= most);
}

/// Trust point `list-counts`: once every count is taken, the reader opens
/// the partition only when the counts total exactly the entries the
/// lists hold, and the first page of the partition's entries it asks for
/// lies inside the lists; a partition of no block opens as none.
#[kani::proof]
fn counts_taken_open_the_partition_s_entries_inside_the_lists() {
    let partitions: u32 = kani::any();
    let partition: u32 = kani::any();
    kani::assume(partition < partitions);
    let (lists_at, most): (u64, u64) = (kani::any(), kani::any());
    let mut reader = after_trailer(partitions, partition, lists_at, most);
    // The file as its trailer lays it out.
    let Some(file_len) = most
        .checked_mul(ENTRY_SIZE as u64)
        .and_then(|entries| entries.checked_add(8 * u64::from(partitions) + TRAILER_SIZE as u64))
        .and_then(|lists| lists.checked_add(lists_at))
    else {
        return;
    };
    reader.file_len = file_len;
    reader.cursor = u64::from(partitions);
    reader.total = kani::any();
    reader.before = kani::any();
    reader.count = kani::any();
    let within = |reader: &Reader| {
        reader
            .before
            .checked_add(reader.count)
            .is_some_and(|sum| sum <= reader.total)
    };
    kani::assume(within(&reader) && reader.total <= reader.most);
    let page = [Entry::default(); 2];
    let Ok(step) = reader.open(&page) else {
        return;
    };
    assert!(reader.total == reader.most);
    match step {
        super::ReadStep::Empty => assert!(reader.count == 0),
        super::ReadStep::Page { at, len } => {
            kani::cover!(true, "the first page of a partition's entries is asked");
            let entries = lists_at + 8 * u64::from(partitions);
            assert!(at >= entries && at + len as u64 <= file_len - TRAILER_SIZE as u64);
            assert!(len > 0 && len <= page.len() * ENTRY_SIZE);
        }
        _ => assert!(false, "no other step after the counts"),
    }
}

/// Trust point `entry`: a page of entries that the reader passes, as it
/// opens the partition or reads one again, names blocks one after
/// another: each ends before the next of the page, and every one before
/// the lists.
#[kani::proof]
#[kani::unwind(3)]
fn a_page_that_passes_names_blocks_one_after_another() {
    let (lists_at, most): (u64, u64) = (kani::any(), kani::any());
    let mut reader = after_trailer(1, 0, lists_at, most);
    reader.bounds = Bounds::all(kani::any());
    reader.phase = super::READY;
    reader.count = kani::any();
    reader.largest = kani::any();
    // Lists that fit the 64 bits of a file's length.
    kani::assume(reader.count <= most && most <= 1 << 40 && lists_at <= 1 << 60);
    reader.entries_at = lists_at + 8;
    reader.file_len = lists_at + 8 + ENTRY_SIZE as u64 * most + TRAILER_SIZE as u64;
    // A page of the partition's entries asked again, from entry `index`.
    let (index, taken): (u64, u64) = (kani::any(), kani::any());
    kani::assume(
        taken >= 1 && taken <= 2 && index <= reader.count && taken <= reader.count - index,
    );
    reader.asked = super::PAGE;
    reader.asked_at = reader.entries_at + ENTRY_SIZE as u64 * index;
    reader.asked_len = ENTRY_SIZE as u64 * taken;
    let page = [
        Entry {
            offset: kani::any(),
            stored: kani::any(),
        },
        Entry {
            offset: kani::any(),
            stored: kani::any(),
        },
    ];
    if reader.took_page(&page).is_err() {
        return;
    }
    kani::cover!(taken == 2, "a page of two entries passes");
    for (at, entry) in page.iter().enumerate().take(taken as usize) {
        assert!(entry.stored >= HEADER_SIZE as u64);
        assert!(entry.offset <= reader.lists_at && reader.lists_at - entry.offset >= entry.stored);
        if at + 1 < taken as usize {
            assert!(page[at + 1].offset - entry.offset >= entry.stored);
        }
    }
}

/// Trust point `entry`: as the reader opens a partition, the page of its
/// entries it passes names blocks one after another, from the entry the
/// page before held: each ends before the next, and the last of the
/// partition before the lists.
#[kani::proof]
#[kani::unwind(3)]
fn entries_that_open_a_partition_name_blocks_one_after_another() {
    let (lists_at, most): (u64, u64) = (kani::any(), kani::any());
    let mut reader = after_trailer(1, 0, lists_at, most);
    reader.bounds = Bounds::all(kani::any());
    reader.phase = super::ENTRIES;
    reader.count = kani::any();
    reader.largest = kani::any();
    kani::assume(reader.count <= most && most <= 1 << 40 && lists_at <= 1 << 60);
    kani::assume(reader.largest <= reader.bounds.largest().saturating_add(super::block_slack()));
    reader.entries_at = lists_at + 8;
    reader.total = most;
    reader.file_len = lists_at + 8 + ENTRY_SIZE as u64 * most + TRAILER_SIZE as u64;
    // The entries taken before, the last of them held, and a page of the
    // next ones asked.
    let (cursor, taken): (u64, u64) = (kani::any(), kani::any());
    kani::assume(
        cursor <= reader.count && taken >= 1 && taken <= 2 && taken <= reader.count - cursor,
    );
    reader.cursor = cursor;
    let held = Entry {
        offset: kani::any(),
        stored: kani::any(),
    };
    reader.held_offset = held.offset;
    reader.held_stored = held.stored;
    reader.asked = super::PAGE;
    reader.asked_at = reader.entries_at + ENTRY_SIZE as u64 * cursor;
    reader.asked_len = ENTRY_SIZE as u64 * taken;
    let page = [
        Entry {
            offset: kani::any(),
            stored: kani::any(),
        },
        Entry {
            offset: kani::any(),
            stored: kani::any(),
        },
    ];
    let Ok(step) = reader.open(&page) else {
        return;
    };
    kani::cover!(cursor > 0 && taken == 2, "a page after another passes");
    if cursor > 0 {
        assert!(page[0].offset >= held.offset && page[0].offset - held.offset >= held.stored);
    }
    if taken == 2 {
        assert!(
            page[1].offset >= page[0].offset && page[1].offset - page[0].offset >= page[0].stored
        );
    }
    if let super::ReadStep::Ready { .. } = step {
        kani::cover!(true, "the partition opens");
        let last = page[taken as usize - 1];
        assert!(last.offset <= lists_at && lists_at - last.offset >= last.stored);
    }
}

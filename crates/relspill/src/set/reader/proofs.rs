//! The proofs by Kani of the reader's checks of a file's trailer, for
//! every value its words and the file's length may hold.

use super::{ENTRY_SIZE, Entry, ReadConfig, Reader, TRAILER_SIZE};
use crate::Bounds;

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

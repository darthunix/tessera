//! The proofs by Kani of the table's checks, for every value the blocks
//! may hold rather than for samples: `cargo kani -p relhash` runs them.
//!
//! Each proof runs the crate's own code over [`AnyRegion`], a region
//! whose reads give any value and whose methods assert what their safety
//! sections ask of a caller. So a proof shows that a check keeps every
//! read it guards inside its block, and, since Kani also checks every
//! arithmetic overflow, index and panic, that the code cannot panic. The
//! design of relhash names the proof of each trust point and module.

use crate::RowMaskView;
use core::ptr::NonNull;

use crate::error::Kind;

use super::batch::{check_partitions, partition_of};
use super::bloom::{place, shift_for, words_within};
use super::chunks::chunk_fits;
use super::header::{
    CHUNK_HEADER, Header, KEY_SLOT, Layout, MAX_KEYS, NRECORDS, Place, RECORD_HEADER,
};
use super::record::{Access, unlinked};
use super::region::Region;
use super::writer::Cursor;
use super::{
    Chunks, MAX_CHUNK_LEN, MAX_PARTITIONS, Marks, Partitions, UNIT_BITS, check_region, mark,
    mark_words, scan_unmarked,
};

/// The chunks of the model region.
const CHUNKS: usize = 2;

/// The bytes each chunk of the model holds: three records of the walk's
/// layout after the used mark.
const STORE: usize = CHUNK_HEADER + 3 * WALK_RECORD;

/// The record of the walk's layout: one key and no payload.
const WALK_RECORD: usize = RECORD_HEADER + KEY_SLOT;

/// The bytes of a chunk, aligned as a chunk is.
#[repr(C, align(8))]
struct Store([u8; STORE]);

/// A table's memory that may hold anything: every read of the index
/// gives any value, up to `most_records` for the record count; the chunks
/// have any lengths, any used marks and, up to [`STORE`] bytes, any
/// bytes. The methods with a safety section assert it.
struct AnyRegion {
    len: usize,
    nchunks: usize,
    lens: [usize; CHUNKS],
    used: [u64; CHUNKS],
    store: [Store; CHUNKS],
    most_records: u64,
}

impl AnyRegion {
    fn new() -> Self {
        Self {
            len: kani::any(),
            nchunks: kani::any_where(|&nchunks| nchunks <= CHUNKS),
            lens: kani::any(),
            used: kani::any(),
            store: [Store(kani::any()), Store(kani::any())],
            most_records: u64::MAX,
        }
    }

    /// Assert that `size` bytes at `offset` lie in the index.
    fn index(&self, offset: usize, size: usize) {
        assert!(
            offset.checked_add(size).is_some_and(|end| end <= self.len),
            "a read or write past the index"
        );
    }

    /// Assert that `size` bytes at `byte` of a chunk lie in it.
    fn chunk(&self, chunk: usize, byte: usize, size: usize) {
        assert!(chunk < self.nchunks, "a chunk past the chunks");
        assert!(
            byte.checked_add(size)
                .is_some_and(|end| end <= self.lens[chunk]),
            "a read or write past a chunk"
        );
    }
}

impl Region for AnyRegion {
    fn len(&self) -> usize {
        self.len
    }

    fn store_u32(&self, offset: usize, _: u32) {
        self.index(offset, 4);
    }

    fn load_u32_relaxed(&self, offset: usize) -> u32 {
        self.index(offset, 4);
        kani::any()
    }

    fn load_u64_relaxed(&self, offset: usize) -> u64 {
        self.index(offset, 8);
        kani::any()
    }

    fn load_u64(&self, offset: usize) -> u64 {
        self.index(offset, 8);
        if offset == NRECORDS {
            kani::any_where(|&count| count <= self.most_records)
        } else {
            kani::any()
        }
    }

    fn store_u64(&self, offset: usize, _: u64) {
        self.index(offset, 8);
    }

    fn fetch_add_u64(&self, offset: usize, _: u64) -> u64 {
        self.index(offset, 8);
        kani::any()
    }

    unsafe fn zero_u32(&self, offset: usize, count: usize) {
        self.index(offset, count * 4);
    }

    unsafe fn load_u32_in(&self, offset: usize) -> u32 {
        self.index(offset, 4);
        kani::any()
    }

    unsafe fn cas_u32_in(&self, offset: usize, current: u32, _: u32) -> Result<u32, u32> {
        self.index(offset, 4);
        if kani::any() {
            Ok(current)
        } else {
            Err(kani::any())
        }
    }

    fn prefetch(&self, _: usize) {}

    fn chunks(&self) -> usize {
        self.nchunks
    }

    fn chunk_len(&self, chunk: usize) -> usize {
        assert!(chunk < self.nchunks, "a chunk past the chunks");
        self.lens[chunk]
    }

    fn chunk_fits(&self, chunk: usize) -> bool {
        // A base aligned as the caller promises: the length decides.
        chunk_fits(
            NonNull::<u64>::dangling().as_ptr().cast(),
            self.chunk_len(chunk),
        )
    }

    unsafe fn chunk_used(&self, chunk: usize) -> u64 {
        self.chunk(chunk, 0, 8);
        self.used[chunk]
    }

    unsafe fn set_chunk_used(&self, chunk: usize, _: u64) {
        self.chunk(chunk, 0, 8);
    }

    type Spot = (usize, usize);

    unsafe fn spot(&self, chunk: usize, byte: usize) -> (usize, usize) {
        self.chunk(chunk, byte, 0);
        (chunk, byte)
    }

    fn advance((chunk, byte): (usize, usize), bytes: usize) -> (usize, usize) {
        (chunk, byte.wrapping_add(bytes))
    }

    unsafe fn chunk_bytes(&self, (chunk, byte): (usize, usize), len: usize) -> &[u8] {
        self.chunk(chunk, byte, len);
        &self.store[chunk].0[byte..byte + len]
    }

    unsafe fn chunk_bytes_mut(&self, (chunk, byte): (usize, usize), len: usize) -> &mut [u8] {
        self.chunk(chunk, byte, len);
        panic!("no proof writes a chunk")
    }

    unsafe fn load_next(&self, (chunk, byte): (usize, usize)) -> u32 {
        self.chunk(chunk, byte, 4);
        kani::any()
    }

    unsafe fn store_next(&self, (chunk, byte): (usize, usize), _: u32) {
        self.chunk(chunk, byte, 4);
    }

    fn prefetch_record(&self, _: (usize, usize)) {}
}

/// The layout of any header that passes its checks, read from a region
/// of any bytes as `Table::attach` reads it: the index's length checked
/// first, its base aligned as the caller promises.
fn any_layout(region: &AnyRegion) -> Option<Layout> {
    check_region(NonNull::<u64>::dangling().as_ptr().cast(), region.len).ok()?;
    Header::load(region).validate(region.len).ok()
}

/// The trust point `header`: a header that passes its checks puts the
/// buckets inside the index, and the parts of a record inside its size.
#[kani::proof]
fn a_header_that_passes_puts_every_bucket_inside_the_index() {
    let region = AnyRegion::new();
    let Some(layout) = any_layout(&region) else {
        return;
    };
    kani::cover!(layout.nbuckets > 1, "a header of more than one bucket");
    assert!(layout.region_len <= region.len);
    assert!((1..=MAX_KEYS).contains(&layout.nkeys));
    assert!(layout.record_size.is_multiple_of(8));
    assert!(RECORD_HEADER + layout.nkeys * KEY_SLOT + layout.payload_size <= layout.record_size);
    // The bucket of any hash, which the model asserts inside the index.
    Access::new(&region, &layout).head(kani::any());
}

/// The trust point `record-count`: the count a walk is bounded by is
/// at most the places the chunks have, whatever the header says.
#[kani::proof]
fn the_record_count_is_bounded_by_the_chunks() {
    let region = AnyRegion::new();
    let Some(layout) = any_layout(&region) else {
        return;
    };
    let access = Access::new(&region, &layout);
    kani::cover!(access.records() > 0, "a count of records");
    assert!(access.records() <= (region.nchunks as u64) << UNIT_BITS);
}

/// The trust points `bucket-head`, `next` and `record`: a reference that
/// passes its check names a record inside its chunk.
#[kani::proof]
fn a_reference_that_passes_names_a_record_inside_its_chunk() {
    let region = AnyRegion::new();
    let Some(layout) = any_layout(&region) else {
        return;
    };
    let access = Access::new(&region, &layout);
    if let Ok(Place { chunk, byte }) = access.place(kani::any(), Kind::Damaged) {
        kani::cover!(chunk > 0, "a record past the first chunk");
        assert!(chunk < region.nchunks);
        assert!(byte >= CHUNK_HEADER && byte.is_multiple_of(8));
        assert!(byte + layout.record_size <= region.lens[chunk]);
    }
}

/// The trust point `used-mark`: a used mark that passes its check ends a
/// record inside its chunk, and the room it leaves is the chunk's rest.
#[kani::proof]
fn a_used_mark_that_passes_ends_a_record() {
    let region = AnyRegion::new();
    let Some(layout) = any_layout(&region) else {
        return;
    };
    let access = Access::new(&region, &layout);
    let chunk = kani::any();
    if let Ok((used, room)) = access.room(chunk) {
        let len = region.lens[chunk];
        let size = layout.record_size;
        kani::cover!(used > CHUNK_HEADER && room > 0, "a chunk part used");
        assert!(used >= CHUNK_HEADER && used <= len);
        assert!((used - CHUNK_HEADER).is_multiple_of(size));
        assert!(used + room * size <= len && len - used - room * size < size);
    }
}

/// The trust point `used-mark` in a walk over a chunk: every record from
/// a place that passes up to the used mark lies inside the chunk. The
/// walk rises by the record size, so its first and last records bound
/// all the others; both are found without a loop, which Kani would
/// unwind once for every record. The mark itself is the proof above.
///
/// The walk is proved over chunks of up to [`STORE`] bytes, for any
/// record size: that the walk's last record ends at the used mark rests
/// on a remainder of their difference, which a solver of bits proves
/// only for short numbers.
#[kani::proof]
fn a_walk_over_a_chunk_stays_inside_it() {
    let region = AnyRegion::new();
    for chunk in 0..CHUNKS {
        kani::assume(region.lens[chunk] <= STORE);
        kani::assume(region.used[chunk] <= STORE as u64);
    }
    let Some(layout) = any_layout(&region) else {
        return;
    };
    let access = Access::new(&region, &layout);
    let chunk = kani::any();
    let from = kani::any_where(|&from| from <= STORE);
    if let Ok(records) = unlinked(&access, chunk, from) {
        if let (Some(first), Some(last)) = (records.clone().next(), records.clone().next_back()) {
            kani::cover!(last > first, "a walk of more than one record");
            assert!(first >= CHUNK_HEADER);
            assert!(last + layout.record_size <= region.lens[chunk]);
        }
    }
}

/// A walk down a chain ends within the record count, whatever the bytes
/// of the chunk: a chunk with room for three records, its bytes any, and
/// a count up to four, past which the walk is the same.
#[kani::proof]
#[kani::unwind(8)]
fn a_walk_down_a_chain_ends_within_the_record_count() {
    let mut region = AnyRegion::new();
    // An index as long as `Table::attach` takes.
    kani::assume(check_region(NonNull::<u64>::dangling().as_ptr().cast(), region.len).is_ok());
    region.nchunks = 1;
    region.lens[0] = STORE;
    region.most_records = 4;
    let layout = Layout {
        region_len: kani::any(),
        buckets_offset: kani::any(),
        nbuckets: kani::any(),
        bucket_shift: kani::any(),
        record_size: WALK_RECORD,
        payload_size: 0,
        nkeys: 1,
    };
    let mut access = Access::new(&region, &layout);
    let found = access.find(kani::any(), kani::any(), |_| kani::any());
    kani::cover!(found.is_ok_and(|offset| offset != 0), "a record found");
    kani::cover!(found.is_err(), "a chain refused");
}

/// A partition of any hash lies within the partitions, for any count and
/// shift that pass: a power of two up to [`MAX_PARTITIONS`].
#[kani::proof]
fn a_partition_lies_within_the_partitions() {
    let bits: u32 = kani::any_where(|&bits| bits <= MAX_PARTITIONS.trailing_zeros());
    let shift: u32 = kani::any_where(|&shift| shift < 32 && shift + bits <= 32);
    let count = 1_u32 << bits;
    assert!(partition_of(kani::any(), shift, count - 1) < count);
}

/// The partitions that pass name chunks the call was given, and give
/// their count less one as the mask: up to four partitions, as the check
/// of the count and the shift does not depend on the chunks.
#[kani::proof]
#[kani::unwind(5)]
fn the_partitions_that_pass_name_chunks_of_the_table() {
    let region = AnyRegion::new();
    let chunks: [u32; 4] = kani::any();
    let count = kani::any_where(|&count| count <= 4);
    let partitions = Partitions {
        shift: kani::any(),
        chunks: &chunks[..count],
    };
    if let Ok(mask) = check_partitions(&region, &partitions) {
        kani::cover!(count == 4, "four partitions");
        assert!(mask as usize + 1 == count && count.is_power_of_two());
        let partition = partition_of(kani::any(), partitions.shift, mask) as usize;
        assert!((partitions.chunks[partition] as usize) < region.nchunks);
    }
}

/// The word of any hash lies within a filter of any size that passes.
#[kani::proof]
fn a_word_of_the_filter_lies_within_it() {
    let nwords = kani::any();
    if let Ok(shift) = shift_for(nwords) {
        kani::cover!(nwords > 1, "a filter of more than one word");
        assert!(place(kani::any(), shift).0 < nwords);
    }
}

/// The words of a filter within a limit are a power of two, within it
/// unless one word is not.
#[kani::proof]
#[kani::unwind(66)]
fn the_words_of_a_filter_within_a_limit_are_a_filter() {
    let limit: usize = kani::any();
    if let Ok(words) = words_within(kani::any(), limit) {
        kani::cover!(words > 1, "a filter of more than one word");
        assert!(words.is_power_of_two());
        assert!(words == 1 || words * 8 <= limit / 8);
    }
}

/// The marks a reference sets: the record it names, in a word within the
/// chunk's marks.
struct MarkOf {
    len: usize,
    record_size: usize,
    reference: u32,
}

impl Marks for MarkOf {
    fn set(&self, chunk: usize, word: usize, bit: u64) {
        let Place { byte, .. } = Place::of(self.reference);
        kani::cover!(word > 0, "a mark past the first word");
        assert!(chunk == 0);
        assert!(word < mark_words(self.len, self.record_size).unwrap());
        assert!(bit.is_power_of_two());
        let record = word * 64 + bit.trailing_zeros() as usize;
        assert!(record * self.record_size + CHUNK_HEADER == byte);
    }

    fn word(&self, _: usize, _: usize) -> u64 {
        panic!("a mark reads no word")
    }
}

/// A reference that passes the check of a mark sets the bit of its own
/// record, within the chunk's words: the multiply that stands for the
/// division by the record size is exact.
#[kani::proof]
fn a_mark_sets_the_bit_of_its_record() {
    let len = kani::any_where(|&len| len <= MAX_CHUNK_LEN);
    let record_size = kani::any();
    let reference = kani::any();
    let rows = [1];
    let _ = mark(
        &[len],
        record_size,
        &[reference],
        &RowMaskView::try_new(1, &rows).unwrap(),
        &MarkOf {
            len,
            record_size,
            reference,
        },
    );
}

/// The words of marks a walk reads: any, within the chunk's `words`.
struct WordsOf {
    words: usize,
}

impl Marks for WordsOf {
    fn set(&self, _: usize, _: usize, _: u64) {
        panic!("a walk sets no mark")
    }

    fn word(&self, chunk: usize, word: usize) -> u64 {
        kani::cover!(word > 0, "a word past the first");
        assert!(chunk == 0 && word < self.words);
        kani::any()
    }
}

/// The walk over the records without a mark reads words within the
/// chunk's marks, as [`mark_words`] counts them, for any used mark,
/// record size and cursor, over a chunk of up to two words of marks: past
/// them, a word repeats the same arithmetic. A used mark or a record size
/// past the chunk's length is refused before any arithmetic on it. Each
/// loop takes at most two turns here: over the one chunk, over its two
/// words, and over a word's bits, as the walk stops at its second record.
#[kani::proof]
#[kani::unwind(3)]
fn a_walk_over_marks_reads_words_within_them() {
    const LEN: usize = CHUNK_HEADER + 65 * 24;
    #[repr(C, align(8))]
    struct Chunk([u8; LEN]);
    // Of the chunk's bytes, the walk reads only its used mark.
    let mut memory = Chunk([0; LEN]);
    let used = kani::any_where(|&used: &u64| used <= LEN as u64);
    memory.0[..8].copy_from_slice(&used.to_ne_bytes());
    let len = kani::any_where(|&len| len <= LEN);
    let bases = [memory.0.as_mut_ptr()];
    let lens = [len];
    // SAFETY: one chunk of `len` bytes, aligned to 8, that nothing else
    // reaches while the walk runs.
    let Ok(chunks) = (unsafe { Chunks::new(&bases, &lens) }) else {
        return;
    };
    let record_size = kani::any_where(|&size| size <= LEN);
    // A size the walk refuses has no words of marks.
    let Ok(words) = mark_words(len, record_size) else {
        return;
    };
    let mut cursor = Cursor::at(kani::any_where(|&chunk| chunk <= 1), kani::any());
    let mut out = [0; 1];
    let _ = scan_unmarked(
        &chunks,
        record_size,
        &mut cursor,
        Some(&WordsOf { words }),
        &mut out,
    );
}

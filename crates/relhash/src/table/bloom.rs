//! A Bloom filter of a table's keys, which a join checks a batch of probe
//! rows against before it looks them up: a row whose bits are not all in
//! the filter has no record with its hash, and leaves the batch without
//! reading a bucket or a record.
//!
//! The filter is words of 64 bits. Each hash picks one word and four
//! bits in it, so a check is one load and a compare:
//!
//! ```text
//!   hash × SPREAD ──► a product of 64 bits
//!                     ├─ its high bits ──────► the word
//!                     └─ its low 24 bits ────► four bit numbers, 6 bits each
//!
//!   word  …0 1 0 0 1 0 0 0 0 1 0 1 0 0…   the four bits of a hash
//!            ▲     ▲         ▲   ▲        all 1: the row may have a record
//! ```
//!
//! The hash is the table's own, every key folded in. The multiplication
//! spreads it, so that the word does not repeat the high bits that pick
//! the bucket. A filter holds [`BITS_PER_RECORD`] bits per record of the
//! table it was filled from, in a power of two of words; with four bits
//! per key that lets through about one absent key in 200, as a test
//! measures at exactly 16 bits.
//!
//! Like the table, the filter is a block of words the caller owns, with
//! no address inside, so the same code serves a filter in a process's
//! memory and one in shared memory next to a shared table.
//!
//! A shared filter ([`SharedFilter`]) starts with a word of its state:
//!
//! ```text
//!   NONE ──claim──► BUILDING ──fill, then publish──► READY
//! ```
//!
//! Each participant decides by its own batches whether it wants the
//! filter. The first that does claims it with a compare-and-swap, fills
//! it alone, and publishes READY with release. Every participant checks a
//! batch against the filter only once it reads READY with acquire, and
//! probes the table without it until then. Nobody waits, and a
//! participant that never wants the filter leaves nothing undone. The
//! loom models in `loom/filter.rs` check this protocol.
//!
//! ```
//! use relhash::{RowMask, RowMaskView};
//! use relhash::bloom;
//!
//! // A filter for 100 records, filled with the hashes of two rows.
//! let mut words = vec![0; bloom::words_for(100)?];
//! bloom::add(&mut words, &[7, 8], &RowMaskView::try_new(2, &[1 << 0 | 1 << 1])?)?;
//! // Probe three rows of hashes 7, 8 and 9.
//! let mut passed = [0];
//! let rows = RowMaskView::try_new(3, &[1 << 0 | 1 << 1 | 1 << 2])?;
//! bloom::probe(&words, &[7, 8, 9], &rows, &mut RowMask::try_new(3, &mut passed)?)?;
//! // Rows 0 and 1, hashes 7 and 8, always pass. Row 2 may pass too: a
//! // filter lets an absent hash through now and then.
//! let passed = RowMaskView::try_new(3, &passed)?;
//! assert!(passed.contains(0)? && passed.contains(1)?);
//! # Ok::<(), anyhow::Error>(())
//! ```

use core::sync::atomic::AtomicU64;

use crate::{RowMask, RowMaskView, check_rows, ones};
use anyhow::{Result, bail, ensure};

use super::header::{CHUNK_HEADER, Layout, Place};
use super::record::{Access, check_record, unlinked};
use super::region::{Region, order};

/// The words of a filter as a check reads them.
pub(super) trait FilterRead {
    /// The number of words, without a shared filter's state.
    fn nwords(&self) -> usize;

    /// Word `word`, which is below [`Self::nwords`].
    fn load(&self, word: usize) -> u64;
}

impl FilterRead for [u64] {
    #[inline(always)]
    fn nwords(&self) -> usize {
        self.len()
    }

    #[inline(always)]
    fn load(&self, word: usize) -> u64 {
        self[word]
    }
}

/// The words of a filter several participants add to at once.
pub(super) trait FilterOr {
    /// Set bits of word `word`, as one atomic step.
    fn or(&self, word: usize, mask: u64);
}

/// A filter several participants share, behind its state word.
pub(super) trait FilterShared: FilterRead + FilterOr {
    /// Take the building of the filter: true for the one participant
    /// that moved the state from none to building.
    fn claim(&self) -> bool;

    /// Mark the filled filter ready.
    fn publish(&self);

    /// Whether the filter is ready, and its words complete.
    fn ready(&self) -> bool;
}

/// The states of a shared filter.
pub(super) const NONE: u64 = 0;
pub(super) const BUILDING: u64 = 1;
pub(super) const READY: u64 = 2;

/// A filter in memory several participants map: a state word, then a
/// power of two of words, [`shared_words_for`] in all.
#[derive(Debug)]
pub struct SharedFilter<'a> {
    state: &'a AtomicU64,
    words: &'a [AtomicU64],
}

impl<'a> SharedFilter<'a> {
    /// Attach to the `nwords` words at `words`, the state word first.
    ///
    /// # Safety
    ///
    /// `words` is aligned to 8 and valid for reads and writes of `nwords`
    /// words for `'a`, and during `'a` the words are accessed only through
    /// shared filters, here or in other processes mapping the same memory.
    pub unsafe fn attach(words: *mut u64, nwords: usize) -> Result<Self> {
        ensure!(
            !words.is_null() && words.addr().is_multiple_of(8),
            "a shared filter must be aligned to 8 bytes"
        );
        ensure!(
            nwords >= 2 && (nwords - 1).is_power_of_two(),
            "a shared filter has a state word and a power of two of words, not {nwords} words"
        );
        // SAFETY: the caller's contract; an `AtomicU64` has the size and
        // alignment of a `u64`, and every access goes through atomics.
        let all = unsafe { core::slice::from_raw_parts(words.cast::<AtomicU64>(), nwords) };
        let Some((state, words)) = all.split_first() else {
            bail!("a shared filter without its state word");
        };
        Ok(Self { state, words })
    }

    /// A shared filter over words this process borrows, for participants
    /// that are threads.
    pub fn from_mut(words: &'a mut [u64]) -> Result<Self> {
        // SAFETY: the words are borrowed exclusively for `'a`, so nothing
        // but this filter accesses them meanwhile.
        unsafe { Self::attach(words.as_mut_ptr(), words.len()) }
    }

    /// Clear the filter and its state, before any participant uses it:
    /// the caller has it to itself.
    pub fn init(&self) {
        for word in self.words {
            word.store(0, order::RELAXED);
        }
        self.state.store(NONE, order::STORE);
    }

    /// Whether the filter is built: from then on [`probe_shared`] checks
    /// batches against it.
    pub fn ready(&self) -> bool {
        FilterShared::ready(self)
    }
}

impl FilterRead for SharedFilter<'_> {
    #[inline(always)]
    fn nwords(&self) -> usize {
        self.words.len()
    }

    #[inline(always)]
    fn load(&self, word: usize) -> u64 {
        self.words[word].load(order::RELAXED)
    }
}

impl FilterOr for SharedFilter<'_> {
    #[inline]
    fn or(&self, word: usize, mask: u64) {
        self.words[word].fetch_or(mask, order::RELAXED);
    }
}

impl FilterShared for SharedFilter<'_> {
    #[inline]
    fn claim(&self) -> bool {
        self.state
            .compare_exchange(NONE, BUILDING, order::CAS, order::CAS_FAILED)
            .is_ok()
    }

    #[inline]
    fn publish(&self) {
        self.state.store(READY, order::STORE);
    }

    #[inline]
    fn ready(&self) -> bool {
        self.state.load(order::LOAD) == READY
    }
}

/// Bits of a filter per record of its table.
pub const BITS_PER_RECORD: u64 = 16;

/// The words of a filter without a state word that participants fill at
/// once: an atomic OR a word.
struct AtomicWords<'a>(&'a [AtomicU64]);

impl FilterOr for AtomicWords<'_> {
    #[inline(always)]
    fn or(&self, word: usize, mask: u64) {
        self.0[word].fetch_or(mask, order::RELAXED);
    }
}

/// The odd multiplier that spreads a hash over 64 bits (2^64 / phi).
const SPREAD: u64 = 0x9E37_79B9_7F4A_7C15;

/// The words of a shared filter for a table of `records` records: the
/// state word and [`words_for`] them.
pub fn shared_words_for(records: u64) -> Result<usize> {
    words_for(records)?
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("a filter for {records} records is too large"))
}

/// The words of a filter for a table of `records` records: a power of
/// two, at least one.
pub fn words_for(records: u64) -> Result<usize> {
    let words = records
        .checked_mul(BITS_PER_RECORD)
        .map(|bits| bits.div_ceil(64).max(1))
        .and_then(u64::checked_next_power_of_two)
        .and_then(|words| usize::try_from(words).ok());
    words.ok_or_else(|| anyhow::anyhow!("a filter for {records} records is too large"))
}

/// The words of a filter for a table of `records` records within an
/// eighth of `limit` bytes: [`words_for`] them, halved while larger, one
/// at least. A smaller filter lets more rows through; a larger one would
/// leave a small memory no room for the rows it guards.
pub fn words_within(records: u64, limit: usize) -> Result<usize> {
    let mut words = words_for(records)?;
    while words > 1 && words.saturating_mul(8) > limit / 8 {
        words /= 2;
    }
    Ok(words)
}

/// The shift that takes a word index out of a spread hash, for a filter
/// of `nwords` words, which must be a power of two.
pub(super) fn shift_for(nwords: usize) -> Result<u32> {
    ensure!(
        nwords.is_power_of_two(),
        "a filter has a power of two of words, not {nwords}"
    );
    Ok(64 - nwords.trailing_zeros())
}

/// The word of a hash and the mask of its four bits.
#[inline(always)]
pub(super) fn place(hash: u32, shift: u32) -> (usize, u64) {
    let spread = u64::from(hash).wrapping_mul(SPREAD);
    let word = spread.checked_shr(shift).unwrap_or(0) as usize;
    let mask = (1 << (spread & 63))
        | (1 << ((spread >> 6) & 63))
        | (1 << ((spread >> 12) & 63))
        | (1 << ((spread >> 18) & 63));
    (word, mask)
}

/// Clear the filter and set the bits of every record of the table.
pub(super) fn fill<R: Region>(region: &R, layout: &Layout, words: &mut [u64]) -> Result<()> {
    let shift = shift_for(words.len())?;
    words.fill(0);
    each_hash(region, layout, |hash| {
        let (word, mask) = place(hash, shift);
        words[word] |= mask;
    })
}

/// Call `f` with the hash of every record of the table, chunk by chunk.
#[inline(always)]
fn each_hash<R: Region>(region: &R, layout: &Layout, mut f: impl FnMut(u32)) -> Result<()> {
    let access = Access::new(region, layout);
    for chunk in 0..region.chunks() {
        for byte in unlinked(&access, chunk, CHUNK_HEADER)? {
            f(check_record(&access, Place { chunk, byte })?);
        }
    }
    Ok(())
}

/// Build a shared filter from the table's records unless another
/// participant has claimed it: true for the one that built it. The table
/// takes no insertions meanwhile.
pub(super) fn try_build<R: Region, F: FilterShared + ?Sized>(
    region: &R,
    layout: &Layout,
    filter: &F,
) -> Result<bool> {
    let shift = shift_for(filter.nwords())?;
    if !filter.claim() {
        return Ok(false);
    }
    each_hash(region, layout, |hash| {
        let (word, mask) = place(hash, shift);
        filter.or(word, mask);
    })?;
    filter.publish();
    Ok(true)
}

/// The rows of `rows` whose hash has all its bits in the filter, into
/// `found`, which this call fills whole: the others have no record with
/// their hash. `hashes` holds a hash per row.
///
/// # Errors
///
/// A filter whose length is not a power of two, and hashes or a result of
/// another row count than the mask, fail before any change.
///
/// ```
/// use relhash::{RowMask, RowMaskView};
/// use relhash::bloom;
///
/// // An empty filter lets nothing through.
/// let words = vec![0; bloom::words_for(100)?];
/// let rows = RowMaskView::try_new(2, &[1 << 0 | 1 << 1])?;
/// // The probe overwrites whatever the result held.
/// let mut found = [1 << 0 | 1 << 1];
/// bloom::probe(&words, &[7, 8], &rows, &mut RowMask::try_new(2, &mut found)?)?;
/// assert_eq!(RowMaskView::try_new(2, &found)?.selected_count(), 0);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn probe(
    words: &[u64],
    hashes: &[u32],
    rows: &RowMaskView<'_>,
    found: &mut RowMask<'_>,
) -> Result<()> {
    check(words, hashes, rows, found)
}

/// Set the bits of the hash of every row of `rows` in the filter: for a
/// filter of a table that spills, which sees its rows as they come, some
/// of them never in memory at once. `hashes` holds a hash per row.
///
/// # Errors
///
/// A filter whose length is not a power of two, and hashes of another row
/// count than the mask, fail before any change.
///
/// ```
/// use relhash::{RowMask, RowMaskView};
/// use relhash::bloom;
///
/// // Add rows 0 and 1 of three, then probe all three: the rows added
/// // pass; row 2, not added, most likely not.
/// let mut words = vec![0; bloom::words_for(100)?];
/// bloom::add(&mut words, &[7, 8, 9], &RowMaskView::try_new(3, &[1 << 0 | 1 << 1])?)?;
/// let mut found = [0];
/// let all = RowMaskView::try_new(3, &[1 << 0 | 1 << 1 | 1 << 2])?;
/// bloom::probe(&words, &[7, 8, 9], &all, &mut RowMask::try_new(3, &mut found)?)?;
/// let found = RowMaskView::try_new(3, &found)?;
/// assert!(found.contains(0)? && found.contains(1)?);
/// // A filter's words are a power of two.
/// assert!(bloom::add(&mut words[..3], &[7], &RowMaskView::try_new(1, &[1 << 0])?).is_err());
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn add(words: &mut [u64], hashes: &[u32], rows: &RowMaskView<'_>) -> Result<()> {
    let nrows = rows.nrows();
    let shift = shift_for(words.len())?;
    check_rows(nrows, &[hashes.len()], "Bloom filter")?;
    for index in 0..nrows.div_ceil(64) {
        for bit in ones(rows.word(index)) {
            let (word, mask) = place(hashes[index * 64 + bit], shift);
            words[word] |= mask;
        }
    }
    Ok(())
}

/// As [`add`], for a filter several participants fill at once: each word
/// is changed by an atomic OR (relaxed), and the filter is read with
/// [`probe`] once a barrier ordered every participant's additions before
/// the reads.
///
/// # Safety
///
/// `words` points to `nwords` words, aligned to 8, valid for reads and
/// writes, which every participant accesses only atomically until the
/// barrier.
pub unsafe fn add_atomic(
    words: *mut u64,
    nwords: usize,
    hashes: &[u32],
    rows: &RowMaskView<'_>,
) -> Result<()> {
    let shift = shift_for(nwords)?;
    ensure!(
        !words.is_null() && words.addr().is_multiple_of(8),
        "the words of a filter filled at once must be aligned to 8 bytes"
    );
    // SAFETY: the caller's contract; an `AtomicU64` has the size and
    // alignment of a `u64`.
    let atomics = unsafe { core::slice::from_raw_parts(words.cast::<AtomicU64>(), nwords) };
    add_with(&AtomicWords(atomics), shift, hashes, rows)
}

/// Set the bits of the hash of every row of `rows` by `filter`'s OR, the
/// shift its words take ([`shift_for`]): [`add_atomic`] over any filter
/// that participants fill at once.
#[inline(always)]
pub(super) fn add_with<F: FilterOr + ?Sized>(
    filter: &F,
    shift: u32,
    hashes: &[u32],
    rows: &RowMaskView<'_>,
) -> Result<()> {
    let nrows = rows.nrows();
    check_rows(nrows, &[hashes.len()], "Bloom filter")?;
    for index in 0..nrows.div_ceil(64) {
        for bit in ones(rows.word(index)) {
            let (word, mask) = place(hashes[index * 64 + bit], shift);
            filter.or(word, mask);
        }
    }
    Ok(())
}

/// [`probe`] against a shared filter, which must be ready.
///
/// # Errors
///
/// As [`probe`], and a filter that is not ready yet.
pub fn probe_shared(
    filter: &SharedFilter<'_>,
    hashes: &[u32],
    rows: &RowMaskView<'_>,
    found: &mut RowMask<'_>,
) -> Result<()> {
    probe_ready(filter, hashes, rows, found)
}

/// [`probe_shared`] over any shared filter.
pub(super) fn probe_ready<F: FilterShared + ?Sized>(
    filter: &F,
    hashes: &[u32],
    rows: &RowMaskView<'_>,
    found: &mut RowMask<'_>,
) -> Result<()> {
    ensure!(filter.ready(), "the shared filter is not built yet");
    check(filter, hashes, rows, found)
}

#[inline(always)]
fn check<F: FilterRead + ?Sized>(
    words: &F,
    hashes: &[u32],
    rows: &RowMaskView<'_>,
    found: &mut RowMask<'_>,
) -> Result<()> {
    let nrows = rows.nrows();
    let shift = shift_for(words.nwords())?;
    check_rows(
        nrows,
        &[hashes.len(), found.as_view().nrows()],
        "Bloom filter",
    )?;
    for index in 0..nrows.div_ceil(64) {
        let mut hits = 0;
        for bit in ones(rows.word(index)) {
            let (word, mask) = place(hashes[index * 64 + bit], shift);
            hits |= u64::from(words.load(word) & mask == mask) << bit;
        }
        found.set_word(index, hits)?;
    }
    Ok(())
}

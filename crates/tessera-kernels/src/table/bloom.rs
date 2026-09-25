//! A Bloom filter of a table's keys, which a join checks a batch of probe
//! rows against before it looks them up: a row whose bits are not all in
//! the filter has no record with its hash, and leaves the batch without
//! reading a bucket or a record.
//!
//! The filter is blocked in a register: each hash selects one 64-bit word
//! and four bits in it, so a check is one load and a compare. The hash is
//! the table's own (every key folded in, under the table's NULL policy),
//! spread by a multiplication so that the word does not repeat the bits
//! that choose the bucket: the word comes from the product's high bits,
//! the four bit positions from its low 24 bits, six each. A filter holds
//! [`BITS_PER_RECORD`] bits per record of the table it was filled from, in
//! a power of two of words; with four bits per key that lets through
//! about one absent key in a hundred.
//!
//! Like the table's region, the filter is a borrowed buffer of words that
//! holds no process address, so the same code serves a filter in a
//! backend's memory and one in dynamic shared memory next to a shared
//! table.
//!
//! A shared filter ([`SharedFilter`]) starts with a state word: none,
//! building, ready. Each participant decides by its own batches whether
//! it wants the filter; the first that does claims it with a
//! compare-and-swap, fills it alone and marks it ready with release, and
//! every participant checks a batch against it only once it reads ready
//! with acquire, probing the table without it until then. Nobody waits,
//! and a participant that never wants the filter leaves nothing undone.
//! The loom model in `loom.rs` checks this protocol.

use core::sync::atomic::AtomicU64;

use anyhow::{Result, ensure};
use tessera_core::{RowMask, RowMaskView};

use super::header::{CHUNK_USED, HEADER_SIZE, Layout};
use super::record::Access;
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

/// A filter several participants share, behind its state word.
pub(super) trait FilterShared: FilterRead {
    /// Set bits of word `word`.
    fn or(&self, word: usize, mask: u64);

    /// Take the building of the filter: true for the one participant
    /// that moved the state from none to building.
    fn claim(&self) -> bool;

    /// Mark the filled filter ready.
    fn publish(&self);

    /// Whether the filter is ready, and its words complete.
    fn ready(&self) -> bool;
}

/// The states of a shared filter.
const NONE: u64 = 0;
const BUILDING: u64 = 1;
const READY: u64 = 2;

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
        let (state, words) = all.split_first().unwrap();
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

impl FilterShared for SharedFilter<'_> {
    #[inline]
    fn or(&self, word: usize, mask: u64) {
        self.words[word].fetch_or(mask, order::RELAXED);
    }

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

/// The shift that takes a word index out of a spread hash, for a filter
/// of `words` words, which must be a power of two.
fn shift_for(nwords: usize) -> Result<u32> {
    ensure!(
        nwords.is_power_of_two(),
        "a filter has a power of two of words, not {nwords}"
    );
    Ok(64 - nwords.trailing_zeros())
}

/// The word of a hash and the mask of its four bits.
#[inline(always)]
fn place(hash: u32, shift: u32) -> (usize, u64) {
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

/// Call `f` with the hash of every record of the table.
#[inline(always)]
fn each_hash<R: Region>(region: &R, layout: &Layout, mut f: impl FnMut(u32)) -> Result<()> {
    let used = region.load_u64(CHUNK_USED) as usize;
    let mut access = Access::new(region, layout);
    let mut byte = HEADER_SIZE;
    while byte < used {
        let record = access.locate((byte / 8) as u32)?;
        f(record.hash());
        byte += record.len();
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
/// use tessera_core::{RowMask, RowMaskView};
/// use tessera_kernels::table::bloom;
///
/// // An empty filter lets nothing through.
/// let words = vec![0; bloom::words_for(100)?];
/// let rows = RowMaskView::try_new(2, &[0b11])?;
/// let mut found = [0b11];
/// bloom::probe(&words, &[7, 8], &rows, &mut RowMask::try_new(2, &mut found)?)?;
/// assert_eq!(found, [0]);
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
    ensure!(
        hashes.len() == nrows && found.as_view().nrows() == nrows,
        "the hashes, mask and result of the batch have different row counts"
    );
    for index in 0..nrows.div_ceil(64) {
        let mut bits = rows.word(index).unwrap();
        let mut hits = 0;
        while bits != 0 {
            let bit = bits.trailing_zeros();
            bits &= bits - 1;
            let (word, mask) = place(hashes[index * 64 + bit as usize], shift);
            hits |= u64::from(words.load(word) & mask == mask) << bit;
        }
        found.set_word(index, hits)?;
    }
    Ok(())
}

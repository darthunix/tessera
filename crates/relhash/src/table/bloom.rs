//! A Bloom filter of a table's keys, which a join checks a batch of probe
//! rows against before it looks them up: a row whose bits are not all in
//! the filter has no record with its hash, and leaves the batch without
//! reading a bucket or a record.
//!
//! The filter is a power of two of 64-bit words, which the caller owns
//! and which hold no address, so the same code serves a filter in a
//! process's memory and one in shared memory next to a shared table. The
//! hash of a row is the one the caller gives the table, every key folded
//! in. It sets four bits of one word, taken from the hash multiplied by
//! an odd constant: the word from the product's high bits, the four bits
//! from its low 24 bits, six bits each. The four bits may coincide, and a
//! check is one load and a compare:
//!
//! ```text
//!  s = hash × 0x9E3779B97F4A7C15, modulo 2^64; k = log2(words)
//!
//!  bit 63                        24 23    18 17    12 11     6 5      0
//!     ┌───────────────────────────┬────────┬────────┬────────┬────────┐
//!     │ the word: the top k bits  │ bit 4  │ bit 3  │ bit 2  │ bit 1  │
//!     │ (word 0 when k is 0)      │        │        │        │        │
//!     └───────────────────────────┴────────┴────────┴────────┴────────┘
//! ```
//!
//! The multiplication spreads the hash, so that the word does not repeat
//! the high bits that pick the bucket. A row passes when every bit of its
//! hash is set, so a row with the hash of a record always passes; at the
//! 16 bits a record of [`words_for`], fewer than one in 100 absent keys
//! pass too.
//!
//! A [`SharedFilter`] is built once for all the participants of a shared
//! table: the first that wants it builds it, the others probe the table
//! without it until it is ready, and nobody waits. [`add_atomic`] lets
//! several participants fill one filter at once instead.
//!
//! ```
//! use relhash::{RowMask, RowMaskView};
//! use relhash::bloom;
//!
//! // A filter for 100 records, filled with the hashes of two rows.
//! let mut words = vec![0; bloom::words_for(100)?];
//! bloom::add(&mut words, &[7, 8], &RowMaskView::try_new(2, &[1 << 0 | 1 << 1])?)?;
//! // Probe three rows of hashes 7, 8 and 9: the filter keeps the rows
//! // that may have a record.
//! let mut words_of_rows = [1 << 0 | 1 << 1 | 1 << 2];
//! let mut rows = RowMask::try_new(3, &mut words_of_rows)?;
//! bloom::probe(&words, &[7, 8, 9], &mut rows)?;
//! // Rows 0 and 1, hashes 7 and 8, always pass. Row 2 may pass too: a
//! // filter lets an absent hash through now and then.
//! assert!(rows.as_view().contains(0)? && rows.as_view().contains(1)?);
//! # Ok::<(), relhash::Error>(())
//! ```

use core::sync::atomic::AtomicU64;

use crate::error::{Error, Result, Rule, ensure};
use crate::{RowMask, RowMaskView, check_rows, ones};

use super::header::{CHUNK_HEADER, Layout};
use super::record::{Access, records};
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
/// power of two of words, `1 + 2^k` words aligned to 8,
/// [`shared_words_for`] in all.
///
/// ```text
///  word   0         1         2                    2^k
///        ┌─────────┬─────────┬─────────┬─────────┬─────────┐
///        │ state   │ word 0  │ word 1  │    …    │ word    │
///        │         │ of the filter     │         │ 2^k - 1 │
///        └─────────┴─────────┴─────────┴─────────┴─────────┘
///  state: 0 none, 1 building, 2 ready
/// ```
///
/// One participant calls [`Self::init`] before any other uses the filter.
/// Then [`Table::try_build_bloom`](crate::Table::try_build_bloom) lets
/// one participant build it: the one whose compare-and-swap moves the
/// state from none to building fills the filter from the table's records
/// and stores ready with release, and its call returns true. The others
/// return false at once, and nobody waits. [`Self::ready`] reads the
/// state with acquire, so a participant that reads ready sees every bit
/// of the filter, and [`probe_shared`] refuses a filter that is not ready.
/// A build that fails, at a damaged record, leaves the state building: no
/// participant ever reads ready, and each probes the table without the
/// filter.
///
/// It holds only atomic words, so threads may share it.
#[derive(Debug)]
pub struct SharedFilter<'a> {
    state: &'a AtomicU64,
    words: &'a [AtomicU64],
}

impl<'a> SharedFilter<'a> {
    /// Attach to the `nwords` words at `words`, the state word first.
    /// Words that are not aligned to 8, or whose count less the state
    /// word is not a power of two, are refused.
    ///
    /// # Safety
    ///
    /// `words` is aligned to 8 and valid for reads and writes of `nwords`
    /// words for `'a`, and during `'a` the words are accessed only through
    /// shared filters, here or in other processes mapping the same memory.
    pub unsafe fn attach(words: *mut u64, nwords: usize) -> Result<Self> {
        ensure!(
            !words.is_null() && words.addr().is_multiple_of(8),
            Error::wrong_call(Rule::FilterAlign)
        );
        ensure!(
            nwords >= 2 && (nwords - 1).is_power_of_two(),
            Error::wrong_call(Rule::FilterWords).with(nwords)
        );
        // SAFETY: the caller's contract; an `AtomicU64` has the size and
        // alignment of a `u64`, and every access goes through atomics.
        let all = unsafe { core::slice::from_raw_parts(words.cast::<AtomicU64>(), nwords) };
        let Some((state, words)) = all.split_first() else {
            return Err(Error::wrong_call(Rule::FilterState));
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
const BITS_PER_RECORD: u64 = 16;

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
        .ok_or(Error::wrong_call(Rule::FilterRecords).with(records))
}

/// The words of a filter for a table of `records` records: the least
/// power of two that is at least `max(1, ceil(16 * records / 64))`, so
/// 16 bits a record at least.
///
/// # Errors
///
/// A count of records whose filter cannot be sized.
pub fn words_for(records: u64) -> Result<usize> {
    let words = records
        .checked_mul(BITS_PER_RECORD)
        .map(|bits| bits.div_ceil(64).max(1))
        .and_then(u64::checked_next_power_of_two)
        .and_then(|words| usize::try_from(words).ok());
    words.ok_or(Error::wrong_call(Rule::FilterRecords).with(records))
}

/// The words of a filter for a table of `records` records within an
/// eighth of `limit` bytes: [`words_for`] them, halved while their bytes
/// are more than an eighth of `limit`, one at least. A smaller filter
/// lets more rows through; a larger one would leave a small memory no
/// room for the rows it guards.
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
        Error::wrong_call(Rule::FilterSize).with(nwords)
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
    let access = Access::new(region, layout);
    access.check_chunks()?;
    words.fill(0);
    each_hash(&access, region.chunks(), |hash| {
        let (word, mask) = place(hash, shift);
        words[word] |= mask;
    })
}

/// Call `f` with the hash of every record of the table's `chunks` chunks,
/// chunk by chunk.
#[inline(always)]
fn each_hash<R: Region>(
    access: &Access<'_, R>,
    chunks: usize,
    mut f: impl FnMut(u32),
) -> Result<()> {
    for chunk in 0..chunks {
        for record in records(access, chunk, CHUNK_HEADER)? {
            f(record?.1);
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
    let access = Access::new(region, layout);
    access.check_chunks()?;
    if !filter.claim() {
        return Ok(false);
    }
    each_hash(&access, region.chunks(), |hash| {
        let (word, mask) = place(hash, shift);
        filter.or(word, mask);
    })?;
    filter.publish();
    Ok(true)
}

/// Keep in `rows` the rows whose hash has all its bits in the filter: the
/// others have no record with their hash, and leave. `hashes` holds a
/// hash per row. The count of rows left is returned. A lookup narrows its
/// rows so before the table's probe, through
/// [`Matches::rows_mut`](crate::Matches::rows_mut).
///
/// # Errors
///
/// A filter whose length is not a power of two, and hashes of another row
/// count than the mask, fail before any change.
///
/// ```
/// use relhash::{RowMask, bloom};
///
/// // An empty filter lets no row through.
/// let words = vec![0; bloom::words_for(100)?];
/// let mut rows = [1 << 0 | 1 << 1];
/// assert_eq!(bloom::probe(&words, &[7, 8], &mut RowMask::try_new(2, &mut rows)?)?, 0);
/// # Ok::<(), relhash::Error>(())
/// ```
pub fn probe(words: &[u64], hashes: &[u32], rows: &mut RowMask<'_>) -> Result<usize> {
    check(words, hashes, rows)
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
/// let mut all = [1 << 0 | 1 << 1 | 1 << 2];
/// let mut rows = RowMask::try_new(3, &mut all)?;
/// bloom::probe(&words, &[7, 8, 9], &mut rows)?;
/// assert!(rows.as_view().contains(0)? && rows.as_view().contains(1)?);
/// // A filter's words are a power of two.
/// assert!(bloom::add(&mut words[..3], &[7], &RowMaskView::try_new(1, &[1 << 0])?).is_err());
/// # Ok::<(), relhash::Error>(())
/// ```
pub fn add(words: &mut [u64], hashes: &[u32], rows: &RowMaskView<'_>) -> Result<()> {
    let nrows = rows.nrows();
    let shift = shift_for(words.len())?;
    check_rows(nrows, &[hashes.len()])?;
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
        Error::wrong_call(Rule::FilterFillAlign)
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
    check_rows(nrows, &[hashes.len()])?;
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
    rows: &mut RowMask<'_>,
) -> Result<usize> {
    probe_ready(filter, hashes, rows)
}

/// [`probe_shared`] over any shared filter.
pub(super) fn probe_ready<F: FilterShared + ?Sized>(
    filter: &F,
    hashes: &[u32],
    rows: &mut RowMask<'_>,
) -> Result<usize> {
    ensure!(filter.ready(), Error::wrong_call(Rule::FilterNotBuilt));
    check(filter, hashes, rows)
}

#[inline(always)]
fn check<F: FilterRead + ?Sized>(
    words: &F,
    hashes: &[u32],
    rows: &mut RowMask<'_>,
) -> Result<usize> {
    let nrows = rows.as_view().nrows();
    let shift = shift_for(words.nwords())?;
    check_rows(nrows, &[hashes.len()])?;
    let mut left = 0;
    for (index, selected) in rows.words_mut().iter_mut().enumerate() {
        let mut hits = 0;
        for bit in ones(*selected) {
            let (word, mask) = place(hashes[index * 64 + bit], shift);
            hits |= u64::from(words.load(word) & mask == mask) << bit;
        }
        *selected = hits;
        left += hits.count_ones() as usize;
    }
    Ok(left)
}

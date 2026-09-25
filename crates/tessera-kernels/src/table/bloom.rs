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
//! backend's memory and, later, one in dynamic shared memory next to a
//! shared table.

use anyhow::{Result, ensure};
use tessera_core::{RowMask, RowMaskView};

use super::header::{CHUNK_USED, HEADER_SIZE, Layout};
use super::record::Access;
use super::region::Region;

/// Bits of a filter per record of its table.
pub const BITS_PER_RECORD: u64 = 16;

/// The odd multiplier that spreads a hash over 64 bits (2^64 / phi).
const SPREAD: u64 = 0x9E37_79B9_7F4A_7C15;

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
fn shift_for(words: &[u64]) -> Result<u32> {
    ensure!(
        !words.is_empty() && words.len().is_power_of_two(),
        "a filter has a power of two of words, not {}",
        words.len()
    );
    Ok(64 - words.len().trailing_zeros())
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
    let shift = shift_for(words)?;
    words.fill(0);
    let used = region.load_u64(CHUNK_USED) as usize;
    let mut access = Access::new(region, layout);
    let mut byte = HEADER_SIZE;
    while byte < used {
        let record = access.locate((byte / 8) as u32)?;
        let (word, mask) = place(record.hash(), shift);
        words[word] |= mask;
        byte += record.len();
    }
    Ok(())
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
    let nrows = rows.nrows();
    let shift = shift_for(words)?;
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
            hits |= u64::from(words[word] & mask == mask) << bit;
        }
        found.set_word(index, hits)?;
    }
    Ok(())
}

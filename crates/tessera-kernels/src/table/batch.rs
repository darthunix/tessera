//! The batch operations: appending rows to a chunk, linking a chunk's
//! records into the buckets, probing, the next match of a row (in any
//! table, or within a group of a grouped one) and the gathering of a
//! payload word.

use core::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{RowMask, RowMaskView};

use super::header::{CHUNK_HEADER, Layout, RECORD_HEADER};
use super::keys::{KeySource, WordKeys, slot_buffer};
use super::record::no_chunk;
use super::record::{Access, PayloadColumns, Place, same_keys};
use super::region::Region;
use super::{Appended, Batch, MAX_PARTITIONS, Partitions, Split};

/// Call `$f` specialized for the common shapes of a table: one or two
/// keys, one or two words after them; 0 stands for any other count, read
/// from the layout at run time. The last parameter is the length of the
/// key slot buffer: the key count, or the maximum for any count.
macro_rules! shaped {
    ($nkeys:expr, $tail:expr, $f:ident($($arg:expr),* $(,)?)) => {
        match ($nkeys, $tail) {
            (1, 1) => $f::<_, _, 1, 1, 1>($($arg),*),
            (1, 2) => $f::<_, _, 1, 2, 1>($($arg),*),
            (1, _) => $f::<_, _, 1, 0, 1>($($arg),*),
            (2, 1) => $f::<_, _, 2, 1, 2>($($arg),*),
            (2, 2) => $f::<_, _, 2, 2, 2>($($arg),*),
            (2, _) => $f::<_, _, 2, 0, 2>($($arg),*),
            _ => $f::<_, _, 0, 0, { $crate::table::MAX_KEYS }>($($arg),*),
        }
    };
}
pub(super) use shaped;

/// Reject a batch whose keys or buffers do not match the table and the
/// mask, before anything is read or changed.
pub(super) fn check<K: KeySource + ?Sized>(
    layout: &Layout,
    keys: &K,
    nrows: usize,
    hashes: usize,
    out: usize,
) -> Result<()> {
    check_keys(layout, keys)?;
    ensure!(
        keys.nrows() == nrows && hashes == nrows && out == nrows,
        "the keys, hashes, mask and offsets of the batch have different row counts"
    );
    Ok(())
}

/// Reject keys that are not the table's, before anything is read or
/// changed; a [`Batch`] checked its row counts when it was made.
pub(super) fn check_keys<K: KeySource + ?Sized>(layout: &Layout, keys: &K) -> Result<()> {
    ensure!(
        keys.nkeys() == layout.nkeys,
        "the table has {} keys, the batch {}",
        layout.nkeys,
        keys.nkeys()
    );
    Ok(())
}

/// Check that `payload` holds `payload_size` bytes for each of `nrows`
/// rows.
fn check_payload(payload: Option<&[u8]>, nrows: usize, payload_size: usize) -> Result<()> {
    if let Some(payload) = payload {
        ensure!(
            nrows.checked_mul(payload_size) == Some(payload.len()),
            "the payload has {} bytes, not {payload_size} per row of {nrows}",
            payload.len()
        );
    }
    Ok(())
}

/// Append the pending rows of `batch`, in row order, as records to chunk
/// `chunk`, whose one writer the caller is, as long as whole records fit:
/// appended rows leave the pending rows and get their references, and the
/// chunk's used mark moves past them. The records are not linked into the
/// buckets: [`link`] does that. The count appended is returned; rows left
/// pending need another chunk.
pub(super) fn append<R: Region, K: KeySource + ?Sized>(
    region: &R,
    layout: &Layout,
    chunk: usize,
    payload: Option<&[u8]>,
    batch: &mut Batch<'_, '_, K>,
) -> Result<usize> {
    check_keys(layout, batch.keys)?;
    check_payload(payload, batch.offsets.len(), layout.payload_size)?;
    let Batch {
        hashes,
        keys,
        pending,
        offsets,
    } = batch;
    shaped!(
        layout.nkeys,
        layout.tail_words(),
        append_rows(
            region, layout, chunk, hashes, *keys, payload, pending, offsets
        )
    )
}

/// The rows of [`append`] for a table of `N` keys and `T` words after
/// them, 0 for either when it is not one of the specialized shapes. The
/// row functions take the batch's parts one by one, as their loops keep
/// each in a register.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn append_rows<R: Region, K: KeySource + ?Sized, const N: usize, const T: usize, const L: usize>(
    region: &R,
    layout: &Layout,
    chunk: usize,
    hashes: &[u32],
    keys: &K,
    payload: Option<&[u8]>,
    pending: &mut RowMask<'_>,
    offsets: &mut [u32],
) -> Result<usize> {
    // Made here, not passed in, so that its fields stay in registers; the
    // index is not read.
    let access = Access::for_chunks(region, layout);
    let (mut used, mut room) = access.room(chunk)?;
    let record_size = access.record_size();
    let nrows = pending.as_view().nrows();
    let payload_size = access.payload_size();
    let mut buffer = slot_buffer::<L>();
    let mut word_keys = WordKeys::new(&mut buffer, access.nkeys());
    let mut appended = 0;
    for index in 0..nrows.div_ceil(64) {
        let selected = pending.as_view().word(index).unwrap();
        if selected == 0 {
            continue;
        }
        if room == 0 {
            break;
        }
        word_keys.load(keys, index, selected)?;
        let wanted = selected.count_ones() as usize;
        let count = wanted.min(room);
        let mut bits = selected;
        let mut done = 0;
        for _ in 0..count {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let row = index * 64 + bit;
            // SAFETY: the payload was checked to hold `payload_size` bytes
            // for each of the `nrows` rows, and `row` is below `nrows`.
            let row_payload = payload.map(|payload| unsafe {
                payload.get_unchecked(row * payload_size..(row + 1) * payload_size)
            });
            // SAFETY: the place lies past the used mark and within the
            // chunk, as `room` counted; the buffer and the shape are this
            // table's.
            unsafe {
                access.write::<N, T>((chunk, used), hashes[row], &word_keys, bit, row_payload)
            };
            offsets[row] = access.reference((chunk, used));
            used += record_size;
            done |= 1 << bit;
        }
        pending.intersect_word(index, !done)?;
        appended += count;
        room -= count;
        if count < wanted {
            break;
        }
    }
    // SAFETY: the caller is the chunk's one writer, and `used` ends the
    // records just written.
    unsafe { access.set_used(chunk, used) };
    Ok(appended)
}

/// As [`append`], with each row's payload taken from `columns`: a word of
/// its NULL bits, then a word per column. The table's payload must be
/// exactly those words.
pub(super) fn append_columns<R: Region, K: KeySource + ?Sized>(
    region: &R,
    layout: &Layout,
    chunk: usize,
    columns: &PayloadColumns<'_>,
    batch: &mut Batch<'_, '_, K>,
) -> Result<usize> {
    let Batch {
        hashes,
        keys,
        pending,
        offsets,
    } = batch;
    let nrows = offsets.len();
    check_keys(layout, *keys)?;
    ensure!(
        layout.payload_size == 8 * (columns.null_words() + columns.len()),
        "the table's payload has {} bytes, not its words of NULL bits and {} columns",
        layout.payload_size,
        columns.len()
    );
    ensure!(
        columns.nrows() == nrows,
        "the payload columns do not have the batch's {nrows} rows"
    );
    shaped!(
        layout.nkeys,
        layout.tail_words(),
        append_column_rows(
            region, layout, chunk, hashes, *keys, columns, pending, offsets
        )
    )
}

/// The rows of [`append_columns`], shaped as [`append_rows`]; the words
/// after the keys are the columns', so `T` does not matter.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn append_column_rows<
    R: Region,
    K: KeySource + ?Sized,
    const N: usize,
    const T: usize,
    const L: usize,
>(
    region: &R,
    layout: &Layout,
    chunk: usize,
    hashes: &[u32],
    keys: &K,
    columns: &PayloadColumns<'_>,
    pending: &mut RowMask<'_>,
    offsets: &mut [u32],
) -> Result<usize> {
    let access = Access::for_chunks(region, layout);
    let (mut used, mut room) = access.room(chunk)?;
    let record_size = access.record_size();
    let nrows = pending.as_view().nrows();
    let mut buffer = slot_buffer::<L>();
    let mut word_keys = WordKeys::new(&mut buffer, access.nkeys());
    let mut appended = 0;
    for index in 0..nrows.div_ceil(64) {
        let selected = pending.as_view().word(index).unwrap();
        if selected == 0 {
            continue;
        }
        if room == 0 {
            break;
        }
        word_keys.load(keys, index, selected)?;
        let wanted = selected.count_ones() as usize;
        let count = wanted.min(room);
        let mut bits = selected;
        let mut done = 0;
        for _ in 0..count {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let row = index * 64 + bit;
            // SAFETY: the place lies past the used mark and within the
            // chunk, as `room` counted; the buffer and the shape are this
            // table's; the payload is a word and one per column, and `row`
            // is below the columns' row count, `nrows`.
            unsafe {
                access.write_columns::<N>((chunk, used), hashes[row], &word_keys, bit, columns, row)
            };
            offsets[row] = access.reference((chunk, used));
            used += record_size;
            done |= 1 << bit;
        }
        pending.intersect_word(index, !done)?;
        appended += count;
        room -= count;
        if count < wanted {
            break;
        }
    }
    // SAFETY: the caller is the chunk's one writer, and `used` ends the
    // records just written.
    unsafe { access.set_used(chunk, used) };
    Ok(appended)
}

/// Check that every partition's chunk exists and that the partition bits
/// lie within the hash; the mask of a partition number is returned.
pub(super) fn check_partitions<R: Region>(region: &R, partitions: &Partitions<'_>) -> Result<u32> {
    let count = partitions.chunks.len();
    ensure!(
        count.is_power_of_two() && count <= MAX_PARTITIONS,
        "{count} partitions are not a power of two up to {MAX_PARTITIONS}"
    );
    let bits = count.trailing_zeros();
    ensure!(
        partitions.shift < 32 && partitions.shift + bits <= 32,
        "partition bits {} to {} lie past the 32 bits of a hash",
        partitions.shift,
        partitions.shift + bits
    );
    if let Some(&chunk) = partitions
        .chunks
        .iter()
        .find(|&&chunk| chunk as usize >= region.chunks())
    {
        return Err(no_chunk(chunk as usize));
    }
    Ok((count - 1) as u32)
}

/// Append the pending rows of `batch`, in row order, as records each to
/// the chunk of its hash's partition, whose one writer the caller is, as
/// long as whole records fit there, each row's payload taken from
/// `columns` as [`append_columns`] takes it: appended rows leave the
/// pending rows, get their references and count in `appended`; a row whose
/// partition's chunk is full stays pending, and the rows after it go on.
/// The count appended is returned.
pub(super) fn append_partitioned_columns<R: Region, K: KeySource + ?Sized>(
    region: &R,
    layout: &Layout,
    partitions: &Partitions<'_>,
    columns: &PayloadColumns<'_>,
    batch: &mut Batch<'_, '_, K>,
    appended: &mut Appended<'_>,
) -> Result<usize> {
    let Batch {
        hashes,
        keys,
        pending,
        offsets,
    } = batch;
    let Appended { rows, nulls } = appended;
    let nrows = offsets.len();
    check_keys(layout, *keys)?;
    ensure!(
        layout.payload_size == 8 * (1 + columns.len()) && columns.len() <= 64,
        "the table's payload has {} bytes, not a word of NULL bits and {} columns, 64 at most",
        layout.payload_size,
        columns.len()
    );
    ensure!(
        columns.nrows() == nrows,
        "the payload columns do not have the batch's {nrows} rows"
    );
    let mask = check_partitions(region, partitions)?;
    ensure!(
        rows.len() == partitions.chunks.len(),
        "{} row counts for {} partitions",
        rows.len(),
        partitions.chunks.len()
    );
    shaped!(
        layout.nkeys,
        layout.tail_words(),
        append_partitioned_column_rows(
            region, layout, partitions, mask, hashes, *keys, columns, pending, offsets, rows, nulls
        )
    )
}

/// The rows of [`append_partitioned_columns`], shaped as [`append_rows`];
/// the words after the keys are the columns', so `T` does not matter.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn append_partitioned_column_rows<
    R: Region,
    K: KeySource + ?Sized,
    const N: usize,
    const T: usize,
    const L: usize,
>(
    region: &R,
    layout: &Layout,
    partitions: &Partitions<'_>,
    mask: u32,
    hashes: &[u32],
    keys: &K,
    columns: &PayloadColumns<'_>,
    pending: &mut RowMask<'_>,
    offsets: &mut [u32],
    rows: &mut [u64],
    nulls: &mut u64,
) -> Result<usize> {
    let access = Access::for_chunks(region, layout);
    let record_size = access.record_size();
    let nrows = pending.as_view().nrows();
    let shift = partitions.shift;
    let mut buffer = slot_buffer::<L>();
    let mut word_keys = WordKeys::new(&mut buffer, access.nkeys());
    let mut appended = 0;
    let mut seen = 0u64;
    for index in 0..nrows.div_ceil(64) {
        let selected = pending.as_view().word(index).unwrap();
        if selected == 0 {
            continue;
        }
        word_keys.load(keys, index, selected)?;
        let mut bits = selected;
        let mut done = 0;
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let row = index * 64 + bit;
            let partition = ((hashes[row] >> shift) & mask) as usize;
            // Checked: every partition's chunk exists, and has a count.
            let chunk = partitions.chunks[partition] as usize;
            let (used, room) = access.room(chunk)?;
            if room == 0 {
                continue;
            }
            // SAFETY: the place lies past the used mark and within the
            // chunk, as `room` counted; the buffer and the shape are this
            // table's; the payload is a word and one per column, and `row`
            // is below the columns' row count, `nrows`.
            seen |= unsafe {
                access.write_columns::<N>((chunk, used), hashes[row], &word_keys, bit, columns, row)
            };
            offsets[row] = access.reference((chunk, used));
            // SAFETY: the caller is the chunk's one writer, and the record
            // just written ends at the new mark.
            unsafe { access.set_used(chunk, used + record_size) };
            rows[partition] += 1;
            done |= 1 << bit;
            appended += 1;
        }
        pending.intersect_word(index, !done)?;
    }
    *nulls |= seen;
    Ok(appended)
}

/// Copy the records of chunk `source` from byte `*from` on, whole and in
/// order, each to the chunk of its hash's partition, whose one writer the
/// caller is, and move `*from` past them: at most `offsets.len()` of them,
/// the new references into `offsets` and their hashes into `hashes`. The
/// copies are not linked. It stops before a record
/// whose partition's chunk is full, which [`Split::full`] names.
pub(super) fn split<R: Region>(
    region: &R,
    layout: &Layout,
    partitions: &Partitions<'_>,
    source: usize,
    from: &mut usize,
    offsets: &mut [u32],
    hashes: &mut [u32],
) -> Result<Split> {
    let mask = check_partitions(region, partitions)?;
    ensure!(
        offsets.len() == hashes.len(),
        "the offsets and hashes of a split have different lengths"
    );
    ensure!(
        !partitions.chunks.contains(&(source as u32)),
        "table chunk {source} is split into itself"
    );
    let access = Access::for_chunks(region, layout);
    let record_size = access.record_size();
    let places = unlinked(&access, source, *from)?;
    let mut count = 0;
    let mut full = None;
    for byte in places {
        if count == offsets.len() {
            break;
        }
        let hash = check_record(&access, (source, byte))?;
        let partition = (hash >> partitions.shift) & mask;
        let chunk = partitions.chunks[partition as usize] as usize;
        let (used, room) = access.room(chunk)?;
        if room == 0 {
            full = Some(partition);
            break;
        }
        // SAFETY: the source record lies below its chunk's used mark, the
        // copy past the destination's, within it as `room` counted, and
        // the chunks differ; the caller is the destination's one writer.
        unsafe {
            let bytes = region.record(region.spot(source, byte), record_size);
            let copy = region.record_mut(region.spot(chunk, used), record_size);
            copy.copy_from_slice(bytes);
            // No next record: the copy is linked anew.
            copy[4..8].fill(0);
            access.set_used(chunk, used + record_size);
        }
        offsets[count] = access.reference((chunk, used));
        hashes[count] = hash;
        count += 1;
        *from = byte + record_size;
    }
    Ok(Split { count, full })
}

/// Link the records of chunk `chunk` from byte `*from` to its used mark
/// into their buckets, first in their chains, and move `*from` past them:
/// they are counted first, so that a probe that finds one also sees a
/// count that covers its chain, then published one by one. Several
/// participants may link chunks of their own at once. The count linked is
/// returned.
pub(super) fn link<R: Region>(
    region: &R,
    layout: &Layout,
    chunk: usize,
    from: &mut usize,
) -> Result<usize> {
    link_counting::<R, false>(region, layout, chunk, from).map(|(count, _)| count)
}

/// As [`link`], and with `DUPLICATES` also count the records whose keys
/// the table held already: once a record is published, the rest of its
/// chain, the head it was put before, is walked for a record with the same
/// hash, NULL bits and keys. The CAS on a bucket orders its records, so of
/// two records of one key exactly the one linked later finds the other,
/// whatever participants link at once: the count is exact, as
/// [`super::exclusive::link_grouped`]'s. Returns the count linked and the
/// duplicates.
pub(super) fn link_counting<R: Region, const DUPLICATES: bool>(
    region: &R,
    layout: &Layout,
    chunk: usize,
    from: &mut usize,
) -> Result<(usize, usize)> {
    let mut access = Access::new(region, layout);
    let mut duplicates = 0;
    let bytes = unlinked(&access, chunk, *from)?;
    let count = bytes.len();
    let end = *from + count * access.record_size();
    access.count(count);
    // The chunk resolved once: a store into a record may not reload it.
    // SAFETY: `unlinked` checked the chunk and its used mark.
    let first = unsafe { region.spot(chunk, 0) };
    for byte in bytes {
        let spot = R::advance(first, byte);
        // SAFETY: the record lies in the chunk, below its used mark.
        let hash = unsafe { check_record_at(&access, spot, (chunk, byte)) }?;
        // SAFETY: as above, and the caller alone links this chunk.
        let rest = unsafe { access.push_at(access.reference((chunk, byte)), spot, hash) };
        if DUPLICATES && rest != 0 {
            // SAFETY: the record is published and never written again.
            let record = unsafe { access.view_at(spot) };
            let (null_bits, keys) = (record.null_bits(), record.keys());
            let same = access.find(rest, hash, |other| {
                other.null_bits() == null_bits && same_keys(other.keys(), keys)
            })?;
            duplicates += usize::from(same != 0);
        }
    }
    *from = end;
    Ok((count, duplicates))
}

/// The first bytes of a chunk's records from byte `from` to its used
/// mark; `from` must be a record boundary.
pub(super) fn unlinked<R: Region>(
    access: &Access<'_, R>,
    chunk: usize,
    from: usize,
) -> Result<core::iter::StepBy<core::ops::Range<usize>>> {
    let (used, _) = access.room(chunk)?;
    ensure!(
        from >= CHUNK_HEADER
            && from <= used
            && (from - CHUNK_HEADER).is_multiple_of(access.record_size()),
        "byte {from} of table chunk {chunk} is no record boundary below its {used} used bytes"
    );
    Ok((from..used).step_by(access.record_size()))
}

/// The hash of the record at a place of a chunk below its used mark,
/// which must claim the table's record length.
#[inline]
pub(super) fn check_record<R: Region>(access: &Access<'_, R>, place: Place) -> Result<u32> {
    // SAFETY: the place lies below the chunk's used mark, which `room`
    // checked against its length; the caller wrote it or it is published.
    unsafe { check_record_at(access, access.spot(place), place) }
}

/// As [`check_record`], for the record at a spot the caller resolved.
///
/// # Safety
///
/// `spot` is the spot of `place`, which lies below its chunk's used mark.
#[inline(always)]
pub(super) unsafe fn check_record_at<R: Region>(
    access: &Access<'_, R>,
    spot: R::Spot,
    place: Place,
) -> Result<u32> {
    // SAFETY: the caller's contract; the record is written or published.
    let view = unsafe { access.view_at(spot) };
    ensure!(
        view.len() == access.record_size(),
        "table chunk {} holds no record at byte {}",
        place.0,
        place.1
    );
    Ok(view.hash())
}

/// Find the first record with the hash and keys of each row of `rows`:
/// `matches[row]` gets its offset and `found` the rows that have one.
pub(super) fn probe<R: Region, K: KeySource + ?Sized>(
    region: &R,
    layout: &Layout,
    hashes: &[u32],
    keys: &K,
    rows: &RowMaskView<'_>,
    matches: &mut [u32],
    found: &mut RowMask<'_>,
) -> Result<()> {
    let nrows = rows.nrows();
    check(layout, keys, nrows, hashes.len(), matches.len())?;
    ensure!(
        found.as_view().nrows() == nrows,
        "the result mask has {} rows, the batch {nrows}",
        found.as_view().nrows()
    );
    shaped!(
        layout.nkeys,
        0,
        probe_rows(region, layout, hashes, keys, rows, matches, found)
    )
}

/// The rows of [`probe`] for a table of `N` keys, 0 when it is not one of
/// the specialized counts; `T` is unused.
#[inline(never)]
fn probe_rows<R: Region, K: KeySource + ?Sized, const N: usize, const T: usize, const L: usize>(
    region: &R,
    layout: &Layout,
    hashes: &[u32],
    keys: &K,
    rows: &RowMaskView<'_>,
    matches: &mut [u32],
    found: &mut RowMask<'_>,
) -> Result<()> {
    // Made here, not passed in, so that its fields stay in registers.
    let mut access = Access::new(region, layout);
    let nrows = rows.nrows();
    let mut buffer = slot_buffer::<L>();
    let mut word_keys = WordKeys::new(&mut buffer, access.nkeys());
    let mut lanes = Lanes::new();
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.word(index).unwrap();
        let mut hits = 0;
        if selected.count_ones() >= VERTICAL_MIN_ROWS {
            word_keys.load(keys, index, selected)?;
            let base = index * 64;
            let end = nrows.min(base + 64);
            hits = probe_word::<R, N>(
                &mut access,
                &word_keys,
                &hashes[base..end],
                selected,
                &mut matches[base..end],
                &mut lanes,
            )?;
        } else if selected != 0 {
            word_keys.load(keys, index, selected)?;
            let mut bits = selected;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let row = index * 64 + bit;
                let hash = hashes[row];
                let head = access.head(hash);
                // SAFETY: the records are this table's, whose key count
                // the buffer was made for and `N` is 0 or.
                let offset = access.find(head, hash, |record| unsafe {
                    word_keys.equal::<N>(bit, record)
                })?;
                if offset != 0 {
                    matches[row] = offset;
                    hits |= 1 << bit;
                }
            }
        }
        found.set_word(index, hits)?;
    }
    Ok(())
}

/// Rows of a word from which a probe goes vertically, phase by phase over
/// the word, rather than row by row: fewer rows do not pay for phases
/// that run over all 64.
pub(super) const VERTICAL_MIN_ROWS: u32 = 8;

/// The per-row arrays of a word's vertical probe, indexed by row within
/// the word: the rows' hashes, the records each row is at, and their
/// spots, resolved once per step for both the hint and the comparison. A
/// probe writes a row's entries before it reads them, for the selected
/// rows only, so the arrays start uninitialized and are never cleared.
pub(super) struct Lanes<S: Copy> {
    hash: [MaybeUninit<u32>; 64],
    current: [MaybeUninit<u32>; 64],
    spot: [MaybeUninit<S>; 64],
}

impl<S: Copy> Lanes<S> {
    #[inline(always)]
    pub(super) fn new() -> Self {
        Self {
            hash: [MaybeUninit::uninit(); 64],
            current: [MaybeUninit::uninit(); 64],
            spot: [MaybeUninit::uninit(); 64],
        }
    }
}

/// The rows of `bits`, lowest first.
#[inline(always)]
fn rows_of(mut bits: u64) -> impl Iterator<Item = usize> {
    core::iter::from_fn(move || {
        (bits != 0).then(|| {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            bit
        })
    })
}

/// Probe the `selected` rows of one word phase by phase: every row's bucket
/// is hinted to the cache, then every head read; while rows remain, every
/// candidate record is checked and hinted, then every candidate compared
/// with its row and the rows that did not match moved down their chains.
/// The loads of different rows are independent and in flight together,
/// which a row-by-row walk, with its longer path per row, allows for fewer
/// rows at a time. A candidate is compared field by field as it is read:
/// gathering the fields into arrays for a vector comparison made vector
/// loads wait on the scalar stores that had just filled them. The answer is the
/// row-by-row one: each row gets the first record of its chain with its
/// hash, null bits and keys. Returns the rows found.
#[inline(always)]
pub(super) fn probe_word<R: Region, const N: usize>(
    access: &mut Access<'_, R>,
    word_keys: &WordKeys<'_>,
    hashes: &[u32],
    selected: u64,
    matches: &mut [u32],
    lanes: &mut Lanes<R::Spot>,
) -> Result<u64> {
    for bit in rows_of(selected) {
        let hash = hashes[bit];
        lanes.hash[bit].write(hash);
        access.prefetch_bucket(hash);
    }
    // SAFETY (every `assume_init` below): the entries of a row are read
    // only for rows of `selected` (`pending` is a subset of it), after the
    // loop above wrote its hash and the loop below its first offset.
    let mut pending = 0;
    for bit in rows_of(selected) {
        let head = access.head(unsafe { lanes.hash[bit].assume_init() });
        lanes.current[bit].write(head);
        pending |= u64::from(head != 0) << bit;
    }
    let mut hits = 0;
    let mut steps = 0;
    while pending != 0 {
        access.check_steps(steps)?;
        steps += 1;
        for bit in rows_of(pending) {
            let place = access.place(unsafe { lanes.current[bit].assume_init() })?;
            // SAFETY: `place` accepted it.
            let spot = unsafe { access.spot(place) };
            lanes.spot[bit].write(spot);
            access.prefetch_record(spot);
        }
        let mut rest = 0;
        for bit in rows_of(pending) {
            let offset = unsafe { lanes.current[bit].assume_init() };
            // SAFETY: `place` accepted every offset of `pending` above,
            // and the loop above resolved its spot.
            let record = unsafe { access.open_at(lanes.spot[bit].assume_init(), offset) }?;
            // SAFETY: the record is this table's, whose key count the
            // buffer was made for and `N` is 0 or.
            let hash = unsafe { lanes.hash[bit].assume_init() };
            if record.hash() == hash && unsafe { word_keys.equal::<N>(bit, &record) } {
                matches[bit] = offset;
                hits |= 1 << bit;
            } else {
                let next = record.next();
                lanes.current[bit].write(next);
                rest |= u64::from(next != 0) << bit;
            }
        }
        pending = rest;
    }
    Ok(hits)
}

/// For each row of `rows`, replace `offsets[row]` by the record after it
/// in its chain with the same hash and keys; `found` gets the rows that
/// have one, and the others keep their offset.
pub(super) fn next_match<R: Region>(
    region: &R,
    layout: &Layout,
    offsets: &mut [u32],
    rows: &RowMaskView<'_>,
    found: &mut RowMask<'_>,
) -> Result<()> {
    let nrows = rows.nrows();
    ensure!(
        offsets.len() == nrows && found.as_view().nrows() == nrows,
        "the offsets, mask and result of the batch have different row counts"
    );
    let mut access = Access::new(region, layout);
    for index in 0..nrows.div_ceil(64) {
        let mut bits = rows.word(index).unwrap();
        let mut hits = 0;
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let row = index * 64 + bit;
            let record = access.locate(offsets[row])?;
            let (hash, null_bits, keys) = (record.hash(), record.null_bits(), record.keys());
            let after = access.find(record.next(), hash, |other| {
                other.null_bits() == null_bits && same_keys(other.keys(), keys)
            })?;
            if after != 0 {
                offsets[row] = after;
                hits |= 1 << bit;
            }
        }
        found.set_word(index, hits)?;
    }
    Ok(())
}

/// For each row of `rows`, the 8 bytes at byte `at` of the payload of the
/// record at `offsets[row]` into `out[row]`; other rows of `out` keep
/// their values.
pub(super) fn gather<R: Region, const PREFETCH: bool>(
    region: &R,
    layout: &Layout,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    at: usize,
    out: &mut [u64],
) -> Result<()> {
    let nrows = rows.nrows();
    ensure!(
        offsets.len() == nrows && out.len() == nrows,
        "the offsets, mask and output of the batch have different row counts"
    );
    ensure!(
        at.checked_add(8)
            .is_some_and(|end| end <= layout.payload_size),
        "a payload word at byte {at} is past the payload of {} bytes",
        layout.payload_size
    );
    let mut access = Access::new(region, layout);
    if !PREFETCH {
        for index in 0..nrows.div_ceil(64) {
            for bit in rows_of(rows.word(index).unwrap()) {
                let row = index * 64 + bit;
                let payload = access.locate(offsets[row])?.payload();
                let mut word = [0; 8];
                word.copy_from_slice(&payload[at..at + 8]);
                out[row] = u64::from_ne_bytes(word);
            }
        }
        return Ok(());
    }
    // Records in no order, as a sort's rows read back: every record of a
    // word of rows is located and its header and word prefetched before
    // any is read, so that the misses overlap. Records a probe has just
    // read are in the cache, where the extra pass only costs.
    let word_at = RECORD_HEADER + 8 * access.nkeys() + at;
    let mut spots = [const { MaybeUninit::<R::Spot>::uninit() }; 64];
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.word(index).unwrap();
        for bit in rows_of(selected) {
            let place = access.place(offsets[index * 64 + bit])?;
            // SAFETY: `place` accepted it.
            let spot = unsafe { access.spot(place) };
            spots[bit].write(spot);
            access.prefetch_record(spot);
            access.prefetch_record(R::advance(spot, word_at));
        }
        for bit in rows_of(selected) {
            let row = index * 64 + bit;
            // SAFETY: the loop above resolved the spot of every row of
            // `selected` from an offset `place` accepted.
            let record = unsafe { access.open_at(spots[bit].assume_init(), offsets[row]) }?;
            let payload = record.payload();
            let mut word = [0; 8];
            word.copy_from_slice(&payload[at..at + 8]);
            out[row] = u64::from_ne_bytes(word);
        }
    }
    Ok(())
}

/// For each row of `rows`, the record right after `offsets[row]` in its
/// chain when it has the same hash, null bits and keys: the next record of
/// the key in a table built by grouped insertion, where they lie next to
/// each other. `found` gets the rows that have one, and the others keep
/// their offset.
pub(super) fn next_in_group<R: Region>(
    region: &R,
    layout: &Layout,
    offsets: &mut [u32],
    rows: &RowMaskView<'_>,
    found: &mut RowMask<'_>,
) -> Result<()> {
    let nrows = rows.nrows();
    ensure!(
        offsets.len() == nrows && found.as_view().nrows() == nrows,
        "the offsets, mask and result of the batch have different row counts"
    );
    let mut access = Access::new(region, layout);
    for index in 0..nrows.div_ceil(64) {
        let mut bits = rows.word(index).unwrap();
        let mut hits = 0;
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let row = index * 64 + bit;
            let record = access.locate(offsets[row])?;
            let next = record.next();
            if next == 0 {
                continue;
            }
            let other = access.locate(next)?;
            if other.hash() == record.hash()
                && other.null_bits() == record.null_bits()
                && same_keys(other.keys(), record.keys())
            {
                offsets[row] = next;
                hits |= 1 << bit;
            }
        }
        found.set_word(index, hits)?;
    }
    Ok(())
}

/// For each row of `rows`, key `key` of the record at `offsets[row]` into
/// `values[row]` as the bits of its slot, an int4 sign-extended as its
/// Datum is, and whether it is NULL into `nulls[row]`; other rows keep
/// their values.
pub(super) fn gather_key<R: Region>(
    region: &R,
    layout: &Layout,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    key: usize,
    values: &mut [u64],
    nulls: &mut [bool],
) -> Result<()> {
    let nrows = rows.nrows();
    ensure!(
        offsets.len() == nrows && values.len() == nrows && nulls.len() == nrows,
        "the offsets, mask and output of the batch have different row counts"
    );
    ensure!(
        key < layout.nkeys,
        "key {key} is past the table's {} keys",
        layout.nkeys
    );
    let mut access = Access::new(region, layout);
    for index in 0..nrows.div_ceil(64) {
        let mut bits = rows.word(index).unwrap();
        while bits != 0 {
            let row = index * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let record = access.locate(offsets[row])?;
            let null = (record.null_bits() >> key) & 1 == 1;
            values[row] = if null { 0 } else { record.keys()[key] as u64 };
            nulls[row] = null;
        }
    }
    Ok(())
}

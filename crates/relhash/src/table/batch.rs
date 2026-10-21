//! The batch operations: appending rows to a chunk, linking a chunk's
//! records into the buckets, probing, the next match of a row (in any
//! table, or within a group of a grouped one) and the gathering of a
//! payload word.

use core::mem::MaybeUninit;

use crate::error::{Error, Kind, Result, Rule, ensure};
use crate::{RowMask, RowMaskView, check_rows, ones};

use super::header::{Layout, NRECORDS, Place, RECORD_HEADER};
use super::keys::{KeySource, WordKeys, slot_buffer};
use super::lanes::{Lanes, VERTICAL_MIN_ROWS, probe_word};
use super::record::no_chunk;
use super::record::{Access, PayloadColumns, same_keys};
use super::record::{check_record, check_record_at, unlinked};
use super::region::Region;
use super::{Appended, MAX_PARTITIONS, Partitions, Split};

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

/// The rows of a batch to place in a table, through one call or several.
///
/// A call takes no memory: a table's chunks and its index are the
/// caller's. So a call that places rows stops when its chunk is full,
/// and a call that finds or inserts also stops when the records reach
/// half the buckets. The caller then adds a chunk or makes a larger
/// index and calls again with the same batch.
/// [`OwnedTable`](super::OwnedTable) does that itself: its calls take
/// the rows to place as a mask they only read, and place every one.
///
/// Besides each row's hash and keys, a batch holds two arrays of the
/// caller's, each with a place for every row; [`Batch::new`] takes the
/// mask's words, [`Batch::from_mask`] a mask made of them:
///
/// - `pending`, a mask, a bit per row: the rows still to place. Before
///   the first call it holds the rows to place, such as those whose keys
///   are not NULL. Each call clears the bit of every row it places, so the
///   mask only shrinks, and the batch is done when it is empty.
/// - `refs`: for each row a call places, the reference of its record,
///   which [`Table::record`](crate::Table::record) reads and through
///   which a caller changes the record's payload. The places of the rows
///   left out of the mask keep what they held. The place of a row still
///   pending holds nothing to read: a call that finds or inserts may have
///   written there the record it found before it stopped.
///
/// A batch of four rows, whose row 2 has a NULL key, appended to chunks
/// with room for two records each:
///
/// ```text
///                        pending     refs
///   before the calls     {0, 1, 3}   [ -,  -,  -,  - ]
///   append to chunk 5    {3}         [r1, r2,  -,  - ]   chunk 5 is full
///   append to chunk 6    {}          [r1, r2,  -, r3 ]   the batch is done
/// ```
///
/// The rows still to place are a mask, not the number of a row to go on
/// from. A call into one chunk places the rows in row order and stops at
/// the first that does not fit, so it leaves the end of the batch. A call
/// by partition, [`ChunkWriter::append_partitioned_columns`] or
/// [`TableMut::find_or_insert_partitioned`], leaves pending a row whose
/// partition's chunk is full and goes on with the rows after it: what it
/// leaves has gaps, and the caller reads those rows to learn which
/// partitions need a new chunk. One mask serves every call, and every
/// caller runs the same loop: call, and while rows are pending, add a
/// chunk or a larger index and call again.
///
/// A call that fails at a row, because the source cannot give a word's
/// keys or a partition's chunk is refused, keeps what it placed before:
/// those rows have left `pending` with their references, the rows that
/// made a record are in the mask of inserted rows, and each chunk's used
/// mark covers their records. The batch given again places the rest.
///
/// [`ChunkWriter::append_partitioned_columns`]: crate::ChunkWriter::append_partitioned_columns
/// [`TableMut::find_or_insert_partitioned`]: crate::TableMut::find_or_insert_partitioned
pub struct Batch<'b, K: ?Sized> {
    pub(super) hashes: &'b [u32],
    pub(super) keys: &'b K,
    pub(super) pending: RowMask<'b>,
    pub(super) refs: &'b mut [u32],
}

impl<'b, K: KeySource + ?Sized> Batch<'b, K> {
    /// A batch of the rows set in `pending`, a word for each 64 of the
    /// `refs.len()` rows, each with its hash, its keys and its slot in
    /// `refs`. The batch borrows its source of keys for as long as it
    /// lives, so a source made for the call is bound first:
    ///
    /// ```
    /// use relhash::{Batch, KeySource, ones};
    ///
    /// struct Keys<'a>(&'a [i32]);
    /// impl KeySource for Keys<'_> {
    ///     fn nkeys(&self) -> usize { 1 }
    ///     fn nrows(&self) -> usize { self.0.len() }
    ///     fn word(&self, _: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Option<u64> {
    ///         for bit in ones(selected) {
    ///             out[bit] = i64::from(self.0[index * 64 + bit]);
    ///         }
    ///         Some(selected)
    ///     }
    /// }
    ///
    /// let values = [7, 8, 7];
    /// let keys = Keys(&values);
    /// let (mut pending, mut refs) = ([0b111], [0; 3]);
    /// let batch = Batch::new(&[1, 2, 1], &keys, &mut pending, &mut refs)?;
    /// assert_eq!(batch.pending().selected_count(), 3);
    /// # Ok::<(), relhash::Error>(())
    /// ```
    #[inline]
    pub fn new(
        hashes: &'b [u32],
        keys: &'b K,
        pending: &'b mut [u64],
        refs: &'b mut [u32],
    ) -> Result<Self> {
        Self::from_mask(hashes, keys, RowMask::try_new(refs.len(), pending)?, refs)
    }

    /// A batch of the rows of `pending`, whose row count the hashes, the
    /// keys and the references must have.
    #[inline]
    pub fn from_mask(
        hashes: &'b [u32],
        keys: &'b K,
        pending: RowMask<'b>,
        refs: &'b mut [u32],
    ) -> Result<Self> {
        let nrows = pending.as_view().nrows();
        check_rows(nrows, &[keys.nrows(), hashes.len(), refs.len()])?;
        Ok(Self {
            hashes,
            keys,
            pending,
            refs,
        })
    }

    /// The rows still to place.
    pub fn pending(&self) -> RowMaskView<'_> {
        self.pending.as_view()
    }

    /// The reference of each placed row's record, at its row.
    pub fn refs(&self) -> &[u32] {
        self.refs
    }
}

/// A lookup of a batch's rows in the table: which rows are still in it,
/// and which record each of them has reached.
///
/// It holds two arrays of the caller's, each with a place for every row
/// of the batch:
///
/// - `rows`, a mask, a bit per row: the rows still in the lookup. A call
///   clears the bit of each row it finds nothing for, so the mask only
///   shrinks.
/// - `refs`: for each row in the mask, the reference of its record, which
///   [`Table::record`](crate::Table::record) reads. A row that leaves the
///   mask keeps the last reference it had.
///
/// A probe starts a lookup at each row's first record with the row's
/// keys. A next match, or a next in a group, moves each row on to the
/// next record with the same keys. The gathers read the payloads of the
/// records of the rows in the mask. A walk over every record of a batch
/// of three rows, whose keys have three records, one and two:
///
/// ```text
///                 rows        refs
///   probe         {0, 1, 2}   [a1, b1, c1]   each row's first record
///   next match    {0, 2}      [a2, b1, c2]   row 1 had one record
///   next match    {0}         [a3, b1, c2]   row 2 had two
///   next match    {}          [a3, b1, c2]   no row has another
/// ```
///
/// The mask and `refs` must have the same rows. That is checked once,
/// when a lookup is made, and not again by each call.
#[derive(Debug)]
pub struct Matches<'b> {
    pub(super) rows: RowMask<'b>,
    pub(super) refs: &'b mut [u32],
}

impl<'b> Matches<'b> {
    /// A lookup of the rows set in `rows`, a word for each 64 of the
    /// `refs.len()` rows, each with its slot in `refs`.
    #[inline]
    pub fn new(rows: &'b mut [u64], refs: &'b mut [u32]) -> Result<Self> {
        Self::from_mask(RowMask::try_new(refs.len(), rows)?, refs)
    }

    /// A lookup of the rows of `rows`, a mask of `refs.len()` rows.
    #[inline]
    pub fn from_mask(rows: RowMask<'b>, refs: &'b mut [u32]) -> Result<Self> {
        check_rows(refs.len(), &[rows.as_view().nrows()])?;
        Ok(Self { rows, refs })
    }

    /// The rows still in the lookup.
    #[inline]
    pub fn rows(&self) -> RowMaskView<'_> {
        self.rows.as_view()
    }

    /// The rows still in the lookup, to narrow before a call, as a
    /// filter's probe does.
    #[inline]
    pub fn rows_mut(&mut self) -> &mut RowMask<'b> {
        &mut self.rows
    }

    /// The reference of each row's record, at its row.
    #[inline]
    pub fn refs(&self) -> &[u32] {
        self.refs
    }
}

/// The rows of a lookup and their references, to read: a [`Matches`] seen
/// by the gathers, or rows and references that come from elsewhere, such
/// as the items of a sort.
#[derive(Clone, Copy, Debug)]
pub struct MatchesView<'a> {
    pub(super) rows: RowMaskView<'a>,
    pub(super) refs: &'a [u32],
}

impl<'a> MatchesView<'a> {
    /// The rows of `rows`, a mask of `refs.len()` rows, each with its
    /// record's reference at its row in `refs`.
    #[inline]
    pub fn new(rows: RowMaskView<'a>, refs: &'a [u32]) -> Result<Self> {
        check_rows(refs.len(), &[rows.nrows()])?;
        Ok(Self { rows, refs })
    }

    /// The rows.
    #[inline]
    pub fn rows(&self) -> RowMaskView<'a> {
        self.rows
    }

    /// The reference of each row's record, at its row.
    #[inline]
    pub fn refs(&self) -> &'a [u32] {
        self.refs
    }
}

impl<'a> From<&'a Matches<'_>> for MatchesView<'a> {
    #[inline]
    fn from(matches: &'a Matches<'_>) -> Self {
        Self {
            rows: matches.rows.as_view(),
            refs: matches.refs,
        }
    }
}

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
    check_rows(nrows, &[keys.nrows(), hashes, out])
}

/// Reject keys that are not the table's, before anything is read or
/// changed; a [`Batch`] checked its row counts when it was made.
pub(super) fn check_keys<K: KeySource + ?Sized>(layout: &Layout, keys: &K) -> Result<()> {
    ensure!(
        keys.nkeys() == layout.nkeys,
        Error::wrong_call(Rule::Keys)
            .with(layout.nkeys)
            .and(keys.nkeys())
    );
    Ok(())
}

/// Check that `payload` holds `payload_size` bytes for each of `nrows`
/// rows.
fn check_payload(payload: Option<&[u8]>, nrows: usize, payload_size: usize) -> Result<()> {
    if let Some(payload) = payload {
        let needed = nrows.checked_mul(payload_size);
        ensure!(
            needed == Some(payload.len()),
            Error::wrong_call(Rule::PayloadBytes)
                .with(payload.len())
                .and(needed.unwrap_or(usize::MAX))
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
    batch: &mut Batch<'_, K>,
) -> Result<usize> {
    check_keys(layout, batch.keys)?;
    check_payload(payload, batch.refs.len(), layout.payload_size)?;
    let Batch {
        hashes,
        keys,
        pending,
        refs,
    } = batch;
    shaped!(
        layout.nkeys,
        layout.tail_words(),
        append_rows(region, layout, chunk, hashes, *keys, payload, pending, refs)
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
    refs: &mut [u32],
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
        let selected = pending.as_view().word(index);
        if selected == 0 {
            continue;
        }
        if room == 0 {
            break;
        }
        word_keys
            .load(keys, index, selected)
            .map_err(|error| stopped(region, chunk, used, error))?;
        let wanted = selected.count_ones() as usize;
        let count = wanted.min(room);
        // The walk of `ones` written out, counted: through the iterator
        // this loop compiled to twice its length; `.cargo/mutants.toml`
        // leaves the step out.
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
                access.write::<N, T>(
                    Place { chunk, byte: used },
                    hashes[row],
                    &word_keys,
                    bit,
                    row_payload,
                )
            };
            refs[row] = Place { chunk, byte: used }.reference();
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

/// End an append that stops at a word on `error`, the source unable to
/// give its keys: the words before stay placed, so the chunk's used mark
/// covers their records. Out of the loop, which keeps its shape; it takes
/// the region, not the access, whose fields stay in registers.
#[cold]
#[inline(never)]
fn stopped<R: Region>(region: &R, chunk: usize, used: usize, error: Error) -> Error {
    // SAFETY: the caller is the chunk's one writer, and `used` ends the
    // records it wrote.
    unsafe { region.set_chunk_used(chunk, used as u64) };
    error
}

/// As [`append`], with each row's payload taken from `columns`: a word of
/// its NULL bits, then a word per column. The table's payload must be
/// exactly those words.
pub(super) fn append_columns<R: Region, K: KeySource + ?Sized>(
    region: &R,
    layout: &Layout,
    chunk: usize,
    columns: &PayloadColumns<'_>,
    batch: &mut Batch<'_, K>,
) -> Result<usize> {
    let Batch {
        hashes,
        keys,
        pending,
        refs,
    } = batch;
    let nrows = refs.len();
    check_keys(layout, *keys)?;
    ensure!(
        layout.payload_size == 8 * (columns.null_words() + columns.len()),
        Error::wrong_call(Rule::PayloadColumns)
            .with(layout.payload_size)
            .and(columns.len())
    );
    check_rows(nrows, &[columns.nrows()])?;
    shaped!(
        layout.nkeys,
        layout.tail_words(),
        append_column_rows(region, layout, chunk, hashes, *keys, columns, pending, refs)
    )
}

/// The rows of [`append_columns`], shaped as [`append_rows`]; the words
/// after the keys are the columns', so `T` does not matter. The loop is
/// [`append_rows`]'s written again: as one loop over a closure that writes
/// a row's record, LLVM laid out `append_rows`' loops anew, and the one of
/// any shape grew.
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
    refs: &mut [u32],
) -> Result<usize> {
    let access = Access::for_chunks(region, layout);
    let (mut used, mut room) = access.room(chunk)?;
    let record_size = access.record_size();
    let nrows = pending.as_view().nrows();
    let mut buffer = slot_buffer::<L>();
    let mut word_keys = WordKeys::new(&mut buffer, access.nkeys());
    let mut appended = 0;
    for index in 0..nrows.div_ceil(64) {
        let selected = pending.as_view().word(index);
        if selected == 0 {
            continue;
        }
        if room == 0 {
            break;
        }
        word_keys
            .load(keys, index, selected)
            .map_err(|error| stopped(region, chunk, used, error))?;
        let wanted = selected.count_ones() as usize;
        let count = wanted.min(room);
        // The walk of `ones` written out, counted: through the iterator
        // this loop compiled to twice its length; `.cargo/mutants.toml`
        // leaves the step out.
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
                access.write_columns::<N>(
                    Place { chunk, byte: used },
                    hashes[row],
                    &word_keys,
                    bit,
                    columns,
                    row,
                )
            };
            refs[row] = Place { chunk, byte: used }.reference();
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
        Error::wrong_call(Rule::Partitions).with(count)
    );
    let bits = count.trailing_zeros();
    ensure!(
        partitions.shift < 32 && partitions.shift + bits <= 32,
        Error::wrong_call(Rule::PartitionBits)
            .with(partitions.shift)
            .and(partitions.shift + bits)
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
    batch: &mut Batch<'_, K>,
    appended: &mut Appended<'_>,
) -> Result<usize> {
    let Batch {
        hashes,
        keys,
        pending,
        refs,
    } = batch;
    let Appended { rows, nulls } = appended;
    let nrows = refs.len();
    check_keys(layout, *keys)?;
    ensure!(
        layout.payload_size == 8 * (1 + columns.len()) && columns.len() <= 64,
        Error::wrong_call(Rule::PartitionColumns)
            .with(layout.payload_size)
            .and(columns.len())
    );
    check_rows(nrows, &[columns.nrows()])?;
    let mask = check_partitions(region, partitions)?;
    ensure!(
        rows.len() == partitions.chunks.len(),
        Error::wrong_call(Rule::PartitionRows)
            .with(rows.len())
            .and(partitions.chunks.len())
    );
    shaped!(
        layout.nkeys,
        layout.tail_words(),
        append_partitioned_column_rows(
            region, layout, partitions, mask, hashes, *keys, columns, pending, refs, rows, nulls
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
    refs: &mut [u32],
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
        let selected = pending.as_view().word(index);
        if selected == 0 {
            continue;
        }
        // A failure leaves the rows written before placed: their marks are
        // stored record by record, and the masks get them below.
        if let Err(error) = word_keys.load(keys, index, selected) {
            *nulls |= seen;
            return Err(error);
        }
        let bits = selected;
        let mut done = 0;
        for bit in ones(bits) {
            let row = index * 64 + bit;
            let partition = ((hashes[row] >> shift) & mask) as usize;
            // Checked: every partition's chunk exists, and has a count.
            let chunk = partitions.chunks[partition] as usize;
            let (used, room) = match access.room(chunk) {
                Ok(room) => room,
                Err(error) => {
                    pending.intersect_word(index, !done)?;
                    *nulls |= seen;
                    return Err(error);
                }
            };
            if room == 0 {
                continue;
            }
            // SAFETY: the place lies past the used mark and within the
            // chunk, as `room` counted; the buffer and the shape are this
            // table's; the payload is a word and one per column, and `row`
            // is below the columns' row count, `nrows`.
            seen |= unsafe {
                access.write_columns::<N>(
                    Place { chunk, byte: used },
                    hashes[row],
                    &word_keys,
                    bit,
                    columns,
                    row,
                )
            };
            refs[row] = Place { chunk, byte: used }.reference();
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
/// caller is, and move `*from` past them: at most `refs.len()` of them,
/// the new references into `refs` and their hashes into `hashes`. The
/// copies are not linked. It stops before a record
/// whose partition's chunk is full, which [`Split::full`] names. A call
/// copies at least one record, so that a count of 0 means the end.
pub(super) fn split<R: Region>(
    region: &R,
    layout: &Layout,
    partitions: &Partitions<'_>,
    source: usize,
    from: &mut usize,
    refs: &mut [u32],
    hashes: &mut [u32],
) -> Result<Split> {
    let mask = check_partitions(region, partitions)?;
    ensure!(
        refs.len() == hashes.len(),
        Error::wrong_call(Rule::SplitLengths)
    );
    // A count of 0 means the source is done, so a call copies one at least.
    ensure!(!refs.is_empty(), Error::wrong_call(Rule::EmptySplit));
    ensure!(
        !partitions.chunks.contains(&(source as u32)),
        Error::wrong_call(Rule::SplitIntoItself).with(source)
    );
    let access = Access::for_chunks(region, layout);
    let record_size = access.record_size();
    let places = unlinked(&access, source, *from)?;
    let mut count = 0;
    let mut full = None;
    for byte in places {
        if count == refs.len() {
            break;
        }
        let hash = check_record(
            &access,
            Place {
                chunk: source,
                byte,
            },
        )?;
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
        refs[count] = Place { chunk, byte: used }.reference();
        hashes[count] = hash;
        count += 1;
        *from = byte + record_size;
    }
    Ok(Split { count, full })
}

/// End a link of chunk `chunk` that stops on `error` before the record at
/// `stop`: the records from there to the chunk's used mark are not linked,
/// so they leave the count, and the next link starts at `stop`. It reads
/// what it needs again, so that the loop keeps no more of it.
#[cold]
#[inline(never)]
fn unlinked_from<R: Region>(
    region: &R,
    layout: &Layout,
    chunk: usize,
    from: &mut usize,
    stop: usize,
    error: Error,
) -> Error {
    // SAFETY: the caller links this chunk alone, and checked its mark.
    let used = unsafe { region.chunk_used(chunk) } as usize;
    let left = (used - stop) / layout.record_size;
    region.fetch_add_u64(NRECORDS, (left as u64).wrapping_neg());
    *from = stop;
    error
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
/// [`super::writer::link_grouped`]'s. Returns the count linked and the
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
    // By the byte up to the end: through the iterator of the bytes the
    // loop kept a count of its own, a move a record more.
    let mut byte = *from;
    while byte != end {
        let spot = R::advance(first, byte);
        // SAFETY: the record lies in the chunk, below its used mark.
        let hash = unsafe { check_record_at(&access, spot, Place { chunk, byte }) }
            .map_err(|error| unlinked_from(region, layout, chunk, from, byte, error))?;
        // SAFETY: as above, and the caller alone links this chunk.
        let rest = unsafe { access.push_at(Place { chunk, byte }.reference(), spot, hash) };
        if DUPLICATES && rest != 0 {
            // SAFETY: the record is published and never written again.
            let record = unsafe { access.view_at(spot) };
            let same = access
                .find_same(rest, hash, record.null_bits(), record.keys())
                .map_err(|error| {
                    unlinked_from(
                        region,
                        layout,
                        chunk,
                        from,
                        byte + layout.record_size,
                        error,
                    )
                })?;
            duplicates += usize::from(same != 0);
        }
        byte += access.record_size();
    }
    *from = end;
    Ok((count, duplicates))
}

/// Find the first record with the hash and keys of each row of `rows`:
/// `matches[row]` gets its offset, and the rows without one leave
/// `rows`; the count of rows left is returned.
pub(super) fn probe<R: Region, K: KeySource + ?Sized>(
    region: &R,
    layout: &Layout,
    hashes: &[u32],
    keys: &K,
    rows: &mut RowMask<'_>,
    matches: &mut [u32],
) -> Result<usize> {
    let nrows = rows.as_view().nrows();
    check(layout, keys, nrows, hashes.len(), matches.len())?;
    shaped!(
        layout.nkeys,
        0,
        probe_rows(region, layout, hashes, keys, rows, matches)
    )
}

/// The rows of [`probe`] for a table of `N` keys, 0 when it is not one of
/// the specialized counts; `T` is unused.
// The loop goes by index: an iterator's pointer, kept on the stack over a
// word's calls, was read back just before the next word's load.
#[allow(clippy::needless_range_loop)]
#[inline(never)]
fn probe_rows<R: Region, K: KeySource + ?Sized, const N: usize, const T: usize, const L: usize>(
    region: &R,
    layout: &Layout,
    hashes: &[u32],
    keys: &K,
    rows: &mut RowMask<'_>,
    matches: &mut [u32],
) -> Result<usize> {
    // Made here, not passed in, so that its fields stay in registers.
    let mut access = Access::new(region, layout);
    let nrows = rows.as_view().nrows();
    let mut buffer = slot_buffer::<L>();
    let mut word_keys = WordKeys::new(&mut buffer, access.nkeys());
    let mut lanes = Lanes::new();
    let mut left = 0;
    let words = rows.words_mut();
    for index in 0..words.len() {
        let selected = words[index];
        // A word of no rows has nothing to look up and stays 0. Each
        // branch writes its own word: one write after both kept the
        // word's address on the stack for every word, an empty one too.
        if selected.count_ones() >= VERTICAL_MIN_ROWS {
            word_keys.load(keys, index, selected)?;
            let base = index * 64;
            let end = nrows.min(base + 64);
            let hits = probe_word::<R, N>(
                &mut access,
                &word_keys,
                &hashes[base..end],
                selected,
                &mut matches[base..end],
                &mut lanes,
            )?;
            debug_assert_eq!(hits & !selected, 0, "a probe keeps only rows it looked up");
            words[index] = hits;
            left += hits.count_ones() as usize;
        } else if selected != 0 {
            word_keys.load(keys, index, selected)?;
            let mut hits = 0;
            for bit in ones(selected) {
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
            words[index] = hits;
            left += hits.count_ones() as usize;
        }
    }
    Ok(left)
}

/// For each row of `rows`, replace `refs[row]` by the record after it
/// in its chain with the same hash and keys; the rows without one leave
/// `rows` and keep their offset. The count of rows left is returned.
pub(super) fn next_match<R: Region>(
    region: &R,
    layout: &Layout,
    refs: &mut [u32],
    rows: &mut RowMask<'_>,
) -> Result<usize> {
    let nrows = rows.as_view().nrows();
    check_rows(nrows, &[refs.len()])?;
    let mut access = Access::new(region, layout);
    let mut left = 0;
    for (index, word) in rows.words_mut().iter_mut().enumerate() {
        let bits = *word;
        let mut hits = 0;
        for bit in ones(bits) {
            let row = index * 64 + bit;
            let record = access.locate(refs[row], Kind::WrongCall)?;
            let after = access.find_same(
                record.next(),
                record.hash(),
                record.null_bits(),
                record.keys(),
            )?;
            if after != 0 {
                refs[row] = after;
                hits |= 1 << bit;
            }
        }
        *word = hits;
        left += hits.count_ones() as usize;
    }
    Ok(left)
}

/// For each row of `rows`, the word at byte `at` of the payload of the
/// record at `refs[row]` into `out[row]`; other rows of `out` keep
/// their values. A payload is words, so `at` is a multiple of 8, as for
/// the calls that fold rows into a payload.
pub(super) fn gather<R: Region, const PREFETCH: bool>(
    region: &R,
    layout: &Layout,
    refs: &[u32],
    rows: &RowMaskView<'_>,
    at: usize,
    out: &mut [u64],
) -> Result<()> {
    let nrows = rows.nrows();
    check_rows(nrows, &[refs.len(), out.len()])?;
    ensure!(
        at.is_multiple_of(8)
            && at
                .checked_add(8)
                .is_some_and(|end| end <= layout.payload_size),
        Error::wrong_call(Rule::PayloadWord)
            .with(at)
            .and(layout.payload_size)
    );
    let mut access = Access::new(region, layout);
    if !PREFETCH {
        for index in 0..nrows.div_ceil(64) {
            for bit in ones(rows.word(index)) {
                let row = index * 64 + bit;
                let payload = access.locate(refs[row], Kind::WrongCall)?.payload();
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
        let selected = rows.word(index);
        for bit in ones(selected) {
            let place = access.place(refs[index * 64 + bit], Kind::WrongCall)?;
            // SAFETY: `place` accepted it.
            let spot = unsafe { access.spot(place) };
            spots[bit].write(spot);
            access.prefetch_record(spot);
            access.prefetch_record(R::advance(spot, word_at));
        }
        for bit in ones(selected) {
            let row = index * 64 + bit;
            // SAFETY: the loop above resolved the spot of every row of
            // `selected` from an offset `place` accepted.
            let record = unsafe { access.open_at(spots[bit].assume_init(), refs[row]) }?;
            let payload = record.payload();
            let mut word = [0; 8];
            word.copy_from_slice(&payload[at..at + 8]);
            out[row] = u64::from_ne_bytes(word);
        }
    }
    Ok(())
}

/// For each row of `rows`, payload word `first + n` (in 8-byte units) of
/// the record at `refs[row]` into `out[n][row]` for every `out[n]`,
/// records in no order, as [`gather`] with its prefetch reads one: a
/// record is located once for all its words, and every cache line they lie
/// on prefetched. Other slots of `out` keep their values.
pub(super) fn gather_words<R: Region>(
    region: &R,
    layout: &Layout,
    refs: &[u32],
    rows: &RowMaskView<'_>,
    first: usize,
    out: &mut [&mut [u64]],
) -> Result<()> {
    let nrows = rows.nrows();
    ensure!(
        refs.len() == nrows && out.iter().all(|words| words.len() == nrows),
        Error::wrong_call(Rule::BatchRows)
    );
    ensure!(
        !out.is_empty()
            && first
                .checked_add(out.len())
                .and_then(|end| end.checked_mul(8))
                .is_some_and(|end| end <= layout.payload_size),
        Error::wrong_call(Rule::PayloadWords)
            .with(out.len())
            .and(first)
    );
    let access = Access::new(region, layout);
    let first_at = RECORD_HEADER + 8 * (access.nkeys() + first);
    let last_at = first_at + 8 * (out.len() - 1);
    let mut spots = [const { MaybeUninit::<R::Spot>::uninit() }; 64];
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.word(index);
        for bit in ones(selected) {
            let place = access.place(refs[index * 64 + bit], Kind::WrongCall)?;
            // SAFETY: `place` accepted it.
            let spot = unsafe { access.spot(place) };
            spots[bit].write(spot);
            access.prefetch_record(spot);
            let mut line = first_at;
            while line < last_at {
                access.prefetch_record(R::advance(spot, line));
                line += 64;
            }
            access.prefetch_record(R::advance(spot, last_at));
        }
        for bit in ones(selected) {
            let row = index * 64 + bit;
            // SAFETY: the loop above resolved the spot of every row of
            // `selected` from an offset `place` accepted.
            let record = unsafe { access.open_at(spots[bit].assume_init(), refs[row]) }?;
            let payload = &record.payload()[8 * first..8 * (first + out.len())];
            for (words, bytes) in out.iter_mut().zip(payload.as_chunks::<8>().0) {
                words[row] = u64::from_ne_bytes(*bytes);
            }
        }
    }
    Ok(())
}

/// For each row of `rows`, the record right after `refs[row]` in its
/// chain when it has the same hash, null bits and keys: the next record of
/// the key in a table built by grouped insertion, where they lie next to
/// each other. The rows without one leave `rows` and keep their offset;
/// the count of rows left is returned.
pub(super) fn next_in_group<R: Region>(
    region: &R,
    layout: &Layout,
    refs: &mut [u32],
    rows: &mut RowMask<'_>,
) -> Result<usize> {
    let nrows = rows.as_view().nrows();
    check_rows(nrows, &[refs.len()])?;
    let mut access = Access::new(region, layout);
    let mut left = 0;
    for (index, word) in rows.words_mut().iter_mut().enumerate() {
        let bits = *word;
        let mut hits = 0;
        for bit in ones(bits) {
            let row = index * 64 + bit;
            let record = access.locate(refs[row], Kind::WrongCall)?;
            let next = record.next();
            if next == 0 {
                continue;
            }
            let other = access.locate(next, Kind::Damaged)?;
            if other.hash() == record.hash()
                && other.null_bits() == record.null_bits()
                && same_keys(other.keys(), record.keys())
            {
                refs[row] = next;
                hits |= 1 << bit;
            }
        }
        *word = hits;
        left += hits.count_ones() as usize;
    }
    Ok(left)
}

/// For each row of `rows`, key `key` of the record at `refs[row]` into
/// `values[row]` as the bits of its slot, an int4 sign-extended as its
/// Datum is, and whether it is NULL into `nulls[row]`; other rows keep
/// their values.
pub(super) fn gather_key<R: Region>(
    region: &R,
    layout: &Layout,
    refs: &[u32],
    rows: &RowMaskView<'_>,
    key: usize,
    values: &mut [u64],
    nulls: &mut [bool],
) -> Result<()> {
    let nrows = rows.nrows();
    check_rows(nrows, &[refs.len(), values.len(), nulls.len()])?;
    ensure!(
        key < layout.nkeys,
        Error::wrong_call(Rule::Key).with(key).and(layout.nkeys)
    );
    let mut access = Access::new(region, layout);
    for index in 0..nrows.div_ceil(64) {
        let bits = rows.word(index);
        for bit in ones(bits) {
            let row = index * 64 + bit;
            let record = access.locate(refs[row], Kind::WrongCall)?;
            let null = (record.null_bits() >> key) & 1 == 1;
            values[row] = if null { 0 } else { record.keys()[key] as u64 };
            nulls[row] = null;
        }
    }
    Ok(())
}

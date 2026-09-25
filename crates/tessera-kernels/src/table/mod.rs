//! A hash table in a borrowed region of memory, for joins and grouping.
//!
//! The region is a byte buffer the caller owns and hands to every call as a
//! pointer and a length: the local memory of a serial plan, grown by
//! `repalloc`, or dynamic shared memory of a parallel one, mapped by every
//! process at an address of its own. The table keeps no address between
//! calls and allocates nothing: everything inside the region refers to
//! other parts by offset from its start, so the bytes stay valid after a
//! move and mean the same to every process. Rust sees the region through
//! the operations here; the caller decides where it lives and grows it.
//!
//! A region holds a header of 96 bytes (see [`header`]), then the record
//! area, filled upward from the header, then the bucket array at the end
//! of the region: growth hands the table a larger region whose first bytes
//! are the old ones, records stay where they are, and the buckets are
//! rebuilt at the new end. Offsets of records are 32 bits wide in units of
//! 8 bytes, which addresses 32 GiB; 0 means none, since the header lies
//! there. A record has a hash, the offset of the next record of its bucket,
//! a bit per key that is NULL and its length in 8-byte units, then one
//! 8-byte slot per key (an int4 sign-extended) and the payload the table
//! was created for, rounded up to 8: opaque bytes, a join's build row or
//! grouping's aggregate states. A bucket is the high bits of the hash;
//! there are a power of two of them, at least 1024 and at least twice the
//! capacity the table was created for, each holding the offset of the
//! first record of its chain. Hashes come from [`crate::int32::hash`],
//! [`crate::int64::hash`] and their `hash_next`, which decide what NULL
//! keys do.
//!
//! [`Table`] is the access several participants may share: inserting
//! ([`Table::insert`], which always adds a record, so equal keys chain) or
//! probing ([`Table::probe`] for the first record with a row's hash and
//! keys, [`Table::next_match`] for the ones after it, [`Table::gather`]
//! for a payload word of each match), not both at a time.
//! [`TableMut`] is the access of one writer, which alone may give rows
//! the record of their keys, creating it when there is none
//! ([`TableMut::find_or_insert`], for grouping), insert records next to
//! those of the same keys ([`TableMut::insert_grouped`], for a join,
//! whose rounds then step with [`Table::next_in_group`]), change a payload in
//! place ([`TableMut::payload_mut`]), walk the records in insertion order
//! ([`TableMut::scan`]) or grow the region ([`TableMut::grow`]). A batch
//! brings its hashes, its keys through a [`KeySource`] and a row mask, and
//! gets record offsets back. Every call attaches anew and checks the whole header; every
//! offset is checked before it is followed, and a chain is walked at most
//! as many steps as there are records, so a corrupt region is an error,
//! never a hang or an access past the buffer. Dimension errors come before
//! any change. A full table is not an error: an insertion leaves the rows
//! without room in its mask for the caller to retry after growing.
//!
//! This is the second module of the crate allowed `unsafe`, for the region
//! over raw pointers and atomics on it; see [`region`]. Tests build tables
//! over `&mut [u64]` through [`TableMut::create_in`] and
//! [`TableMut::exclusive`], which need no `unsafe` and align the region.
//!
//! ```
//! use tessera_kernels::table::{KeyKind, TableConfig, TableMut, region_size};
//!
//! let config = TableConfig { keys: &[KeyKind::Int32], payload_size: 8 };
//! let mut words = vec![0; region_size(&config, 100)?.div_ceil(8)];
//! let table = TableMut::create_in(&mut words, &config, 100)?;
//! assert_eq!(table.stats().records, 0);
//! assert_eq!(table.key_kinds(), &[KeyKind::Int32]);
//!
//! // Three rows with keys 7, 8 and 7, hashed by the int4 kernel's formula.
//! use tessera_core::{ColumnView, RowMask, RowMaskView};
//! use tessera_kernels::int32::murmurhash32;
//! let keys = [ColumnView::try_new(&[7, 8, 7], None)?];
//! let hashes: Vec<u32> = [7, 8, 7].map(|key: i32| murmurhash32(key as u32)).into();
//! let payload = [1u64, 2, 3].map(u64::to_ne_bytes).concat();
//! let mut pending = [0b111];
//! let mut offsets = [0; 3];
//! let mut mask = RowMask::try_new(3, &mut pending)?;
//! table.insert(&hashes, &keys[..], Some(&payload), &mut mask, &mut offsets)?;
//! assert_eq!(pending, [0], "every row found room");
//!
//! // The first row's key has two records; the second row's has one.
//! let mut hits = [0];
//! let mut matches = [0; 3];
//! let mut found = RowMask::try_new(3, &mut hits)?;
//! table.probe(&hashes, &keys[..], &RowMaskView::try_new(3, &[0b011])?, &mut matches, &mut found)?;
//! assert_eq!(hits, [0b011]);
//! assert_eq!(table.record(matches[0])?.payload, 3u64.to_ne_bytes());
//! let mut more = RowMask::try_new(3, &mut hits)?;
//! table.next_match(&mut matches, &RowMaskView::try_new(3, &[0b011])?, &mut more)?;
//! assert_eq!(hits, [0b001]);
//! assert_eq!(table.record(matches[0])?.payload, 1u64.to_ne_bytes());
//! # Ok::<(), anyhow::Error>(())
//! ```
#![allow(unsafe_code)]

mod batch;
pub mod bloom;
mod exclusive;
mod header;
mod keys;
mod lanes;
#[cfg(all(test, loom))]
mod loom;
pub mod phases;
mod record;
mod region;

use core::marker::PhantomData;
use core::ops::Deref;

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};

pub use exclusive::{Cursor, Fold, Slot};
use header::{CHUNK_USED, Header, Layout, NRECORDS};
pub use header::{
    FORMAT_VERSION, HEADER_SIZE, KeyKind, MAX_KEYS, TableConfig, VERSION_OFFSET, region_size,
};
pub use keys::{KeySource, KeyValue, normalize_word};
use record::Access;
pub use record::Record;
use region::{RawRegion, Region};

/// What a table holds, for planning and EXPLAIN.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stats {
    /// Records inserted.
    pub records: u64,
    /// Buckets of the table.
    pub buckets: u64,
    /// Bytes in use: the header, the records and the buckets.
    pub bytes_used: u64,
    /// Bytes of the region the table was created or grown over.
    pub region_len: u64,
}

/// A table over a region, as several participants may share it.
///
/// It is neither `Send` nor `Sync`: each participant attaches its own.
#[derive(Debug)]
pub struct Table<'a> {
    region: RawRegion,
    layout: Layout,
    _region: PhantomData<&'a ()>,
}

/// A table over a region that one writer has to itself.
#[derive(Debug)]
pub struct TableMut<'a>(Table<'a>);

impl<'a> Deref for TableMut<'a> {
    type Target = Table<'a>;

    fn deref(&self) -> &Table<'a> {
        &self.0
    }
}

/// Write the header of a new table for `capacity` records and clear its
/// buckets; the layout is returned.
///
/// # Safety
///
/// The caller has the region to itself, so nothing else reads or writes
/// it meanwhile.
unsafe fn init<R: Region>(region: &R, config: &TableConfig<'_>, capacity: u64) -> Result<Layout> {
    let header = Header::new(config, capacity, region.len())?;
    let layout = header.validate(region.len())?;
    header.store(region);
    // SAFETY: the caller's contract.
    unsafe { region.zero_u32(layout.buckets_offset, layout.nbuckets as usize) };
    Ok(layout)
}

/// The bytes of a buffer of words.
fn words_as_bytes(words: &[u64]) -> &[u8] {
    // SAFETY: any initialized `u64` is eight initialized bytes, and `u8`
    // has no alignment requirement.
    unsafe { core::slice::from_raw_parts(words.as_ptr().cast::<u8>(), words.len() * 8) }
}

/// The bytes of a buffer of words, to write.
fn words_as_bytes_mut(words: &mut [u64]) -> &mut [u8] {
    // SAFETY: as in `words_as_bytes`, and any bytes make a `u64`.
    unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), words.len() * 8) }
}

/// Reject a region that could not hold a header.
fn check_region(region: *mut u8, len: usize) -> Result<()> {
    ensure!(
        !region.is_null() && region.addr().is_multiple_of(8),
        "a table region must be aligned to 8 bytes"
    );
    ensure!(
        len >= HEADER_SIZE,
        "a table region of {len} bytes is shorter than the {HEADER_SIZE}-byte header"
    );
    Ok(())
}

impl<'a> Table<'a> {
    /// Attach to the table in the `len` bytes at `region`, checking its
    /// header.
    ///
    /// # Safety
    ///
    /// `region` is aligned to 8 and valid for reads and writes of `len`
    /// bytes for `'a`, and during `'a` the bytes are accessed only through
    /// tables, here or in other processes mapping the same memory, and only
    /// in one phase at a time: insertions or probes.
    pub unsafe fn attach(region: *mut u8, len: usize) -> Result<Self> {
        check_region(region, len)?;
        // SAFETY: the caller's contract.
        let region = unsafe { RawRegion::new(region, len) };
        let layout = Header::load(&region).validate(len)?;
        Ok(Self {
            region,
            layout,
            _region: PhantomData,
        })
    }

    /// The kinds of the keys, in key order.
    pub fn key_kinds(&self) -> &[KeyKind] {
        &self.layout.kinds[..self.layout.nkeys]
    }

    /// Bytes of payload per record.
    pub fn payload_size(&self) -> usize {
        self.layout.payload_size
    }

    /// The counts of the table as of now.
    pub fn stats(&self) -> Stats {
        let buckets = u64::from(self.layout.nbuckets);
        Stats {
            records: self.region.load_u64(NRECORDS),
            buckets,
            bytes_used: self.region.load_u64(CHUNK_USED) + buckets * 4,
            region_len: self.layout.region_len as u64,
        }
    }

    /// Insert the rows of `pending` as new records, in row order, until
    /// the table has no room: each row inserted leaves `pending` and gets
    /// the offset of its record in `offsets`; the count inserted is
    /// returned, and rows still pending need a larger region. `hashes` has
    /// one hash per physical row, `keys` the table's keys, `payload` the
    /// payload of every physical row one after another or `None` for
    /// zeros. Equal keys make separate records.
    pub fn insert<K: KeySource + ?Sized>(
        &self,
        hashes: &[u32],
        keys: &K,
        payload: Option<&[u8]>,
        pending: &mut RowMask<'_>,
        offsets: &mut [u32],
    ) -> Result<usize> {
        batch::insert(
            &self.region,
            &self.layout,
            hashes,
            keys,
            payload,
            pending,
            offsets,
        )
    }

    /// Write the rows of `pending` as this table's records into `buffer`
    /// from byte `*used` on, as long as whole records fit, for
    /// [`Table::insert_staged`] to add later: a participant of a shared
    /// build keeps there the rows a full table had no room for. Written
    /// rows leave `pending`; the count written is returned.
    pub fn stage<K: KeySource + ?Sized>(
        &self,
        buffer: &mut [u64],
        used: &mut usize,
        hashes: &[u32],
        keys: &K,
        payload: Option<&[u8]>,
        pending: &mut RowMask<'_>,
    ) -> Result<usize> {
        batch::stage(
            &self.layout,
            words_as_bytes_mut(buffer),
            used,
            hashes,
            keys,
            payload,
            pending,
        )
    }

    /// Add the records [`Table::stage`] wrote in the first `used` bytes of
    /// `buffer`, from byte `*consumed` on, as long as the table has room;
    /// `*consumed` moves past them and the count added is returned.
    pub fn insert_staged(
        &self,
        buffer: &[u64],
        used: usize,
        consumed: &mut usize,
    ) -> Result<usize> {
        let bytes = words_as_bytes(buffer);
        ensure!(used <= bytes.len(), "{used} staged bytes exceed the buffer");
        batch::insert_staged(&self.region, &self.layout, &bytes[..used], consumed)
    }

    /// Find the first record of its chain with the hash, null bits and
    /// keys of each row of `rows`: `matches[row]` receives its offset and
    /// `found` the rows that have one, as a mask this call produces. The
    /// other records with the same keys follow through
    /// [`Table::next_match`], or [`Table::next_in_group`] in a table
    /// filled by [`TableMut::insert_grouped`].
    pub fn probe<K: KeySource + ?Sized>(
        &self,
        hashes: &[u32],
        keys: &K,
        rows: &RowMaskView<'_>,
        matches: &mut [u32],
        found: &mut RowMask<'_>,
    ) -> Result<()> {
        batch::probe(
            &self.region,
            &self.layout,
            hashes,
            keys,
            rows,
            matches,
            found,
        )
    }

    /// For each row of `rows`, replace `offsets[row]`, a record offset
    /// from a probe or an earlier call, by the offset of the next record
    /// in its chain with the same hash, null bits and keys; `found`
    /// receives the rows that have one, and the others keep their offset.
    pub fn next_match(
        &self,
        offsets: &mut [u32],
        rows: &RowMaskView<'_>,
        found: &mut RowMask<'_>,
    ) -> Result<()> {
        batch::next_match(&self.region, &self.layout, offsets, rows, found)
    }

    /// For each row of `rows`, the 8 bytes at byte `at` of the payload of
    /// the record at `offsets[row]` into `out[row]`, native-endian: one
    /// word of a batch's matches per call, such as a Datum of the build
    /// row a join keeps there. `at + 8` must be within the payload; rows
    /// outside `rows` keep their values in `out`.
    pub fn gather(
        &self,
        offsets: &[u32],
        rows: &RowMaskView<'_>,
        at: usize,
        out: &mut [u64],
    ) -> Result<()> {
        batch::gather(&self.region, &self.layout, offsets, rows, at, out)
    }

    /// For each row of `rows`, replace `offsets[row]` by the record right
    /// after it when that one has the same hash, null bits and keys, and
    /// put the row in `found`; other rows keep their offset. In a table
    /// filled by [`TableMut::insert_grouped`] this is the next record of
    /// the key, found in one step instead of a walk down the chain.
    pub fn next_in_group(
        &self,
        offsets: &mut [u32],
        rows: &RowMaskView<'_>,
        found: &mut RowMask<'_>,
    ) -> Result<()> {
        batch::next_in_group(&self.region, &self.layout, offsets, rows, found)
    }

    /// For each row of `rows`, key `key` of the record at `offsets[row]`:
    /// its slot's bits into `values[row]` (an int4 sign-extended, as its
    /// Datum is, 0 for a NULL) and whether it is NULL into `nulls[row]`.
    /// Rows outside `rows` keep their values.
    pub fn gather_key(
        &self,
        offsets: &[u32],
        rows: &RowMaskView<'_>,
        key: usize,
        values: &mut [u64],
        nulls: &mut [bool],
    ) -> Result<()> {
        batch::gather_key(
            &self.region,
            &self.layout,
            offsets,
            rows,
            key,
            values,
            nulls,
        )
    }

    /// Fill a Bloom filter of [`bloom::words_for`] this table's records
    /// words (or any power of two) with the hash of every record, after
    /// clearing it: a probe row it rejects has no record with its hash.
    /// No insertion may run at the same time, as for a walk.
    pub fn bloom(&self, words: &mut [u64]) -> Result<()> {
        bloom::fill(&self.region, &self.layout, words)
    }

    /// Build a shared filter of this table's records unless another
    /// participant has claimed it: true for the one that built it. Call
    /// it, like [`Table::bloom`], while the table takes no insertions.
    pub fn try_build_bloom(&self, filter: &bloom::SharedFilter<'_>) -> Result<bool> {
        bloom::try_build(&self.region, &self.layout, filter)
    }

    /// The record at an offset a call of this table returned.
    pub fn record(&self, offset: u32) -> Result<Record<'_>> {
        Ok(Access::new(&self.region, &self.layout)
            .locate(offset)?
            .record())
    }
}

impl<'a> TableMut<'a> {
    /// Create an empty table for `capacity` records in the `len` bytes at
    /// `region`, which must be a multiple of 8 and at least
    /// [`region_size`]; all of them are used.
    ///
    /// # Safety
    ///
    /// As [`Table::attach`], and no other table is over the region while
    /// this one exists.
    pub unsafe fn create(
        region: *mut u8,
        len: usize,
        config: &TableConfig<'_>,
        capacity: u64,
    ) -> Result<Self> {
        check_region(region, len)?;
        // SAFETY: the caller's contract.
        let region = unsafe { RawRegion::new(region, len) };
        // SAFETY: the caller has the region to itself.
        let layout = unsafe { init(&region, config, capacity) }?;
        Ok(Self(Table {
            region,
            layout,
            _region: PhantomData,
        }))
    }

    /// Attach as the one writer of the table in the `len` bytes at `region`.
    ///
    /// # Safety
    ///
    /// As [`TableMut::create`].
    pub unsafe fn attach_mut(region: *mut u8, len: usize) -> Result<Self> {
        // SAFETY: the caller's contract.
        unsafe { Table::attach(region, len) }.map(Self)
    }

    /// Create a table in the words of a slice, as [`TableMut::create`].
    pub fn create_in(
        words: &'a mut [u64],
        config: &TableConfig<'_>,
        capacity: u64,
    ) -> Result<Self> {
        let len = words.len() * 8;
        // SAFETY: the slice is borrowed exclusively for `'a`, its storage is
        // aligned to 8 and only this table uses it until it is dropped.
        unsafe { Self::create(words.as_mut_ptr().cast(), len, config, capacity) }
    }

    /// Attach to the table in the words of a slice, as
    /// [`TableMut::attach_mut`].
    pub fn exclusive(words: &'a mut [u64]) -> Result<Self> {
        let len = words.len() * 8;
        // SAFETY: as for `create_in`.
        unsafe { Self::attach_mut(words.as_mut_ptr().cast(), len) }
    }

    /// Give each row of `pending` the record of its keys, creating one
    /// with a zero payload where none exists, in row order, until the
    /// table has no room for a new one: resolved rows leave `pending` and
    /// get their record offsets in `offsets`, the rows whose record this
    /// call created form `inserted`, and the count resolved is returned.
    /// Rows left pending need a larger region.
    pub fn find_or_insert<K: KeySource + ?Sized>(
        &mut self,
        hashes: &[u32],
        keys: &K,
        pending: &mut RowMask<'_>,
        offsets: &mut [u32],
        inserted: &mut RowMask<'_>,
    ) -> Result<usize> {
        exclusive::find_or_insert(
            &self.0.region,
            &self.0.layout,
            hashes,
            keys,
            pending,
            offsets,
            inserted,
        )
    }

    /// Insert the rows of `pending` as [`Table::insert`] does, but each
    /// right after a record with the same keys when the table holds one,
    /// so that a key's records lie next to each other and
    /// [`Table::next_in_group`] steps through them; `duplicates` receives
    /// the rows whose keys were there already. A lookup per row, and one
    /// writer: the parallel build of a shared table uses
    /// [`Table::insert`].
    pub fn insert_grouped<K: KeySource + ?Sized>(
        &mut self,
        hashes: &[u32],
        keys: &K,
        payload: Option<&[u8]>,
        pending: &mut RowMask<'_>,
        offsets: &mut [u32],
        duplicates: &mut RowMask<'_>,
    ) -> Result<usize> {
        exclusive::insert_grouped(
            &self.0.region,
            &self.0.layout,
            hashes,
            keys,
            payload,
            pending,
            offsets,
            duplicates,
        )
    }

    /// The payload of the record at an offset, to change in place.
    pub fn payload_mut(&mut self, offset: u32) -> Result<&mut [u8]> {
        exclusive::payload_mut(&self.0.region, &self.0.layout, offset)
    }

    /// Add one to the `i64` at byte `at` of the payload of each selected
    /// row's record: `count(*)` of a grouped aggregate, whose rows hold
    /// the offsets [`TableMut::find_or_insert`] gave them.
    pub fn count_rows(&mut self, offsets: &[u32], rows: &RowMaskView<'_>, at: usize) -> Result<()> {
        exclusive::count_rows(&self.0.region, &self.0.layout, offsets, rows, at)
    }

    /// Add one to the `i64` at byte `at` for each selected row whose
    /// value in `column` is not NULL: `count(x)`.
    pub fn count_values<C: ColumnReader + ?Sized>(
        &mut self,
        offsets: &[u32],
        rows: &RowMaskView<'_>,
        column: &C,
        at: usize,
    ) -> Result<()> {
        exclusive::count_values(&self.0.region, &self.0.layout, offsets, rows, column, at)
    }

    /// Fold each selected row's non-NULL value into the aggregate state
    /// at `slot` of its record's payload, in row order; the state's flag
    /// marks that it has a value. A sum that overflows an `i64` fails
    /// with [`crate::ops::ArithmeticError::BigintOutOfRange`].
    pub fn fold<C, V>(
        &mut self,
        offsets: &[u32],
        rows: &RowMaskView<'_>,
        column: &C,
        fold: Fold,
        slot: Slot,
    ) -> Result<()>
    where
        C: ColumnReader<Value = V> + ?Sized,
        V: Into<i64> + Copy,
    {
        exclusive::fold(
            &self.0.region,
            &self.0.layout,
            offsets,
            rows,
            column,
            fold,
            slot,
        )
    }

    /// Visit the records from `cursor` on, in insertion order, as many as
    /// `out` holds: their offsets fill `out`, the count is returned and
    /// the cursor moves past them; 0 means the walk is over.
    pub fn scan(&self, cursor: &mut Cursor, out: &mut [u32]) -> Result<usize> {
        exclusive::scan(&self.0.region, &self.0.layout, cursor, out)
    }

    /// Grow the table to the first `new_len` bytes of its region, after
    /// the caller made the region that large with the used bytes intact
    /// (`repalloc`, or a copy into a new region): the buckets are rebuilt
    /// at the new end for the records that could now fit, and records and
    /// their offsets stay as they were. `new_len` is a multiple of 8, at
    /// least the old length and at most the region's.
    pub fn grow(&mut self, new_len: usize) -> Result<()> {
        self.0.layout = exclusive::grow(&self.0.region, &self.0.layout, new_len)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::int32::murmurhash32;
    use tessera_core::ColumnView;

    /// A region's base, which the threads of a test attach to.
    struct Base(*mut u8);
    // SAFETY: every thread accesses the region only through tables.
    unsafe impl Sync for Base {}

    impl Base {
        fn get(&self) -> *mut u8 {
            self.0
        }
    }

    #[test]
    fn one_of_four_threads_builds_a_shared_filter_every_key_passes() {
        let config = TableConfig {
            keys: &[KeyKind::Int32],
            payload_size: 0,
        };
        let count = 200;
        let mut region = vec![0_u64; region_size(&config, count).unwrap() / 8];
        let len = region.len() * 8;
        let base = Base(region.as_mut_ptr().cast::<u8>());
        let keys: Vec<i32> = (0..count as i32).map(|key| key * 7).collect();
        let hashes: Vec<u32> = keys.iter().map(|&key| murmurhash32(key as u32)).collect();
        let column = [ColumnView::try_new(&keys, None).unwrap()];
        let all = vec![u64::MAX; 4];
        let mut all_rows = all.clone();
        all_rows[3] = (1 << (count - 192)) - 1;
        {
            // SAFETY: the vector is aligned to 8, and this table alone uses it.
            let table = unsafe { TableMut::create(base.0, len, &config, count) }.unwrap();
            let mut pending_words = all_rows.clone();
            let mut pending = RowMask::try_new(count as usize, &mut pending_words).unwrap();
            let mut offsets = vec![0; count as usize];
            let inserted = table
                .insert(&hashes, &column[..], None, &mut pending, &mut offsets)
                .unwrap();
            assert_eq!(inserted, count as usize);
        }
        let mut words = vec![0; bloom::shared_words_for(count).unwrap()];
        let filter = bloom::SharedFilter::from_mut(&mut words).unwrap();
        filter.init();
        let rows = RowMaskView::try_new(count as usize, &all_rows).unwrap();
        let mut found_words = [0; 4];
        let mut found = RowMask::try_new(count as usize, &mut found_words).unwrap();
        assert!(bloom::probe_shared(&filter, &hashes, &rows, &mut found).is_err());
        let built: usize = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        // SAFETY: the table is built, and every thread only
                        // reads it through a table of its own.
                        let table = unsafe { Table::attach(base.get(), len) }.unwrap();
                        let built = table.try_build_bloom(&filter).unwrap();
                        while !filter.ready() {
                            std::thread::yield_now();
                        }
                        let mut found_words = [0; 4];
                        let mut found = RowMask::try_new(count as usize, &mut found_words).unwrap();
                        bloom::probe_shared(&filter, &hashes, &rows, &mut found).unwrap();
                        assert_eq!(found_words, all_rows.as_slice(), "a key was rejected");
                        usize::from(built)
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .sum()
        });
        assert_eq!(built, 1);
    }

    #[test]
    fn a_misaligned_or_odd_region_is_refused() {
        let config = TableConfig {
            keys: &[KeyKind::Int32],
            payload_size: 0,
        };
        let mut words = vec![0_u64; region_size(&config, 1000).unwrap().div_ceil(8) + 2];
        let len = words.len() * 8 - 16;
        let base = words.as_mut_ptr().cast::<u8>();
        // SAFETY: `base + 4` and `base + 8` with `len` bytes lie inside the
        // vector, which nothing else uses meanwhile.
        unsafe {
            let misaligned = base.add(4);
            assert!(TableMut::create(misaligned, len, &config, 1000).is_err());
            assert!(Table::attach(misaligned, len).is_err());
            let odd = TableMut::create(base, len - 4, &config, 1000);
            assert!(odd.unwrap_err().to_string().contains("multiple of 8"));
            assert!(TableMut::create(base.add(8), len, &config, 1000).is_ok());
        }
    }
}

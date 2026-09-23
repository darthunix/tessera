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
//! newest record hashed into it. Hashes come from [`crate::int32::hash`]
//! and [`crate::int32::hash_next`], which decide what NULL keys do.
//!
//! [`Table`] is the access several participants may share: inserting
//! ([`Table::insert`], which always adds a record, so equal keys chain) or
//! probing ([`Table::probe`] for the first record with a row's hash and
//! keys, [`Table::next_match`] for the ones after it), not both at a time.
//! [`TableMut`] is the access of one writer, which alone may change
//! records, walk them or grow the region. A batch brings its hashes, its
//! keys through a [`KeySource`] and a row mask, and gets record offsets
//! back. Every call attaches anew and checks the whole header; every
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
//! table.next_match(&matches, &RowMaskView::try_new(3, &[0b011])?, &mut offsets, &mut more)?;
//! assert_eq!(hits, [0b001]);
//! assert_eq!(table.record(offsets[0])?.payload, 1u64.to_ne_bytes());
//! # Ok::<(), anyhow::Error>(())
//! ```
#![allow(unsafe_code)]

mod batch;
mod header;
mod keys;
mod record;
mod region;

use core::marker::PhantomData;
use core::ops::Deref;

use anyhow::{Result, ensure};
use tessera_core::{RowMask, RowMaskView};

use header::{CHUNK_USED, HEADER_SIZE, Header, Layout, NRECORDS};
pub use header::{FORMAT_VERSION, KeyKind, MAX_KEYS, TableConfig, region_size};
pub use keys::{KeySource, normalize_word};
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

    /// Find the newest record with the hash, null bits and keys of each
    /// row of `rows`: `matches[row]` receives its offset and `found` the
    /// rows that have one, as a mask this call produces. Older records
    /// with the same keys follow through [`Table::next_match`].
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

    /// For each row of `rows`, find the record after `current[row]` with
    /// the same hash, null bits and keys: `next[row]` receives its offset
    /// and `found` the rows that have one. `current` holds record offsets
    /// from a probe or an earlier call.
    pub fn next_match(
        &self,
        current: &[u32],
        rows: &RowMaskView<'_>,
        next: &mut [u32],
        found: &mut RowMask<'_>,
    ) -> Result<()> {
        batch::next_match(&self.region, &self.layout, current, rows, next, found)
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
        let header = Header::new(config, capacity, len)?;
        let layout = header.validate(len)?;
        // SAFETY: the caller's contract.
        let region = unsafe { RawRegion::new(region, len) };
        header.store(&region);
        // SAFETY: the caller has the region to itself, so nothing else reads
        // or writes the buckets while they are cleared.
        unsafe { region.bytes_mut(layout.buckets_offset, layout.nbuckets as usize * 4) }.fill(0);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_misaligned_or_odd_region_is_refused() {
        let config = TableConfig {
            keys: &[KeyKind::Int32],
            payload_size: 0,
        };
        let mut words = vec![0; region_size(&config, 1000).unwrap().div_ceil(8) + 2];
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

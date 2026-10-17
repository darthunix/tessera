//! The header at the start of a table's index, the sizes it fixes, and
//! the limits of chunks and references.
//!
//! ```text
//!   index: header (96 bytes), then the buckets, 4 bytes each
//!   ┌──────────────────────────┬──────────┬──────────┬─────┬──────────┐
//!   │ header: the format, the  │ bucket 0 │ bucket 1 │  …  │ bucket n │
//!   │ sizes, the record count  │          │          │     │          │
//!   └──────────────────────────┴──────────┴──────────┴─────┴──────────┘
//! ```
//!
//! The header is integers only, so any bytes read as some header, and a
//! check of it is a comparison of numbers. Its fields lie in the order of
//! [`Header`]; the spec of `hash-table` draws them with their offsets in
//! its requirement "The index of a table". A call checks the header once
//! and copies it into a [`Layout`], which its loops use. Only the count
//! of records changes after the index is made; it is read and written
//! atomically, through the region.
//!
//! A chunk is a block of at most [`MAX_CHUNK_LEN`] bytes: its used mark,
//! then records one after another. A [`Place`] is where a record lies,
//! and a reference is a place packed into 32 bits.

use core::mem::{offset_of, size_of};

use anyhow::{Result, bail, ensure};

use super::region::Region;

/// The format of the table this code writes and accepts.
pub const FORMAT_VERSION: u32 = 2;
/// The most keys a record holds.
pub const MAX_KEYS: usize = 16;
/// The first eight bytes of a region: `TESSTABL` in ASCII.
const MAGIC: u64 = u64::from_le_bytes(*b"TESSTABL");
/// The fewest buckets a table has, as pg_batch chooses them.
const MIN_BUCKETS: u64 = 1024;
/// The most buckets: the count stays below 2^32.
const MAX_BUCKETS: u64 = 1 << 31;
/// Bytes of a record before its keys: hash, next, null bits, length.
pub(super) const RECORD_HEADER: usize = 16;
/// Bytes of one key slot.
pub(super) const KEY_SLOT: usize = 8;
/// Bytes of the header, where the buckets start.
pub const HEADER_SIZE: usize = size_of::<Header>();
/// Byte offset of the format version in the header.
pub const VERSION_OFFSET: usize = offset_of!(Header, version);
/// Offset of the record count.
pub(super) const NRECORDS: usize = offset_of!(Header, nrecords);
/// Bits of a record reference that hold its offset in its chunk, in
/// 8-byte units.
pub const UNIT_BITS: u32 = 17;
/// The most bytes a chunk has: its units fit [`UNIT_BITS`].
pub const MAX_CHUNK_LEN: usize = 8 << UNIT_BITS;
/// The most chunks a table has: their numbers fit the other bits.
pub const MAX_CHUNKS: usize = 1 << (32 - UNIT_BITS);
/// Bytes of a chunk before its first record: its used mark.
pub const CHUNK_HEADER: usize = 8;

/// Where a record lies: the number of its chunk and its first byte in
/// the chunk. A reference is the same two numbers packed into 32 bits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Place {
    pub(super) chunk: usize,
    pub(super) byte: usize,
}

impl Place {
    /// The place a reference names.
    #[inline(always)]
    pub(super) fn of(reference: u32) -> Self {
        Self {
            chunk: (reference >> UNIT_BITS) as usize,
            byte: (reference & ((1 << UNIT_BITS) - 1)) as usize * 8,
        }
    }

    /// The reference of the record at this place.
    #[inline(always)]
    pub(super) fn reference(self) -> u32 {
        ((self.chunk as u32) << UNIT_BITS) | (self.byte / 8) as u32
    }
}

/// What a table is created with: its keys and the payload of every record.
/// A key is a word of 8 bytes, compared bit for bit: the table gives a
/// word no type, and the caller's source of keys puts each value in its
/// word.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TableConfig {
    /// The keys of a record; one to [`MAX_KEYS`].
    pub nkeys: usize,
    /// Bytes of payload per record, opaque to the table.
    pub payload_size: usize,
}

/// The header as it lies in the region.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(super) struct Header {
    magic: u64,
    version: u32,
    header_size: u32,
    region_len: u64,
    buckets_offset: u64,
    reserved_used: u64,
    nrecords: u64,
    nbuckets: u32,
    bucket_shift: u32,
    record_size: u32,
    payload_size: u32,
    nkeys: u32,
    flags: u32,
    reserved: [u64; 3],
}

const _: () = assert!(size_of::<Header>() == 96);

/// The validated, unchanging part of a header.
#[derive(Clone, Copy, Debug)]
pub(super) struct Layout {
    pub region_len: usize,
    pub buckets_offset: usize,
    pub nbuckets: u32,
    pub bucket_shift: u32,
    pub record_size: usize,
    pub payload_size: usize,
    pub nkeys: usize,
}

impl Layout {
    /// The most records that find or insert creates in the table: half its
    /// buckets, so that a chain stays short; more rows wait for a larger
    /// index.
    #[inline(always)]
    pub(super) fn most_records(&self) -> u64 {
        u64::from(self.nbuckets / 2)
    }

    /// The 8-byte words of a record after its keys: payload and padding.
    pub(super) fn tail_words(&self) -> usize {
        self.record_size / 8 - RECORD_HEADER / 8 - self.nkeys
    }

    /// A fingerprint of what a record is: the format, the count of keys,
    /// the record and payload sizes (FNV-1a). A table that spills writes
    /// its records' bytes to disk, and the blocks of the file carry this
    /// number; a table reads a block back only when the number is its own,
    /// so that the bytes of another table or another format are refused
    /// rather than read as records. The crate computes it, since only the
    /// crate knows what a record's bytes are.
    pub(super) fn fingerprint(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        let mut feed = |byte: u8| {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        };
        FORMAT_VERSION.to_le_bytes().into_iter().for_each(&mut feed);
        (self.nkeys as u32)
            .to_le_bytes()
            .into_iter()
            .for_each(&mut feed);
        (self.record_size as u64)
            .to_le_bytes()
            .into_iter()
            .for_each(&mut feed);
        (self.payload_size as u64)
            .to_le_bytes()
            .into_iter()
            .for_each(&mut feed);
        hash
    }
}

/// Bytes the index of a table for `capacity` records takes: the header
/// and the buckets, a multiple of 8.
pub fn index_size(config: &TableConfig, capacity: u64) -> Result<usize> {
    check_config(config)?;
    Ok(HEADER_SIZE + bucket_count(capacity) as usize * 4)
}

/// Bytes of one record of a table of this configuration.
pub fn record_bytes(config: &TableConfig) -> Result<usize> {
    Ok(check_config(config)? as usize)
}

/// The record size of a configuration, after checking its count of keys.
fn check_config(config: &TableConfig) -> Result<u32> {
    check_nkeys(config.nkeys)?;
    record_size(config.nkeys, config.payload_size)
}

/// Refuse a count of keys a table cannot have, given or read.
fn check_nkeys(nkeys: usize) -> Result<()> {
    ensure!(
        (1..=MAX_KEYS).contains(&nkeys),
        "a table has 1 to {MAX_KEYS} keys, not {nkeys}"
    );
    Ok(())
}

/// Refuse a size that no record of a table has. A record is a multiple
/// of 8, a header and a key at least, and no more than a chunk holds
/// after its used mark; a size between the records would name places
/// where no record starts.
pub(super) fn check_record_size(size: usize) -> Result<()> {
    ensure!(
        record_size_fits(size),
        "records of {size} bytes are not the records of a table"
    );
    Ok(())
}

/// Whether `size` bytes can be the records of a table, as
/// [`check_record_size`] says.
#[inline(always)]
fn record_size_fits(size: usize) -> bool {
    size.is_multiple_of(8)
        && (RECORD_HEADER + KEY_SLOT..=MAX_CHUNK_LEN - CHUNK_HEADER).contains(&size)
}

/// Bytes of a record: its header, the key slots and the payload, rounded
/// up to 8, at most what a chunk has room for after its used mark: a
/// larger record could never be appended. A table is not made for one;
/// a header that claims one is read as [`record_size_of`] reads it, and
/// every reference of such a table is refused, since no record fits.
fn record_size(nkeys: usize, payload_size: usize) -> Result<u32> {
    let size = record_size_of(nkeys, payload_size).filter(|&size| record_size_fits(size as usize));
    let Some(size) = size else {
        bail!(
            "a record of {nkeys} keys and a payload of {payload_size} bytes \
             does not fit in a chunk of {MAX_CHUNK_LEN} bytes"
        );
    };
    Ok(size)
}

/// Bytes of a record as [`record_size`] computes them, without the bound
/// of a chunk: `None` past the 32 bits a header holds.
#[inline]
fn record_size_of(nkeys: usize, payload_size: usize) -> Option<u32> {
    payload_size
        .checked_add(RECORD_HEADER + KEY_SLOT * nkeys)
        .and_then(|size| size.checked_add(7))
        .map(|size| size & !7)
        .and_then(|size| u32::try_from(size).ok())
}

/// The bucket count for `capacity` records: a power of two of at least
/// [`MIN_BUCKETS`] and at least twice the capacity.
fn bucket_count(capacity: u64) -> u32 {
    let wanted = capacity
        .saturating_mul(2)
        .clamp(MIN_BUCKETS, MAX_BUCKETS)
        .next_power_of_two();
    // A power of two of at most MAX_BUCKETS, which a u32 holds.
    const _: () = assert!(MAX_BUCKETS <= u32::MAX as u64);
    wanted as u32
}

/// The shift that turns a hash into a bucket index: its high bits.
fn bucket_shift(nbuckets: u32) -> u32 {
    32 - nbuckets.trailing_zeros()
}

/// The layout of a configuration's records, for appending to chunks
/// before the table has an index: the bucket fields are the smallest
/// index's, which appending does not read.
pub(super) fn chunk_layout(config: &TableConfig) -> Result<Layout> {
    let len = index_size(config, 0)?;
    Header::new(config, 0, len)?.validate(len)
}

impl Header {
    /// The header of a new table's index of `len` bytes.
    pub(super) fn new(config: &TableConfig, capacity: u64, len: usize) -> Result<Self> {
        let record_size = check_config(config)?;
        ensure!(
            len.is_multiple_of(8),
            "a table index of {len} bytes is not a multiple of 8"
        );
        let needed = index_size(config, capacity)?;
        ensure!(
            len >= needed,
            "a table index of {len} bytes is smaller than the {needed} bytes \
             that {capacity} records need"
        );
        let nbuckets = bucket_count(capacity);
        Ok(Self {
            magic: MAGIC,
            version: FORMAT_VERSION,
            header_size: HEADER_SIZE as u32,
            region_len: len as u64,
            buckets_offset: HEADER_SIZE as u64,
            reserved_used: 0,
            nrecords: 0,
            nbuckets,
            bucket_shift: bucket_shift(nbuckets),
            record_size,
            payload_size: config.payload_size as u32,
            nkeys: config.nkeys as u32,
            flags: 0,
            reserved: [0; 3],
        })
    }

    /// The header of a larger index of `len` bytes for `capacity` records,
    /// with no record linked yet: the layout stays, the buckets are the
    /// ones [`index_size`] gives the capacity.
    pub(super) fn with_capacity(&self, capacity: u64, len: usize) -> Result<Self> {
        let nbuckets = bucket_count(capacity);
        ensure!(
            len.is_multiple_of(8) && HEADER_SIZE + nbuckets as usize * 4 <= len,
            "a table index of {len} bytes cannot hold {nbuckets} buckets"
        );
        ensure!(
            nbuckets >= self.nbuckets,
            "a table index does not shrink from {} buckets",
            self.nbuckets
        );
        Ok(Self {
            region_len: len as u64,
            nrecords: 0,
            nbuckets,
            bucket_shift: bucket_shift(nbuckets),
            ..*self
        })
    }

    /// Read the header at the start of a region: the counters with acquire
    /// ordering, since insertions in other processes move them; the other
    /// fields change only under one writer, before the table is shared,
    /// and are read without ordering.
    pub(super) fn load<R: Region>(region: &R) -> Self {
        let reserved = offset_of!(Header, reserved);
        // The count before the used mark: an insertion reserves room, then
        // counts, so a count read with acquire is covered by any used mark
        // read after it, and a header read while others insert still has
        // no more records than used bytes.
        let nrecords = region.load_u64(NRECORDS);
        Self {
            magic: region.load_u64_relaxed(offset_of!(Header, magic)),
            version: region.load_u32_relaxed(offset_of!(Header, version)),
            header_size: region.load_u32_relaxed(offset_of!(Header, header_size)),
            region_len: region.load_u64_relaxed(offset_of!(Header, region_len)),
            buckets_offset: region.load_u64_relaxed(offset_of!(Header, buckets_offset)),
            reserved_used: region.load_u64_relaxed(offset_of!(Header, reserved_used)),
            nrecords,
            nbuckets: region.load_u32_relaxed(offset_of!(Header, nbuckets)),
            bucket_shift: region.load_u32_relaxed(offset_of!(Header, bucket_shift)),
            record_size: region.load_u32_relaxed(offset_of!(Header, record_size)),
            payload_size: region.load_u32_relaxed(offset_of!(Header, payload_size)),
            nkeys: region.load_u32_relaxed(offset_of!(Header, nkeys)),
            flags: region.load_u32_relaxed(offset_of!(Header, flags)),
            reserved: [0, 8, 16].map(|at| region.load_u64_relaxed(reserved + at)),
        }
    }

    /// Write the header at the start of a region.
    pub(super) fn store<R: Region>(&self, region: &R) {
        region.store_u64(offset_of!(Header, magic), self.magic);
        region.store_u32(offset_of!(Header, version), self.version);
        region.store_u32(offset_of!(Header, header_size), self.header_size);
        region.store_u64(offset_of!(Header, region_len), self.region_len);
        region.store_u64(offset_of!(Header, buckets_offset), self.buckets_offset);
        region.store_u64(offset_of!(Header, reserved_used), self.reserved_used);
        region.store_u64(NRECORDS, self.nrecords);
        region.store_u32(offset_of!(Header, nbuckets), self.nbuckets);
        region.store_u32(offset_of!(Header, bucket_shift), self.bucket_shift);
        region.store_u32(offset_of!(Header, record_size), self.record_size);
        region.store_u32(offset_of!(Header, payload_size), self.payload_size);
        region.store_u32(offset_of!(Header, nkeys), self.nkeys);
        region.store_u32(offset_of!(Header, flags), self.flags);
        for (word, &value) in self.reserved.iter().enumerate() {
            region.store_u64(offset_of!(Header, reserved) + 8 * word, value);
        }
    }

    /// Check every field against the format and the `len` bytes given, and
    /// return the layout the table works with.
    ///
    /// Every call of the table checks the header, so the checks run as
    /// plain comparisons; the messages are built only when one fails, by
    /// the same checks in [`Self::explain`], kept out of line.
    #[inline]
    pub(super) fn validate(&self, len: usize) -> Result<Layout> {
        match self.layout_if_valid(len) {
            Some(layout) => Ok(layout),
            None => Err(self.invalid(len)),
        }
    }

    /// The layout when every check of [`Self::explain`] passes.
    #[inline]
    fn layout_if_valid(&self, len: usize) -> Option<Layout> {
        let region_len = usize::try_from(self.region_len).ok()?;
        let nkeys = self.nkeys as usize;
        let payload_size = self.payload_size as usize;
        let nbuckets = self.nbuckets;
        let buckets_offset = usize::try_from(self.buckets_offset).ok()?;
        let buckets_end = buckets_offset.checked_add(nbuckets as usize * 4)?;
        let fixed = self.magic == MAGIC
            && self.version == FORMAT_VERSION
            && self.header_size as usize == HEADER_SIZE
            && self.flags == 0
            && region_len <= len
            && region_len.is_multiple_of(8)
            && region_len >= HEADER_SIZE
            && (1..=MAX_KEYS).contains(&nkeys);
        if !fixed {
            return None;
        }
        let valid = self.record_size == record_size_of(nkeys, payload_size)?
            && nbuckets.is_power_of_two()
            && u64::from(nbuckets) >= MIN_BUCKETS
            && self.bucket_shift == bucket_shift(nbuckets)
            && buckets_offset == HEADER_SIZE
            && buckets_end <= region_len
            && self.reserved_used == 0
            && self.reserved == [0; 3];
        valid.then_some(Layout {
            region_len,
            buckets_offset,
            nbuckets,
            bucket_shift: self.bucket_shift,
            record_size: self.record_size as usize,
            payload_size,
            nkeys,
        })
    }

    /// The error of a header the fast checks refused.
    #[cold]
    #[inline(never)]
    fn invalid(&self, len: usize) -> anyhow::Error {
        match self.explain(len) {
            Err(error) => error,
            Ok(_) => anyhow::anyhow!("the table header is inconsistent"),
        }
    }

    /// The checks of [`Self::validate`], each with its message.
    fn explain(&self, len: usize) -> Result<Layout> {
        ensure!(self.magic == MAGIC, "the index does not hold a table");
        ensure!(
            self.version == FORMAT_VERSION,
            "table format version {} is not the supported {FORMAT_VERSION}",
            self.version
        );
        ensure!(
            self.header_size as usize == HEADER_SIZE,
            "table header size {} is not {HEADER_SIZE}",
            self.header_size
        );
        ensure!(
            self.flags == 0,
            "table header has unknown flags {:#x}",
            self.flags
        );
        let region_len = usize::try_from(self.region_len).ok();
        let Some(region_len) = region_len.filter(|&n| n <= len) else {
            bail!(
                "table index length {} exceeds the {len} bytes given",
                self.region_len
            );
        };
        ensure!(
            region_len.is_multiple_of(8) && region_len >= HEADER_SIZE,
            "table index length {region_len} is not a multiple of 8 past the header"
        );
        let nkeys = self.nkeys as usize;
        check_nkeys(nkeys)?;
        let payload_size = self.payload_size as usize;
        ensure!(
            Some(self.record_size) == record_size_of(nkeys, payload_size),
            "table record size {} does not match {nkeys} keys and a payload of \
             {payload_size} bytes",
            self.record_size
        );
        let nbuckets = self.nbuckets;
        ensure!(
            nbuckets.is_power_of_two() && u64::from(nbuckets) >= MIN_BUCKETS,
            "table bucket count {nbuckets} is not a power of two of at least {MIN_BUCKETS}"
        );
        ensure!(
            self.bucket_shift == bucket_shift(nbuckets),
            "table bucket shift {} does not match {nbuckets} buckets",
            self.bucket_shift
        );
        let buckets_offset = usize::try_from(self.buckets_offset).ok();
        let buckets_end = buckets_offset.and_then(|off| off.checked_add(nbuckets as usize * 4));
        let (Some(buckets_offset), Some(buckets_end)) = (buckets_offset, buckets_end) else {
            bail!("table buckets lie outside the region");
        };
        ensure!(
            buckets_offset == HEADER_SIZE && buckets_end <= region_len,
            "table buckets at {buckets_offset} lie outside the index of {region_len} bytes"
        );
        ensure!(
            self.reserved_used == 0 && self.reserved == [0; 3],
            "table header has a reserved word set"
        );
        Ok(Layout {
            region_len,
            buckets_offset,
            nbuckets,
            bucket_shift: self.bucket_shift,
            record_size: self.record_size as usize,
            payload_size,
            nkeys,
        })
    }
}

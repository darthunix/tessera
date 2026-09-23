//! The header at the start of a region and the layout it fixes.
//!
//! The header is 96 bytes of integers, so any bytes decode to a value and
//! every check is an explicit comparison. Its fields, by offset: `magic`
//! (0), `version` (8), `header_size` (12), `region_len` (16),
//! `buckets_offset` (24), `chunk_used` (32), `nrecords` (40), `nbuckets`
//! (48), `bucket_shift` (52), `record_size` (56), `payload_size` (60),
//! `nkeys` (64), `flags` (68), `kinds` (72, one byte per key) and a reserved
//! word (88). Only `chunk_used` and `nrecords` change after creation; they
//! are read and written atomically through the region, the rest is copied
//! into a [`Layout`] once validated.

use core::mem::{offset_of, size_of};

use anyhow::{Result, bail, ensure};

use super::region::Region;

/// The format of the region this code writes and accepts.
pub const FORMAT_VERSION: u32 = 1;
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
/// Bytes of the header, where the first record starts.
pub const HEADER_SIZE: usize = size_of::<Header>();
/// Byte offset of the format version in the header.
pub const VERSION_OFFSET: usize = offset_of!(Header, version);
/// Offset of the first free byte of the record area.
pub(super) const CHUNK_USED: usize = offset_of!(Header, chunk_used);
/// Offset of the record count.
pub(super) const NRECORDS: usize = offset_of!(Header, nrecords);

/// What a key column holds; the record stores every key in an 8-byte slot.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyKind {
    /// An int4, sign-extended into its slot.
    Int32 = 1,
    /// An int8.
    Int64 = 2,
}

impl KeyKind {
    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Int32),
            2 => Some(Self::Int64),
            _ => None,
        }
    }
}

/// What a table is created with: its keys and the payload of every record.
#[derive(Clone, Copy, Debug)]
pub struct TableConfig<'a> {
    /// The kinds of the keys, in key order; one to [`MAX_KEYS`].
    pub keys: &'a [KeyKind],
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
    chunk_used: u64,
    nrecords: u64,
    nbuckets: u32,
    bucket_shift: u32,
    record_size: u32,
    payload_size: u32,
    nkeys: u32,
    flags: u32,
    kinds: [u8; MAX_KEYS],
    reserved: u64,
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
    pub kinds: [KeyKind; MAX_KEYS],
}

impl Layout {
    /// The 8-byte words of a record after its keys: payload and padding.
    pub(super) fn tail_words(&self) -> usize {
        self.record_size / 8 - RECORD_HEADER / 8 - self.nkeys
    }
}

/// Bytes a region needs for a table of `capacity` records: the header,
/// the records and the buckets, a multiple of 8.
pub fn region_size(config: &TableConfig<'_>, capacity: u64) -> Result<usize> {
    let record_size = check_config(config)?;
    let total = capacity
        .checked_mul(u64::from(record_size))
        .and_then(|records| records.checked_add(HEADER_SIZE as u64))
        .and_then(|used| used.checked_add(u64::from(bucket_count(capacity)) * 4))
        .and_then(|total| usize::try_from(total).ok())
        .filter(|total| total.checked_add(8).is_some());
    let Some(total) = total else {
        bail!("a table of {capacity} records does not fit in memory");
    };
    Ok(total)
}

/// The record size of a configuration, after checking it.
fn check_config(config: &TableConfig<'_>) -> Result<u32> {
    let nkeys = config.keys.len();
    ensure!(
        (1..=MAX_KEYS).contains(&nkeys),
        "a table has 1 to {MAX_KEYS} keys, not {nkeys}"
    );
    record_size(nkeys, config.payload_size)
}

/// Bytes of a record: its header, the key slots and the payload, rounded
/// up to 8.
fn record_size(nkeys: usize, payload_size: usize) -> Result<u32> {
    let size = payload_size
        .checked_add(RECORD_HEADER + KEY_SLOT * nkeys)
        .and_then(|size| size.checked_add(7))
        .map(|size| size & !7)
        .and_then(|size| u32::try_from(size).ok());
    let Some(size) = size else {
        bail!("a payload of {payload_size} bytes exceeds the record size limit");
    };
    Ok(size)
}

/// The bucket count for `capacity` records: a power of two of at least
/// [`MIN_BUCKETS`] and at least twice the capacity.
fn bucket_count(capacity: u64) -> u32 {
    let wanted = capacity
        .saturating_mul(2)
        .clamp(MIN_BUCKETS, MAX_BUCKETS)
        .next_power_of_two();
    u32::try_from(wanted).expect("bucket count fits a u32")
}

/// The shift that turns a hash into a bucket index: its high bits.
fn bucket_shift(nbuckets: u32) -> u32 {
    32 - nbuckets.trailing_zeros()
}

impl Header {
    /// The header of a new table over `len` bytes.
    pub(super) fn new(config: &TableConfig<'_>, capacity: u64, len: usize) -> Result<Self> {
        let record_size = check_config(config)?;
        ensure!(
            len.is_multiple_of(8),
            "a table region of {len} bytes is not a multiple of 8"
        );
        let needed = region_size(config, capacity)?;
        ensure!(
            len >= needed,
            "a table region of {len} bytes is smaller than the {needed} bytes \
             that {capacity} records need"
        );
        let nbuckets = bucket_count(capacity);
        let mut kinds = [0; MAX_KEYS];
        for (slot, kind) in kinds.iter_mut().zip(config.keys) {
            *slot = *kind as u8;
        }
        Ok(Self {
            magic: MAGIC,
            version: FORMAT_VERSION,
            header_size: HEADER_SIZE as u32,
            region_len: len as u64,
            buckets_offset: (len - nbuckets as usize * 4) as u64,
            chunk_used: HEADER_SIZE as u64,
            nrecords: 0,
            nbuckets,
            bucket_shift: bucket_shift(nbuckets),
            record_size,
            payload_size: config.payload_size as u32,
            nkeys: config.keys.len() as u32,
            flags: 0,
            kinds,
            reserved: 0,
        })
    }

    /// The header after growing from `old_len` to `new_len` bytes: the
    /// bucket count that [`region_size`] would give the records that fit
    /// beside the buckets (the largest power of two below four times
    /// them, at least the floor), fewer if the used part leaves no room
    /// for them, and the buckets at the new end.
    pub(super) fn grown(&self, old_len: usize, new_len: usize) -> Result<Self> {
        ensure!(
            new_len.is_multiple_of(8) && new_len >= old_len,
            "a table grows to a multiple of 8 bytes of at least its {old_len}"
        );
        let used = usize::try_from(self.chunk_used).unwrap_or(usize::MAX);
        let record_size = (self.record_size as usize).max(1);
        let free = new_len - HEADER_SIZE;
        let mut nbuckets = bucket_count((free / record_size) as u64);
        loop {
            let room = free.saturating_sub(nbuckets as usize * 4) / record_size;
            let fits = nbuckets as usize * 4 + used <= new_len;
            if u64::from(nbuckets) <= MIN_BUCKETS || (fits && room * 4 > nbuckets as usize) {
                break;
            }
            nbuckets /= 2;
        }
        ensure!(
            nbuckets as usize * 4 + used <= new_len,
            "a table of {used} used bytes does not fit {new_len} bytes with its buckets"
        );
        Ok(Self {
            region_len: new_len as u64,
            buckets_offset: (new_len - nbuckets as usize * 4) as u64,
            nbuckets,
            bucket_shift: bucket_shift(nbuckets),
            ..*self
        })
    }

    /// Read the header at the start of a region.
    pub(super) fn load<R: Region>(region: &R) -> Self {
        let mut kinds = [0; MAX_KEYS];
        let base = offset_of!(Header, kinds);
        kinds[..8].copy_from_slice(&region.load_u64(base).to_ne_bytes());
        kinds[8..].copy_from_slice(&region.load_u64(base + 8).to_ne_bytes());
        Self {
            magic: region.load_u64(offset_of!(Header, magic)),
            version: region.load_u32(offset_of!(Header, version)),
            header_size: region.load_u32(offset_of!(Header, header_size)),
            region_len: region.load_u64(offset_of!(Header, region_len)),
            buckets_offset: region.load_u64(offset_of!(Header, buckets_offset)),
            chunk_used: region.load_u64(CHUNK_USED),
            nrecords: region.load_u64(NRECORDS),
            nbuckets: region.load_u32(offset_of!(Header, nbuckets)),
            bucket_shift: region.load_u32(offset_of!(Header, bucket_shift)),
            record_size: region.load_u32(offset_of!(Header, record_size)),
            payload_size: region.load_u32(offset_of!(Header, payload_size)),
            nkeys: region.load_u32(offset_of!(Header, nkeys)),
            flags: region.load_u32(offset_of!(Header, flags)),
            kinds,
            reserved: region.load_u64(offset_of!(Header, reserved)),
        }
    }

    /// Write the header at the start of a region.
    pub(super) fn store<R: Region>(&self, region: &R) {
        let base = offset_of!(Header, kinds);
        let mut half = [0; 8];
        half.copy_from_slice(&self.kinds[..8]);
        region.store_u64(base, u64::from_ne_bytes(half));
        half.copy_from_slice(&self.kinds[8..]);
        region.store_u64(base + 8, u64::from_ne_bytes(half));
        region.store_u64(offset_of!(Header, magic), self.magic);
        region.store_u32(offset_of!(Header, version), self.version);
        region.store_u32(offset_of!(Header, header_size), self.header_size);
        region.store_u64(offset_of!(Header, region_len), self.region_len);
        region.store_u64(offset_of!(Header, buckets_offset), self.buckets_offset);
        region.store_u64(CHUNK_USED, self.chunk_used);
        region.store_u64(NRECORDS, self.nrecords);
        region.store_u32(offset_of!(Header, nbuckets), self.nbuckets);
        region.store_u32(offset_of!(Header, bucket_shift), self.bucket_shift);
        region.store_u32(offset_of!(Header, record_size), self.record_size);
        region.store_u32(offset_of!(Header, payload_size), self.payload_size);
        region.store_u32(offset_of!(Header, nkeys), self.nkeys);
        region.store_u32(offset_of!(Header, flags), self.flags);
        region.store_u64(offset_of!(Header, reserved), self.reserved);
    }

    /// Check every field against the format and the `len` bytes given, and
    /// return the layout the table works with.
    pub(super) fn validate(&self, len: usize) -> Result<Layout> {
        ensure!(self.magic == MAGIC, "the region does not hold a table");
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
                "table region length {} exceeds the {len} bytes given",
                self.region_len
            );
        };
        ensure!(
            region_len.is_multiple_of(8) && region_len >= HEADER_SIZE,
            "table region length {region_len} is not a multiple of 8 past the header"
        );
        let nkeys = self.nkeys as usize;
        ensure!(
            (1..=MAX_KEYS).contains(&nkeys),
            "a table has 1 to {MAX_KEYS} keys, not {nkeys}"
        );
        let mut kinds = [KeyKind::Int32; MAX_KEYS];
        for (key, (slot, &code)) in kinds.iter_mut().zip(&self.kinds).enumerate() {
            if key < nkeys {
                let Some(kind) = KeyKind::from_code(code) else {
                    bail!("table key {key} has unknown kind {code}");
                };
                *slot = kind;
            }
        }
        let payload_size = self.payload_size as usize;
        ensure!(
            self.record_size == record_size(nkeys, payload_size)?,
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
            buckets_offset.is_multiple_of(8) && buckets_end <= region_len,
            "table buckets at {buckets_offset} lie outside the region of {region_len} bytes"
        );
        let chunk_used = usize::try_from(self.chunk_used).ok();
        let Some(chunk_used) = chunk_used.filter(|&used| used <= buckets_offset) else {
            bail!(
                "table record area of {} bytes overlaps the buckets at {buckets_offset}",
                self.chunk_used
            );
        };
        ensure!(
            chunk_used.is_multiple_of(8) && chunk_used >= HEADER_SIZE,
            "table record area end {chunk_used} is not a multiple of 8 past the header"
        );
        let used_by_records = self.nrecords.checked_mul(u64::from(self.record_size));
        ensure!(
            used_by_records.is_some_and(|used| used <= (chunk_used - HEADER_SIZE) as u64),
            "table record count {} does not fit its {chunk_used} used bytes",
            self.nrecords
        );
        Ok(Layout {
            region_len,
            buckets_offset,
            nbuckets,
            bucket_shift: self.bucket_shift,
            record_size: self.record_size as usize,
            payload_size,
            nkeys,
            kinds,
        })
    }
}

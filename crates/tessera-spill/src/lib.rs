//! Blocks of temporary storage.
//!
//! A node that spills writes chunks of a hash table whole: a chunk of
//! records (the table's own format, see `docs/table.md`) or a chunk of the
//! by-reference values its records refer to. Each goes to disk as a
//! [`BlockHeader`] and then the chunk's bytes, so reading one back gives a
//! chunk that is ready at once: the records need no decoding, only linking
//! into a new index. The header says which chunk it was and of which
//! partition and level, and carries the table's layout fingerprint, so
//! that a block of another table or a damaged file is an error, never a
//! record read the wrong way.
//!
//! This crate reads and writes nothing: it lays out and checks headers in
//! the caller's buffers. The files are the node's, through PostgreSQL's
//! temporary files (see `docs/spill.md`). Temporary files are read back by
//! the process, or the processes of one query, that wrote them, on the
//! same machine: the header is in native byte order and carries no
//! checksum, as PostgreSQL's own temporary files do not.

use anyhow::{Result, ensure};

pub mod columns;
mod damaged;
mod pack;
pub mod plan;
pub use damaged::Damaged;
use damaged::intact;
pub use pack::{pack, unpack};

/// The first word of every block.
pub const MAGIC: u64 = u64::from_le_bytes(*b"TESSSPIL");

/// The format of the blocks this crate writes and reads.
pub const VERSION: u32 = 2;

/// Bytes of a block header; the body follows, aligned to 8.
pub const HEADER_SIZE: usize = 48;

/// The most partition bits one level takes, and the deepest level: a
/// level uses bits of a 32-bit hash that the levels above did not.
pub const MAX_LEVEL: u32 = 32;

/// What a block holds.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockKind {
    /// A chunk of records, its used mark first.
    Records = 1,
    /// A chunk of by-reference values.
    Values = 2,
    /// A chunk of rows by column ([`columns`]), always stored packed.
    Columns = 3,
}

impl BlockKind {
    fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::Records),
            2 => Some(Self::Values),
            3 => Some(Self::Columns),
            _ => None,
        }
    }
}

/// The header of a block: which chunk follows and what it belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockHeader {
    pub kind: BlockKind,
    /// The chunk's number in its table, records and values each numbered
    /// on their own: a value reference names the value chunk by it.
    pub number: u32,
    /// The partition the chunk's records or values belong to.
    pub partition: u32,
    /// The level of partitioning, 0 for the first.
    pub level: u32,
    /// The table's layout fingerprint.
    pub fingerprint: u64,
    /// Bytes of the body: a multiple of 8, at least 8 for records.
    pub len: u64,
    /// For a chunk of records stored packed ([`pack`]), the bytes on disk,
    /// fewer than `len`, a multiple of 8; 0 when the body is stored as it
    /// is, as every chunk of values is.
    pub packed: u32,
}

const MAGIC_AT: usize = 0;
const VERSION_AT: usize = 8;
const KIND_AT: usize = 12;
const NUMBER_AT: usize = 16;
const PARTITION_AT: usize = 20;
const LEVEL_AT: usize = 24;
const PACKED_AT: usize = 28;
const FINGERPRINT_AT: usize = 32;
const LEN_AT: usize = 40;

fn put_u32(out: &mut [u8], at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_ne_bytes());
}

fn put_u64(out: &mut [u8], at: usize, value: u64) {
    out[at..at + 8].copy_from_slice(&value.to_ne_bytes());
}

fn get_u32(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_ne_bytes(word)
}

fn get_u64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_ne_bytes(word)
}

impl BlockHeader {
    fn check(&self, max_len: u64) -> Result<()> {
        ensure!(
            self.len.is_multiple_of(8) && self.len <= max_len,
            "a spilled block of {} bytes is not a multiple of 8 up to {max_len}",
            self.len
        );
        ensure!(
            self.kind != BlockKind::Records || self.len >= 8,
            "a spilled chunk of records has no used mark"
        );
        ensure!(
            self.kind != BlockKind::Columns
                || (self.len >= columns::HEADER as u64
                    && self.packed >= 8
                    && self.packed.is_multiple_of(8)),
            "a spilled chunk of columns of {} bytes packed into {} is no packed chunk",
            self.len,
            self.packed
        );
        ensure!(
            self.packed == 0
                || self.kind == BlockKind::Columns
                || (self.kind == BlockKind::Records
                    && self.packed.is_multiple_of(8)
                    && u64::from(self.packed) >= 16
                    && u64::from(self.packed) < self.len),
            "a spilled block packed into {} bytes of {} is no packed chunk of records",
            self.packed,
            self.len
        );
        ensure!(
            self.level < MAX_LEVEL,
            "a spilled block at level {} is past the hash's bits",
            self.level
        );
        Ok(())
    }

    /// Lay the header out in the first [`HEADER_SIZE`] bytes of `out`,
    /// checking it as [`Self::read`] would, with `max_len` the most body
    /// bytes the reader accepts.
    pub fn write(&self, out: &mut [u8], max_len: u64) -> Result<()> {
        ensure!(
            out.len() >= HEADER_SIZE,
            "a spilled block header needs {HEADER_SIZE} bytes"
        );
        self.check(max_len)?;
        put_u64(out, MAGIC_AT, MAGIC);
        put_u32(out, VERSION_AT, VERSION);
        put_u32(out, KIND_AT, self.kind as u32);
        put_u32(out, NUMBER_AT, self.number);
        put_u32(out, PARTITION_AT, self.partition);
        put_u32(out, LEVEL_AT, self.level);
        put_u32(out, PACKED_AT, self.packed);
        put_u64(out, FINGERPRINT_AT, self.fingerprint);
        put_u64(out, LEN_AT, self.len);
        Ok(())
    }

    /// Read and check the header at the start of `bytes`: the magic, the
    /// version, a known kind, the table's `fingerprint` and a body length
    /// of at most `max_len`.
    pub fn read(bytes: &[u8], fingerprint: u64, max_len: u64) -> Result<Self> {
        ensure!(
            bytes.len() >= HEADER_SIZE,
            "a spilled block header needs {HEADER_SIZE} bytes, got {}",
            bytes.len()
        );
        intact!(
            get_u64(bytes, MAGIC_AT) == MAGIC,
            "the bytes hold no spilled block"
        );
        let version = get_u32(bytes, VERSION_AT);
        intact!(
            version == VERSION,
            "spilled block version {version} is not the supported {VERSION}"
        );
        let Some(kind) = BlockKind::from_code(get_u32(bytes, KIND_AT)) else {
            return Err(Damaged(format!(
                "a spilled block of unknown kind {}",
                get_u32(bytes, KIND_AT)
            ))
            .into());
        };
        let header = Self {
            kind,
            number: get_u32(bytes, NUMBER_AT),
            partition: get_u32(bytes, PARTITION_AT),
            level: get_u32(bytes, LEVEL_AT),
            fingerprint: get_u64(bytes, FINGERPRINT_AT),
            len: get_u64(bytes, LEN_AT),
            packed: get_u32(bytes, PACKED_AT),
        };
        intact!(
            header.fingerprint == fingerprint,
            "a spilled block belongs to another table"
        );
        header
            .check(max_len)
            .map_err(|error| Damaged(format!("{error:#}")))?;
        Ok(header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> BlockHeader {
        BlockHeader {
            kind: BlockKind::Records,
            number: 7,
            partition: 3,
            level: 1,
            fingerprint: 0x1234_5678_9abc_def0,
            len: 4096,
            packed: 0,
        }
    }

    #[test]
    fn a_header_reads_back_as_written() -> Result<()> {
        let mut bytes = [0_u8; HEADER_SIZE];
        header().write(&mut bytes, 1 << 20)?;
        assert_eq!(
            BlockHeader::read(&bytes, header().fingerprint, 1 << 20)?,
            header()
        );
        let values = BlockHeader {
            kind: BlockKind::Values,
            len: 0,
            ..header()
        };
        values.write(&mut bytes, 1 << 20)?;
        assert_eq!(
            BlockHeader::read(&bytes, values.fingerprint, 1 << 20)?,
            values
        );
        Ok(())
    }

    #[test]
    fn every_damaged_field_is_refused() -> Result<()> {
        let mut good = [0_u8; HEADER_SIZE];
        header().write(&mut good, 1 << 20)?;
        let fingerprint = header().fingerprint;
        let cases: [(&str, usize, u64, usize); 9] = [
            ("magic", MAGIC_AT, 1, 8),
            ("version", VERSION_AT, 1, 4),
            ("kind", KIND_AT, 9, 4),
            ("odd packing", PACKED_AT, 1001, 4),
            ("packing no shorter", PACKED_AT, 4096, 4),
            ("fingerprint", FINGERPRINT_AT, 1, 8),
            ("odd length", LEN_AT, 4097, 8),
            ("long length", LEN_AT, 2 << 20, 8),
            ("level", LEVEL_AT, 32, 4),
        ];
        for (name, at, value, width) in cases {
            let mut bytes = good;
            if width == 4 {
                put_u32(&mut bytes, at, value as u32);
            } else {
                put_u64(&mut bytes, at, value);
            }
            // A damaged field is damaged data, not a misuse of the call.
            let error = BlockHeader::read(&bytes, fingerprint, 1 << 20)
                .expect_err(&format!("{name} accepted"));
            assert!(error.downcast_ref::<Damaged>().is_some(), "{name}: {error}");
        }
        // A buffer shorter than a header is the caller's misuse.
        let short = BlockHeader::read(&good[..HEADER_SIZE - 1], fingerprint, 1 << 20)
            .expect_err("a short buffer accepted");
        assert!(short.downcast_ref::<Damaged>().is_none());
        let foreign = BlockHeader::read(&good, fingerprint + 1, 1 << 20)
            .expect_err("another table's block accepted");
        assert!(foreign.downcast_ref::<Damaged>().is_some());
        Ok(())
    }

    #[test]
    fn a_header_that_would_not_read_back_is_not_written() {
        let mut bytes = [0_u8; HEADER_SIZE];
        let empty_records = BlockHeader { len: 0, ..header() };
        assert!(empty_records.write(&mut bytes, 1 << 20).is_err());
        assert!(header().write(&mut bytes, 1024).is_err());
        let packed_values = BlockHeader {
            kind: BlockKind::Values,
            packed: 64,
            ..header()
        };
        assert!(packed_values.write(&mut bytes, 1 << 20).is_err());
        let packed = BlockHeader {
            packed: 64,
            ..header()
        };
        assert!(packed.write(&mut bytes, 1 << 20).is_ok());
        assert!(
            header()
                .write(&mut bytes[..HEADER_SIZE - 1], 1 << 20)
                .is_err()
        );
    }

    #[test]
    fn a_columns_header_keeps_its_lengths() -> Result<()> {
        let columns = BlockHeader {
            kind: BlockKind::Columns,
            len: columns::HEADER as u64,
            packed: 8,
            ..header()
        };
        let mut good = [0_u8; HEADER_SIZE];
        columns.write(&mut good, 1 << 20)?;
        assert_eq!(
            BlockHeader::read(&good, columns.fingerprint, 1 << 20)?,
            columns
        );
        // A body shorter than a chunk's header, no packed length, and a
        // packed length that is no multiple of 8.
        let cases: [(&str, u64, u32); 3] = [
            ("a short body", 8, 8),
            ("no packed length", columns.len, 0),
            ("an odd packed length", columns.len, 12),
        ];
        for (name, len, packed) in cases {
            let bad = BlockHeader {
                len,
                packed,
                ..columns
            };
            let mut bytes = [0_u8; HEADER_SIZE];
            assert!(bad.write(&mut bytes, 1 << 20).is_err(), "{name} written");
            let mut bytes = good;
            put_u64(&mut bytes, LEN_AT, len);
            put_u32(&mut bytes, PACKED_AT, packed);
            let error = BlockHeader::read(&bytes, columns.fingerprint, 1 << 20)
                .expect_err(&format!("{name} read"));
            assert!(error.downcast_ref::<Damaged>().is_some(), "{name}: {error}");
        }
        Ok(())
    }

    #[test]
    fn the_level_and_the_limit_hold_for_every_kind() -> Result<()> {
        let kinds = [
            header(),
            BlockHeader {
                kind: BlockKind::Values,
                ..header()
            },
            BlockHeader {
                kind: BlockKind::Columns,
                packed: 64,
                ..header()
            },
        ];
        for good in kinds {
            let name = format!("{:?}", good.kind);
            let mut bytes = [0_u8; HEADER_SIZE];
            // The last level, and a body of exactly the limit.
            let last = BlockHeader {
                level: MAX_LEVEL - 1,
                ..good
            };
            last.write(&mut bytes, good.len)?;
            assert_eq!(BlockHeader::read(&bytes, good.fingerprint, good.len)?, last);
            // One level more, on writing and on reading.
            let deep = BlockHeader {
                level: MAX_LEVEL,
                ..good
            };
            assert!(
                deep.write(&mut [0_u8; HEADER_SIZE], 1 << 20).is_err(),
                "{name}"
            );
            let mut damaged = bytes;
            put_u32(&mut damaged, LEVEL_AT, MAX_LEVEL);
            let error = BlockHeader::read(&damaged, good.fingerprint, 1 << 20)
                .expect_err(&format!("{name} read past the last level"));
            assert!(error.downcast_ref::<Damaged>().is_some(), "{name}: {error}");
            // A body longer than the reader accepts.
            assert!(
                good.write(&mut [0_u8; HEADER_SIZE], good.len - 8).is_err(),
                "{name}"
            );
            let error = BlockHeader::read(&bytes, good.fingerprint, good.len - 8)
                .expect_err(&format!("{name} read past the limit"));
            assert!(error.downcast_ref::<Damaged>().is_some(), "{name}: {error}");
        }
        Ok(())
    }
}

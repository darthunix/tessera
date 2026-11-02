//! Blocks of temporary storage.
//!
//! A node that spills writes chunks whole: a chunk of a hash table's
//! records (the table's own format, see `openspec/specs/hash-table/`), a
//! chunk of the by-reference values that records or rows refer to, or a
//! chunk of rows by column ([`columns`]). Each goes to disk as a
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
//!
//! A call that refuses returns an [`Error`], one word: a wrong call, a
//! value the caller passed, or damaged bytes, read back from a file, and
//! the [`Rule`] it broke. A host reports damaged bytes as damaged data
//! and a wrong call as its own fault; a limit of the host, and why a
//! source of rows it gives cannot give a value, stay the host's to say.

// All memory comes to the crate as safe slices its host makes, so the
// compiler alone keeps it sound, its loom models included.
#![forbid(unsafe_code)]

#[cfg(not(target_pointer_width = "64"))]
compile_error!("relspill needs a target whose pointers are 64 bits wide");

pub mod columns;
mod error;
#[cfg(all(test, loom))]
mod loom;
mod pack;
pub mod phases;
pub mod plan;
pub mod set;
pub mod shared_spill;
pub mod spill_columns;
use error::ensure;
pub use error::{Error, Kind, Result, Rule};
pub use pack::{pack, unpack};

/// The first word of every block.
pub const MAGIC: u64 = u64::from_le_bytes(*b"TESSSPIL");

/// The format of the blocks this crate writes and reads.
pub const VERSION: u32 = 3;

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

/// Where the low `W` bytes of a word of `N` bytes start in its bytes in
/// the machine's order: a value stored narrower than its word keeps these
/// bytes, as the narrower integer has them.
const fn low<const W: usize, const N: usize>() -> usize {
    if cfg!(target_endian = "big") {
        N - W
    } else {
        0
    }
}

impl BlockHeader {
    /// Check the fields against each other and `max_len`: an error of
    /// `kind`, a wrong call for a header to write, damaged bytes for one
    /// read back.
    fn check(&self, max_len: u64, kind: Kind) -> Result<()> {
        let refuse = |rule| Error::new(kind, rule);
        ensure!(
            self.len.is_multiple_of(8) && self.len <= max_len,
            refuse(Rule::BlockLength).with(max_len).and(self.len)
        );
        ensure!(
            self.kind != BlockKind::Records || self.len >= relhash::CHUNK_HEADER as u64,
            refuse(Rule::UsedMark)
        );
        ensure!(
            self.kind != BlockKind::Columns
                || (self.len >= columns::HEADER as u64
                    && self.packed >= 8
                    && self.packed.is_multiple_of(8)),
            refuse(Rule::ColumnsBlock).with(self.len).and(self.packed)
        );
        ensure!(
            self.packed == 0
                || self.kind == BlockKind::Columns
                || (self.kind == BlockKind::Records
                    && self.packed.is_multiple_of(8)
                    // A packed body's two counts and its codes of lanes.
                    && u64::from(self.packed) >= 16
                    && u64::from(self.packed) < self.len),
            refuse(Rule::PackedRecords).with(self.len).and(self.packed)
        );
        ensure!(self.level < MAX_LEVEL, refuse(Rule::Level).with(self.level));
        Ok(())
    }

    /// Lay the header out in the first [`HEADER_SIZE`] bytes of `out`,
    /// checking it as [`Self::read`] would, with `max_len` the most body
    /// bytes the reader accepts.
    pub fn write(&self, out: &mut [u8], max_len: u64) -> Result<()> {
        ensure!(
            out.len() >= HEADER_SIZE,
            Error::wrong_call(Rule::Header).with(out.len())
        );
        self.check(max_len, Kind::WrongCall)?;
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
            Error::wrong_call(Rule::Header).with(bytes.len())
        );
        ensure!(
            get_u64(bytes, MAGIC_AT) == MAGIC,
            Error::damaged(Rule::Magic)
        );
        let version = get_u32(bytes, VERSION_AT);
        ensure!(
            version == VERSION,
            Error::damaged(Rule::Version).with(version)
        );
        let Some(kind) = BlockKind::from_code(get_u32(bytes, KIND_AT)) else {
            return Err(Error::damaged(Rule::BlockKind).with(get_u32(bytes, KIND_AT)));
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
        ensure!(
            header.fingerprint == fingerprint,
            Error::damaged(Rule::Fingerprint)
        );
        header.check(max_len, Kind::Damaged)?;
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
        let cases: [(&str, usize, u64, usize, Rule); 9] = [
            ("magic", MAGIC_AT, 1, 8, Rule::Magic),
            ("version", VERSION_AT, 1, 4, Rule::Version),
            ("kind", KIND_AT, 9, 4, Rule::BlockKind),
            ("odd packing", PACKED_AT, 1001, 4, Rule::PackedRecords),
            (
                "packing no shorter",
                PACKED_AT,
                4096,
                4,
                Rule::PackedRecords,
            ),
            ("fingerprint", FINGERPRINT_AT, 1, 8, Rule::Fingerprint),
            ("odd length", LEN_AT, 4097, 8, Rule::BlockLength),
            ("long length", LEN_AT, 2 << 20, 8, Rule::BlockLength),
            ("level", LEVEL_AT, 32, 4, Rule::Level),
        ];
        for (name, at, value, width, rule) in cases {
            let mut bytes = good;
            if width == 4 {
                put_u32(&mut bytes, at, value as u32);
            } else {
                put_u64(&mut bytes, at, value);
            }
            // A damaged field is damaged data, not a misuse of the call.
            let error = BlockHeader::read(&bytes, fingerprint, 1 << 20)
                .expect_err(&format!("{name} accepted"));
            assert_eq!(
                (error.kind(), error.rule()),
                (Kind::Damaged, rule),
                "{name}"
            );
        }
        // A buffer shorter than a header is the caller's misuse.
        let short = BlockHeader::read(&good[..HEADER_SIZE - 1], fingerprint, 1 << 20)
            .expect_err("a short buffer accepted");
        assert_eq!(
            (short.kind(), short.rule()),
            (Kind::WrongCall, Rule::Header)
        );
        let foreign = BlockHeader::read(&good, fingerprint + 1, 1 << 20)
            .expect_err("another table's block accepted");
        assert_eq!(
            (foreign.kind(), foreign.rule()),
            (Kind::Damaged, Rule::Fingerprint)
        );
        Ok(())
    }

    #[test]
    fn a_header_that_would_not_read_back_is_not_written() {
        let mut bytes = [0_u8; HEADER_SIZE];
        let refused = |result: Result<()>| result.map_err(|error| (error.kind(), error.rule()));
        let empty_records = BlockHeader { len: 0, ..header() };
        assert_eq!(
            refused(empty_records.write(&mut bytes, 1 << 20)),
            Err((Kind::WrongCall, Rule::UsedMark))
        );
        assert_eq!(
            refused(header().write(&mut bytes, 1024)),
            Err((Kind::WrongCall, Rule::BlockLength))
        );
        let packed_values = BlockHeader {
            kind: BlockKind::Values,
            packed: 64,
            ..header()
        };
        assert_eq!(
            refused(packed_values.write(&mut bytes, 1 << 20)),
            Err((Kind::WrongCall, Rule::PackedRecords))
        );
        let packed = BlockHeader {
            packed: 64,
            ..header()
        };
        assert!(packed.write(&mut bytes, 1 << 20).is_ok());
        assert_eq!(
            refused(header().write(&mut bytes[..HEADER_SIZE - 1], 1 << 20)),
            Err((Kind::WrongCall, Rule::Header))
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
            let error = bad.write(&mut bytes, 1 << 20).expect_err(name);
            assert_eq!(
                (error.kind(), error.rule()),
                (Kind::WrongCall, Rule::ColumnsBlock)
            );
            let mut bytes = good;
            put_u64(&mut bytes, LEN_AT, len);
            put_u32(&mut bytes, PACKED_AT, packed);
            let error = BlockHeader::read(&bytes, columns.fingerprint, 1 << 20)
                .expect_err(&format!("{name} read"));
            assert_eq!(
                (error.kind(), error.rule()),
                (Kind::Damaged, Rule::ColumnsBlock)
            );
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
            assert_eq!((error.kind(), error.rule()), (Kind::Damaged, Rule::Level));
            // A body longer than the reader accepts.
            assert!(
                good.write(&mut [0_u8; HEADER_SIZE], good.len - 8).is_err(),
                "{name}"
            );
            let error = BlockHeader::read(&bytes, good.fingerprint, good.len - 8)
                .expect_err(&format!("{name} read past the limit"));
            assert_eq!(
                (error.kind(), error.rule()),
                (Kind::Damaged, Rule::BlockLength)
            );
        }
        Ok(())
    }
}

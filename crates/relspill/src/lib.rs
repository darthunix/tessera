// The crate's page is its README.
#![doc = include_str!("../README.md")]
//! # Errors
//!
//! A call that refuses returns an [`Error`], one word: a wrong call, a
//! value the caller passed, or damaged bytes, read back from a file, and
//! the [`Rule`] it broke. A host reports damaged bytes as damaged data
//! and a wrong call as its own fault; a limit of the host, and why a
//! source of rows it gives cannot give a value, stay the host's to say.

// All memory comes to the crate as safe slices its host makes, so the
// compiler alone keeps it sound, its loom models included.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(not(target_pointer_width = "64"))]
compile_error!("relspill needs a target whose pointers are 64 bits wide");

pub mod columns;
mod error;
pub mod group;
pub mod join;
#[cfg(all(test, loom))]
mod loom;
mod pack;
pub mod phases;
pub mod plan;
pub mod set;
pub mod shared;
pub mod shared_spill;
pub mod sort;
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

/// What a block holds, by the code in its header, and the lengths each
/// kind allows. For every kind the body length is a multiple of 8 and at
/// most the bound of its kind ([`Bounds`]).
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockKind {
    /// A chunk of a `relhash` table's records, its used mark of 8 bytes
    /// first: a body of 8 bytes at least. It is stored as it is, with a
    /// packed length of 0, or as [`pack`] packs it, with a packed length
    /// that is a multiple of 8, at least 16 and below the body length.
    Records = 1,
    /// A chunk of the by-reference values that records or rows refer to
    /// ([`columns::value_ref`]), stored as it is: its packed length is 0.
    Values = 2,
    /// A chunk of rows by column ([`columns`]): a body of
    /// [`columns::HEADER`] bytes at least, always stored as
    /// [`columns::pack`] packs it, with a packed length that is a multiple
    /// of 8 and at least 8.
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
///
/// A block is this header, [`HEADER_SIZE`] bytes in the machine's byte
/// order, and then the stored body, the chunk as it is or packed:
///
/// ```text
///  byte    0           4           8           12          16
///          ┌───────────────────────┬───────────┬───────────┐
///        0 │ magic                 │ version   │ kind      │
///          ├───────────┬───────────┼───────────┼───────────┤
///       16 │ number    │ partition │ level     │ packed    │
///          ├───────────┴───────────┼───────────┴───────────┤
///       32 │ fingerprint           │ len                   │
///          └───────────────────────┴───────────────────────┘
///       48   the stored body
/// ```
///
/// The magic is [`MAGIC`], whose bytes are `TESSSPIL` on a little-endian
/// machine, the version [`VERSION`], and the kind the code of a
/// [`BlockKind`]; the other fields are this struct's. The header carries
/// no checksum: a temporary file is read back by the query that wrote it,
/// on the same machine, as PostgreSQL's own temporary files are.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockHeader {
    /// The kind of the chunk that follows.
    pub kind: BlockKind,
    /// The number the writer gives the chunk, records and values each
    /// numbered on their own: a value reference names the value chunk by
    /// it.
    pub number: u32,
    /// The partition the chunk's records or values belong to.
    pub partition: u32,
    /// The level of partitioning, 0 for the first, below [`MAX_LEVEL`].
    pub level: u32,
    /// The number the set was made with, which a reader compares with its
    /// own: of a hash table's layout, or of the width of a row.
    pub fingerprint: u64,
    /// Bytes of the chunk a reader gets, the body unpacked; for a chunk of
    /// columns, a chunk of its rows only. [`BlockKind`] gives its bounds.
    pub len: u64,
    /// The bytes of the stored body when it is stored packed, 0 when it is
    /// stored as it is; [`BlockKind`] says which kind is stored which way.
    pub packed: u32,
}

/// The most body bytes a block of each kind takes: a writer refuses a
/// longer block, and a reader takes one as damaged bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Bounds {
    /// A chunk of records or of columns, by the plan of its level.
    pub chunk: u64,
    /// A chunk of values: for a writer, the most it takes; for a reader,
    /// the longest one the set's writers wrote.
    pub values: u64,
}

impl Bounds {
    /// The same bound for every kind.
    pub const fn all(len: u64) -> Self {
        Self {
            chunk: len,
            values: len,
        }
    }

    /// The bound of a block of `kind`.
    pub const fn of(&self, kind: BlockKind) -> u64 {
        match kind {
            BlockKind::Values => self.values,
            BlockKind::Records | BlockKind::Columns => self.chunk,
        }
    }

    /// The larger of the two.
    pub fn largest(&self) -> u64 {
        self.chunk.max(self.values)
    }
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
    /// Check the fields against each other and `bounds`: an error of
    /// `kind`, a wrong call for a header to write, damaged bytes for one
    /// read back.
    fn check(&self, bounds: Bounds, kind: Kind) -> Result<()> {
        let refuse = |rule| Error::new(kind, rule);
        let most = bounds.of(self.kind);
        ensure!(
            self.len.is_multiple_of(8) && self.len <= most,
            refuse(Rule::BlockLength).with(most).and(self.len)
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
    /// checking it as [`Self::read`] would, with `bounds` the most body
    /// bytes the reader accepts.
    ///
    /// # Errors
    ///
    /// A wrong call, and nothing written, for `out` shorter than a header
    /// or a header that [`Self::read`] would refuse: a body length or a
    /// packed length its kind does not allow, a body past the bound of its
    /// kind, or a level of [`MAX_LEVEL`] or more.
    pub fn write(&self, out: &mut [u8], bounds: Bounds) -> Result<()> {
        ensure!(
            out.len() >= HEADER_SIZE,
            Error::wrong_call(Rule::Header).with(out.len())
        );
        self.check(bounds, Kind::WrongCall)?;
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
    /// version, a known kind, the set's `fingerprint`, the lengths its kind
    /// allows within `bounds`, and the level.
    ///
    /// # Errors
    ///
    /// Damaged bytes for a field that fails a check; a wrong call for
    /// `bytes` shorter than a header.
    pub fn read(bytes: &[u8], fingerprint: u64, bounds: Bounds) -> Result<Self> {
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
        header.check(bounds, Kind::Damaged)?;
        Ok(header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: Bounds = Bounds::all(1 << 20);

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
        header().write(&mut bytes, BOUNDS)?;
        assert_eq!(
            BlockHeader::read(&bytes, header().fingerprint, BOUNDS)?,
            header()
        );
        let values = BlockHeader {
            kind: BlockKind::Values,
            len: 0,
            ..header()
        };
        values.write(&mut bytes, BOUNDS)?;
        assert_eq!(
            BlockHeader::read(&bytes, values.fingerprint, BOUNDS)?,
            values
        );
        Ok(())
    }

    #[test]
    fn every_damaged_field_is_refused() -> Result<()> {
        let mut good = [0_u8; HEADER_SIZE];
        header().write(&mut good, BOUNDS)?;
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
            let error = BlockHeader::read(&bytes, fingerprint, BOUNDS)
                .expect_err(&format!("{name} accepted"));
            assert_eq!(
                (error.kind(), error.rule()),
                (Kind::Damaged, rule),
                "{name}"
            );
        }
        // A buffer shorter than a header is the caller's misuse.
        let short = BlockHeader::read(&good[..HEADER_SIZE - 1], fingerprint, BOUNDS)
            .expect_err("a short buffer accepted");
        assert_eq!(
            (short.kind(), short.rule()),
            (Kind::WrongCall, Rule::Header)
        );
        let foreign = BlockHeader::read(&good, fingerprint + 1, BOUNDS)
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
            refused(empty_records.write(&mut bytes, BOUNDS)),
            Err((Kind::WrongCall, Rule::UsedMark))
        );
        assert_eq!(
            refused(header().write(&mut bytes, Bounds::all(1024))),
            Err((Kind::WrongCall, Rule::BlockLength))
        );
        let packed_values = BlockHeader {
            kind: BlockKind::Values,
            packed: 64,
            ..header()
        };
        assert_eq!(
            refused(packed_values.write(&mut bytes, BOUNDS)),
            Err((Kind::WrongCall, Rule::PackedRecords))
        );
        let packed = BlockHeader {
            packed: 64,
            ..header()
        };
        assert!(packed.write(&mut bytes, BOUNDS).is_ok());
        assert_eq!(
            refused(header().write(&mut bytes[..HEADER_SIZE - 1], BOUNDS)),
            Err((Kind::WrongCall, Rule::Header))
        );
    }

    #[test]
    fn each_kind_of_block_takes_its_own_bound() -> Result<()> {
        let bounds = Bounds {
            chunk: 4096,
            values: 1 << 16,
        };
        let mut bytes = [0_u8; HEADER_SIZE];
        let long_values = BlockHeader {
            kind: BlockKind::Values,
            len: 1 << 16,
            ..header()
        };
        long_values.write(&mut bytes, bounds)?;
        assert_eq!(
            BlockHeader::read(&bytes, long_values.fingerprint, bounds)?,
            long_values
        );
        let mut refused = |kind, len| {
            let header = BlockHeader {
                kind,
                len,
                ..header()
            };
            let written = header.write(&mut bytes, Bounds::all(len));
            let read = BlockHeader::read(&bytes, header.fingerprint, bounds);
            (
                written.is_ok(),
                read.map_err(|error| (error.kind(), error.rule())),
            )
        };
        let past = Err((Kind::Damaged, Rule::BlockLength));
        assert_eq!(refused(BlockKind::Values, (1 << 16) + 8), (true, past));
        assert_eq!(refused(BlockKind::Records, 4096 + 8), (true, past));
        Ok(())
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
        columns.write(&mut good, BOUNDS)?;
        assert_eq!(
            BlockHeader::read(&good, columns.fingerprint, BOUNDS)?,
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
            let error = bad.write(&mut bytes, BOUNDS).expect_err(name);
            assert_eq!(
                (error.kind(), error.rule()),
                (Kind::WrongCall, Rule::ColumnsBlock)
            );
            let mut bytes = good;
            put_u64(&mut bytes, LEN_AT, len);
            put_u32(&mut bytes, PACKED_AT, packed);
            let error = BlockHeader::read(&bytes, columns.fingerprint, BOUNDS)
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
            last.write(&mut bytes, Bounds::all(good.len))?;
            assert_eq!(
                BlockHeader::read(&bytes, good.fingerprint, Bounds::all(good.len))?,
                last
            );
            // One level more, on writing and on reading.
            let deep = BlockHeader {
                level: MAX_LEVEL,
                ..good
            };
            assert!(
                deep.write(&mut [0_u8; HEADER_SIZE], BOUNDS).is_err(),
                "{name}"
            );
            let mut damaged = bytes;
            put_u32(&mut damaged, LEVEL_AT, MAX_LEVEL);
            let error = BlockHeader::read(&damaged, good.fingerprint, BOUNDS)
                .expect_err(&format!("{name} read past the last level"));
            assert_eq!((error.kind(), error.rule()), (Kind::Damaged, Rule::Level));
            // A body longer than the reader accepts.
            assert!(
                good.write(&mut [0_u8; HEADER_SIZE], Bounds::all(good.len - 8))
                    .is_err(),
                "{name}"
            );
            let error = BlockHeader::read(&bytes, good.fingerprint, Bounds::all(good.len - 8))
                .expect_err(&format!("{name} read past the limit"));
            assert_eq!(
                (error.kind(), error.rule()),
                (Kind::Damaged, Rule::BlockLength)
            );
        }
        Ok(())
    }
}

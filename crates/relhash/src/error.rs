//! The errors of the crate's calls.
//!
//! A call that refuses its arguments or the table's blocks returns an
//! [`Error`]: the rule it found broken, who broke it, and up to two
//! numbers that show how. The error is one word: it allocates nothing,
//! and a call returns it in registers. Its text is written only when it
//! is printed.
#![forbid(unsafe_code)]

use core::fmt;
use core::num::NonZeroU64;

use crate::table::{
    CHUNK_HEADER, FORMAT_VERSION, HEADER_SIZE, HELD, MAX_CHUNK_LEN, MAX_CHUNKS, MAX_KEYS,
    MAX_PARTITIONS, MAX_PAYLOAD_COLUMNS, MIN_BUCKETS,
};

/// The result of the crate's calls.
pub(crate) type Result<T, E = Error> = core::result::Result<T, E>;

/// What a call refused: the rule it found broken, who broke it, and up
/// to two numbers that show how.
///
/// The error is one integer, so that a result of a count or a reference
/// is two registers, as with a pointer; a struct of a rule and bytes was
/// returned through memory.
///
/// ```text
///   bits 56..=63   the rule's number, counted from 1
///   bit  55        the kind: 0 a wrong call, 1 a damaged table
///   bits 23..=54   the first number, up to 2^32 - 1
///   bits 0..=22    the second number, up to 2^23 - 1
/// ```
///
/// A number wider than its field keeps the field's largest value, which
/// the message prints with "or more". Tests compare the kind and the
/// rule; the message is for people.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Error(NonZeroU64);

// One word, and a result of nothing no wider; a count or a reference
// with it stays two registers.
const _: () = assert!(size_of::<Error>() == 8);
const _: () = assert!(size_of::<Result<()>>() == 8);
const _: () = assert!(size_of::<Result<usize>>() == 16);
// A rule's number fits its byte.
const _: () = assert!(RULES.len() < 256);

const RULE_SHIFT: u32 = 56;
const KIND_SHIFT: u32 = 55;
const A_SHIFT: u32 = 23;
const B_MAX: u32 = (1 << A_SHIFT) - 1;

/// Who broke the rule of an [`Error`]. What the call read decides the
/// kind: a value the caller passed makes a wrong call, a byte of the
/// table's blocks a damaged table. Each [`Rule`] says what it refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A value the caller passed breaks the rule: the call changed
    /// nothing when the value was a size, a count or a block. A wrong call
    /// is:
    ///
    /// - arrays of a batch of different row counts, or a mask of the wrong
    ///   words, as [`RowMaskView`](crate::RowMaskView) says;
    /// - keys or a payload that are not the table's, a key past the
    ///   table's keys, or a table of fewer than 1 or more than
    ///   [`MAX_KEYS`](crate::MAX_KEYS) keys;
    /// - payload columns past their limit or that do not fill the payload,
    ///   or a payload word past the payload;
    /// - a record that does not fit a chunk;
    /// - a chunk that does not exist, is not aligned to 8, or is not a
    ///   multiple of 8 from 8 bytes to [`MAX_CHUNK_LEN`](crate::MAX_CHUNK_LEN)
    ///   long, or more than [`MAX_CHUNKS`](crate::MAX_CHUNKS) chunks;
    /// - an index shorter than its header or not aligned to 8;
    /// - a new index that is not a multiple of 8, cannot hold its buckets,
    ///   is smaller than its capacity needs, or has fewer buckets than the
    ///   old;
    /// - a chunk split or merged into itself, or a split of no records;
    /// - partitions that are not a power of two up to the limit, or whose
    ///   bits pass the 32 bits of a hash;
    /// - a Bloom filter of the wrong size or alignment, for more records
    ///   than it may hold, or a shared one not built yet;
    /// - a source of keys that cannot give a row's key;
    /// - a slot of a payload cursor past its slots or holding no reference;
    /// - a reference or a cursor the caller passed that names no chunk or
    ///   lies outside its chunk, a cursor of a walk past its chunk's used
    ///   mark, or an empty output of a walk;
    /// - a count of rows of a partition that one more row would carry past
    ///   2^64;
    /// - an owning table that holds the most records a find or insert may
    ///   make.
    WrongCall,
    /// A byte of the table's blocks breaks the rule. The table is to be
    /// dropped or reset: the calls over it stay sound, but what they
    /// answer is not promised. A reference a caller passed that lands
    /// inside a record reads as damage too, as the crate cannot tell the
    /// two apart. A damaged table is:
    ///
    /// - a header with a field that breaks a rule of
    ///   [`HEADER_SIZE`](crate::HEADER_SIZE), the field named, or an index
    ///   length below the header's;
    /// - a reference read from the table that names no chunk, lies outside
    ///   its chunk or names a byte where no record starts;
    /// - a record, whoever named it, whose size is not the table's;
    /// - a used mark that ends no record;
    /// - a chain longer than the count of records;
    /// - a header of a grown index that no longer matches the table it grew
    ///   from.
    ///
    /// A walk down a chain takes no more steps than the header's count of
    /// records, and no more than the places a reference can name in the
    /// chunks it is given, 2^17 a chunk. A call checks its arguments, the
    /// header and the count of chunks before it changes anything, and a
    /// chunk before it reads or writes it.
    Damaged,
}

/// The rules, listed once: the enum, and the table that turns a rule's
/// number in an error back into the rule.
macro_rules! rules {
    ($($(#[$doc:meta])* $rule:ident,)*) => {
        /// The rule an [`Error`] names. The message of each is in the
        /// [`Display`](fmt::Display) of the error.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(u8)]
        #[non_exhaustive]
        pub enum Rule {
            $($(#[$doc])* $rule,)*
        }

        /// Every rule, at the index of its number.
        const RULES: &[Rule] = &[$(Rule::$rule,)*];
    };
}

rules! {
    /// A mask of rows has more or fewer words than its rows need.
    MaskWords,
    /// A mask of rows has a bit set past its last row.
    MaskPadding,
    /// A row past the rows of a mask.
    Row,
    /// A word past the words of a mask.
    MaskWord,
    /// A word for a mask has a bit set past the mask's last row.
    WordPadding,
    /// Two masks of different row counts.
    MaskRows,
    /// The masks and arrays of a call have different row counts.
    RowCounts,

    /// The first word of the index is not the mark of a table.
    Magic,
    /// The header's format version is not the one the crate reads.
    Version,
    /// The header's size is not the format's.
    HeaderSize,
    /// The header has a flag set.
    Flags,
    /// The header's index length exceeds the bytes given.
    IndexLength,
    /// The header's index length is not a multiple of 8 past the header.
    IndexShape,
    /// A table of fewer than 1 or more than [`MAX_KEYS`] keys.
    KeyCount,
    /// The header's record size is not the size of its keys and payload.
    RecordSize,
    /// The header's bucket count is not a power of two of the least.
    BucketCount,
    /// The header's bucket shift does not match its bucket count.
    BucketShift,
    /// The header's buckets lie outside the index.
    Buckets,
    /// The header has a reserved word set.
    Reserved,

    /// An index not aligned to 8 bytes.
    IndexAlign,
    /// An index shorter than its header.
    IndexShort,
    /// A new index that is not a multiple of 8.
    IndexLen,
    /// A new index smaller than its capacity needs.
    IndexSmall,
    /// A new index that cannot hold its buckets.
    IndexBuckets,
    /// A larger index with fewer buckets than the old.
    IndexShrinks,
    /// The table changed while it grew to a larger index.
    Grew,

    /// A record of keys and a payload that does not fit a chunk.
    RecordFit,
    /// A record size that no record of a table has.
    RecordBytes,
    /// A record that does not fit the owning table's chunks.
    RecordRoom,
    /// Records of keys and a payload that are not the table's.
    Config,
    /// Keys that are not the table's.
    Keys,
    /// A key past the table's keys.
    Key,
    /// A source of keys that cannot give a selected row's key.
    KeyRead,
    /// A payload of the wrong length for its rows.
    PayloadBytes,
    /// Payload columns that do not fill the table's payload.
    PayloadColumns,
    /// Payload columns that do not fill a payload of partitions.
    PartitionColumns,
    /// Payload columns past their limit.
    PayloadShape,
    /// A payload column of the wrong row count.
    PayloadRows,
    /// A payload word past the payload.
    PayloadWord,
    /// Payload words that are none or past the payload.
    PayloadWords,
    /// The references, mask and outputs of a batch have different row
    /// counts.
    BatchRows,

    /// A chunk that does not exist.
    NoChunk,
    /// A chunk of the table not aligned to 8 or of a length no chunk has.
    Chunk,
    /// A block given as a chunk not aligned to 8 or of a length no chunk
    /// has.
    Block,
    /// Bases and lengths of chunks that do not pair up, or too many.
    Chunks,
    /// More than [`MAX_CHUNKS`] chunks.
    ChunkLimit,
    /// A chunk's used mark that ends no record.
    UsedMark,
    /// A block's used mark that ends no record.
    BlockUsedMark,
    /// A byte of a chunk that is no record boundary below its used mark.
    Boundary,

    /// A record offset that lies outside the records.
    Outside,
    /// A record offset that does not start a record.
    Misplaced,
    /// A place of a chunk below its used mark that holds no record.
    NoRecord,
    /// A chain longer than the table's count of records.
    Chain,
    /// A reference to mark that names no record.
    Reference,
    /// A cursor that lies outside the records.
    Cursor,
    /// A cursor past its chunk's records.
    CursorPast,
    /// A record of the merged chunk found linked in the table.
    MergedFound,

    /// A count of partitions that is not a power of two up to
    /// [`MAX_PARTITIONS`].
    Partitions,
    /// Partition bits that pass the 32 bits of a hash.
    PartitionBits,
    /// Row counts and partitions that do not pair up.
    PartitionRows,
    /// The references and hashes of a split differ in length.
    SplitLengths,
    /// A split of no records.
    EmptySplit,
    /// A chunk split into itself.
    SplitIntoItself,
    /// A chunk merged into itself.
    MergeIntoItself,

    /// A slot past a payload cursor's [`HELD`].
    Slot,
    /// A slot of a payload cursor that holds no reference.
    EmptySlot,

    /// A shared Bloom filter not aligned to 8 bytes.
    FilterAlign,
    /// A shared Bloom filter of the wrong count of words.
    FilterWords,
    /// A shared Bloom filter without its state word.
    FilterState,
    /// A Bloom filter for more records than it may hold.
    FilterRecords,
    /// A Bloom filter of a count of words that is not a power of two.
    FilterSize,
    /// The words of a Bloom filter filled at once not aligned to 8 bytes.
    FilterFillAlign,
    /// A shared Bloom filter not built yet.
    FilterNotBuilt,
    /// A walk into an output with room for no record.
    EmptyWalk,
    /// A count of rows of a partition that one more row would carry past
    /// 2^64.
    PartitionCount,
    /// An owning table that holds the most records a find or insert may
    /// make.
    MostRecords,
}

impl Error {
    /// A wrong call: a value the caller passed breaks `rule`.
    #[inline]
    pub(crate) const fn wrong_call(rule: Rule) -> Self {
        Self::new(Kind::WrongCall, rule)
    }

    /// A damaged table: a byte of the table's blocks breaks `rule`.
    #[inline]
    pub(crate) const fn damaged(rule: Rule) -> Self {
        Self::new(Kind::Damaged, rule)
    }

    /// The error of `kind` for `rule`, without numbers.
    #[inline]
    pub(crate) const fn new(kind: Kind, rule: Rule) -> Self {
        let word = (rule as u64 + 1) << RULE_SHIFT | (kind as u64) << KIND_SHIFT;
        // A rule's number is never 0, so neither is the word.
        match NonZeroU64::new(word) {
            Some(word) => Self(word),
            None => Self(NonZeroU64::MAX),
        }
    }

    /// The error with `a` as its first number.
    #[cold]
    #[inline(never)]
    pub(crate) fn with(self, a: impl TryInto<u32>) -> Self {
        let a = a.try_into().unwrap_or(u32::MAX);
        Self(self.0 | u64::from(a) << A_SHIFT)
    }

    /// The error with `b` as its second number.
    #[cold]
    #[inline(never)]
    pub(crate) fn and(self, b: impl TryInto<u32>) -> Self {
        let b = b.try_into().unwrap_or(u32::MAX).min(B_MAX);
        Self(self.0 | u64::from(b))
    }

    /// Who broke the rule.
    pub fn kind(self) -> Kind {
        if self.0.get() >> KIND_SHIFT & 1 == 0 {
            Kind::WrongCall
        } else {
            Kind::Damaged
        }
    }

    /// The rule that was broken.
    pub fn rule(self) -> Rule {
        RULES[(self.0.get() >> RULE_SHIFT) as usize - 1]
    }

    /// The two numbers, 0 where the rule has none.
    pub fn numbers(self) -> (u32, u32) {
        let word = self.0.get();
        ((word >> A_SHIFT) as u32, word as u32 & B_MAX)
    }
}

/// A number as an error holds it: its field's largest value prints with
/// "or more".
struct Number(u32, u32);

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(value, max) = *self;
        write!(f, "{value}")?;
        if value == max {
            f.write_str(" or more")?;
        }
        Ok(())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (raw, _) = self.numbers();
        let (a, b) = (Number(raw, u32::MAX), Number(self.numbers().1, B_MAX));
        let limits = format_args!("of {CHUNK_HEADER} to {MAX_CHUNK_LEN} bytes");
        match self.rule() {
            Rule::MaskWords => write!(f, "expected {a} bitmap words, got {b}"),
            Rule::MaskPadding => f.write_str("bitmap has set bits beyond its physical row count"),
            Rule::Row => write!(f, "physical row {a} is out of bounds for {b} rows"),
            Rule::MaskWord => f.write_str("mask word is out of bounds"),
            Rule::WordPadding => f.write_str("mask word has set padding bits"),
            Rule::MaskRows => write!(f, "expected {a} physical rows, got {b}"),
            Rule::RowCounts => {
                write!(
                    f,
                    "the masks of a call have different row counts: {a} and {b}"
                )
            }
            Rule::Magic => f.write_str("the index does not hold a table"),
            Rule::Version => {
                write!(
                    f,
                    "table format version {a} is not the supported {FORMAT_VERSION}"
                )
            }
            Rule::HeaderSize => write!(f, "table header size {a} is not {HEADER_SIZE}"),
            Rule::Flags => write!(f, "table header has unknown flags {raw:#x}"),
            Rule::IndexLength => write!(f, "table index length {b} exceeds the {a} bytes given"),
            Rule::IndexShape => {
                write!(
                    f,
                    "table index length {a} is not a multiple of 8 past the header"
                )
            }
            Rule::KeyCount => write!(f, "a table has 1 to {MAX_KEYS} keys, not {a}"),
            Rule::RecordSize => {
                write!(
                    f,
                    "table record size {a} is not the {b} bytes of its keys and payload"
                )
            }
            Rule::BucketCount => write!(
                f,
                "table bucket count {a} is not a power of two of at least {MIN_BUCKETS}"
            ),
            Rule::BucketShift => write!(f, "table bucket shift {b} does not match {a} buckets"),
            Rule::Buckets => {
                write!(f, "table buckets at {b} lie outside the index of {a} bytes")
            }
            Rule::Reserved => f.write_str("table header has a reserved word set"),
            Rule::IndexAlign => f.write_str("a table index must be aligned to 8 bytes"),
            Rule::IndexShort => write!(
                f,
                "a table index of {a} bytes is shorter than the {HEADER_SIZE}-byte header"
            ),
            Rule::IndexLen => write!(f, "a table index of {a} bytes is not a multiple of 8"),
            Rule::IndexSmall => write!(
                f,
                "a table index of {a} bytes is smaller than the {b} bytes its records need"
            ),
            Rule::IndexBuckets => {
                write!(f, "a table index of {a} bytes cannot hold {b} buckets")
            }
            Rule::IndexShrinks => write!(f, "a table index does not shrink from {a} buckets"),
            Rule::Grew => f.write_str("the table changed while it grew"),
            Rule::RecordFit => write!(
                f,
                "a record of {b} keys and a payload of {a} bytes does not fit in a chunk \
                 of {MAX_CHUNK_LEN} bytes"
            ),
            Rule::RecordBytes => write!(f, "records of {a} bytes are not the records of a table"),
            Rule::RecordRoom => write!(f, "a record does not fit a chunk of {a} bytes"),
            Rule::Config => write!(
                f,
                "records of {b} keys and a payload of {a} bytes are not the table's"
            ),
            Rule::Keys => write!(f, "the table has {a} keys, the batch {b}"),
            Rule::Key => write!(f, "key {a} is past the table's {b} keys"),
            Rule::KeyRead => {
                write!(f, "the source of keys cannot give key {b} of rows word {a}")
            }
            Rule::PayloadBytes => write!(f, "the payload has {a} bytes, not {b}"),
            Rule::PayloadColumns => write!(
                f,
                "the table's payload has {a} bytes, not its words of NULL bits and {b} columns"
            ),
            Rule::PartitionColumns => write!(
                f,
                "the table's payload has {a} bytes, not a word of NULL bits and {b} columns, \
                 64 at most"
            ),
            Rule::PayloadShape => write!(
                f,
                "a payload has up to {MAX_PAYLOAD_COLUMNS} columns of values and NULL flags, \
                 not {a} and {b}"
            ),
            Rule::PayloadRows => {
                write!(f, "a payload column does not have the batch's {a} rows")
            }
            Rule::PayloadWord => write!(
                f,
                "a payload word at byte {a} is not a word of the payload of {b} bytes"
            ),
            Rule::PayloadWords => write!(
                f,
                "{a} payload words from word {b} are none or past the payload"
            ),
            Rule::BatchRows => f.write_str(
                "the references, mask and outputs of the batch have different row counts",
            ),
            Rule::NoChunk => write!(f, "table chunk {a} does not exist"),
            Rule::Chunk => write!(
                f,
                "table chunk {a} of {b} bytes is not aligned to 8 or not a multiple of 8 {limits}"
            ),
            Rule::Block => write!(
                f,
                "a table chunk of {a} bytes is not aligned to 8 or not a multiple of 8 {limits}"
            ),
            Rule::Chunks => write!(
                f,
                "a table has at most {MAX_CHUNKS} chunks with a length each, not {a} bases \
                 and {b} lengths"
            ),
            Rule::ChunkLimit => write!(f, "a table has at most {MAX_CHUNKS} chunks"),
            Rule::UsedMark => write!(
                f,
                "table chunk {a} has a used mark of {b} bytes that ends no record"
            ),
            Rule::BlockUsedMark => write!(
                f,
                "a table chunk has a used mark of {a} bytes that ends no record of {b} bytes"
            ),
            Rule::Boundary => write!(
                f,
                "byte {a} of table chunk {b} is no record boundary below its used bytes"
            ),
            Rule::Outside => write!(f, "table record offset {a} lies outside the records"),
            Rule::Misplaced => write!(f, "table record offset {a} does not start a record"),
            Rule::NoRecord => write!(f, "table chunk {a} holds no record at byte {b}"),
            Rule::Chain => f.write_str("table chain is longer than its record count"),
            Rule::Reference => write!(f, "table reference {raw:#x} names no record"),
            Rule::Cursor => write!(
                f,
                "table cursor at byte {b} of chunk {a} lies outside the records"
            ),
            Rule::CursorPast => write!(
                f,
                "table cursor at byte {b} of chunk {a} lies past its chunk's records"
            ),
            Rule::MergedFound => {
                write!(f, "table record {a} found in chunk {b}, which is merged")
            }
            Rule::Partitions => write!(
                f,
                "{a} partitions are not a power of two up to {MAX_PARTITIONS}"
            ),
            Rule::PartitionBits => {
                write!(
                    f,
                    "partition bits {a} to {b} lie past the 32 bits of a hash"
                )
            }
            Rule::PartitionRows => write!(f, "{a} row counts for {b} partitions"),
            Rule::SplitLengths => {
                f.write_str("the references and hashes of a split have different lengths")
            }
            Rule::EmptySplit => f.write_str("a split copies at least one record a call"),
            Rule::SplitIntoItself => write!(f, "table chunk {a} is split into itself"),
            Rule::MergeIntoItself => write!(f, "table chunk {a} is merged into itself"),
            Rule::Slot => write!(f, "slot {a} is past a payload cursor's {HELD}"),
            Rule::EmptySlot => write!(f, "slot {a} of a payload cursor holds no reference"),
            Rule::FilterAlign => f.write_str("a shared filter must be aligned to 8 bytes"),
            Rule::FilterWords => write!(
                f,
                "a shared filter has a state word and a power of two of words, not {a} words"
            ),
            Rule::FilterState => f.write_str("a shared filter without its state word"),
            Rule::FilterRecords => write!(f, "a filter for {a} records is too large"),
            Rule::FilterSize => write!(f, "a filter has a power of two of words, not {a}"),
            Rule::FilterFillAlign => {
                f.write_str("the words of a filter filled at once must be aligned to 8 bytes")
            }
            Rule::FilterNotBuilt => f.write_str("the shared filter is not built yet"),
            Rule::EmptyWalk => f.write_str("a walk gives at least one record a call"),
            Rule::PartitionCount => write!(f, "the count of rows of partition {a} is full"),
            Rule::MostRecords => write!(
                f,
                "a table of {a} records holds the most a find or insert may make"
            ),
        }
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} {:?}: {self}", self.kind(), self.rule())
    }
}

impl core::error::Error for Error {}

/// Return `$error` from the function when `$condition` does not hold;
/// the error is made only then, and converted into the function's.
macro_rules! ensure {
    ($condition:expr, $error:expr $(,)?) => {
        if !$condition {
            return Err(::core::convert::From::from($error));
        }
    };
}

pub(crate) use ensure;

#[cfg(test)]
mod tests {
    use super::{Error, Kind, Rule};

    #[test]
    fn an_error_keeps_its_kind_rule_and_numbers() {
        let error = Error::damaged(Rule::Cursor).with(u32::MAX).and(5);
        assert_eq!(error.kind(), Kind::Damaged);
        assert_eq!(error.rule(), Rule::Cursor);
        assert_eq!(error.numbers(), (u32::MAX, 5));
        let error = Error::wrong_call(Rule::Key).with(3).and(2);
        assert_eq!((error.kind(), error.numbers()), (Kind::WrongCall, (3, 2)));
        assert_eq!(error.to_string(), "key 3 is past the table's 2 keys");
    }

    #[test]
    fn every_rule_comes_back_from_its_error() {
        for &rule in super::RULES {
            for kind in [Kind::WrongCall, Kind::Damaged] {
                let error = Error::new(kind, rule).with(u32::MAX).and(usize::MAX);
                assert_eq!((error.kind(), error.rule()), (kind, rule));
            }
        }
    }

    #[test]
    fn a_number_wider_than_its_field_prints_or_more() {
        let error = Error::wrong_call(Rule::IndexSmall)
            .with(1_u64 << 40)
            .and(usize::MAX);
        assert_eq!(error.numbers(), (u32::MAX, (1 << 23) - 1));
        assert_eq!(
            error.to_string(),
            "a table index of 4294967295 or more bytes is smaller than the 8388607 or more \
             bytes its records need"
        );
        // The largest number that fits prints as it is.
        let error = Error::wrong_call(Rule::IndexSmall)
            .with(4096)
            .and((1 << 23) - 2);
        assert_eq!(
            error.to_string(),
            "a table index of 4096 bytes is smaller than the 8388606 bytes its records need"
        );
    }

    #[test]
    fn a_reference_prints_in_hex_without_or_more() {
        let error = Error::wrong_call(Rule::Reference).with(u32::MAX);
        assert_eq!(
            error.to_string(),
            "table reference 0xffffffff names no record"
        );
    }
}

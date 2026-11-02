//! The errors of the crate's calls.
//!
//! A call that refuses its arguments or the bytes it reads returns an
//! [`Error`]: the rule it found broken, who broke it, and up to two
//! numbers that show how. The error is one word: it allocates nothing,
//! and a call returns it in registers. Its text is written only when it
//! is printed. A limit of the host, such as the longest row it allows,
//! and an error of a source of rows the host gives, stay the host's: the
//! crate names only the rule that met them ([`Rule::ValueRead`],
//! [`Rule::ChunkRead`]), and the host keeps its own error to report.

use core::fmt;
use core::num::NonZeroU64;

use relhash::MAX_PARTITIONS;

use crate::columns::{HEADER, MAX_WORDS};
use crate::{HEADER_SIZE, VERSION, phases};

/// The result of the crate's calls.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// What a call refused: the rule it found broken, who broke it, and up
/// to two numbers that show how.
///
/// The error is one integer, so that a result of a count is two
/// registers, as with a pointer.
///
/// ```text
///   bits 56..=63   the rule's number, counted from 1
///   bit  55        the kind: 0 a wrong call, 1 damaged bytes
///   bits 23..=54   the first number, up to 2^32 - 1
///   bits 0..=22    the second number, up to 2^23 - 1
/// ```
///
/// A number wider than its field keeps the field's largest value, which
/// the message prints with "or more". Tests compare the kind and the
/// rule; the message is for people.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Error(NonZeroU64);

// One word, and a result of nothing no wider.
const _: () = assert!(size_of::<Error>() == 8);
const _: () = assert!(size_of::<Result<()>>() == 8);
// A rule's number fits its byte.
const _: () = assert!(RULES.len() < 256);

const RULE_SHIFT: u32 = 56;
const KIND_SHIFT: u32 = 55;
const A_SHIFT: u32 = 23;
const B_MAX: u32 = (1 << A_SHIFT) - 1;

/// Who broke the rule of an [`Error`]. What the call read decides the
/// kind: a value the caller passed, or one its source of rows gave, makes
/// a wrong call; a byte read back from a file makes damaged bytes. Each
/// [`Rule`] says what it refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A value the caller passed breaks the rule: a buffer too short, a
    /// count or a partition out of range, a phase a barrier cannot give,
    /// a counter carried past its word or below zero. The call changed
    /// nothing, unless its documentation says what it leaves.
    WrongCall,
    /// A byte read back from a file breaks the rule: a block header, a
    /// packed body of records or a packed chunk of columns that is not
    /// what the crate writes. The block is to be dropped; no byte of it
    /// is promised.
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
    /// A buffer shorter than a block header.
    Header,
    /// The first word of a block is not the mark of a spilled block.
    Magic,
    /// A block's format version is not the one the crate reads.
    Version,
    /// A block of a kind the crate does not know.
    BlockKind,
    /// A block of another table than the reader's.
    Fingerprint,
    /// A block's length is not a multiple of 8 up to the reader's bound.
    BlockLength,
    /// A block of records shorter than a chunk's used mark.
    UsedMark,
    /// A block of columns that is no packed chunk of columns.
    ColumnsBlock,
    /// A packed block of records that is no packed chunk of records.
    PackedRecords,
    /// A block at a level past the bits of a hash.
    Level,

    /// A packed body of records shorter than its counts.
    PackedCounts,
    /// The counts of a packed body of records do not fill its chunk.
    PackedFill,
    /// A packed body of records shorter than its codes of lanes.
    LaneCodes,
    /// A packed body of records that stores the lane of the next records.
    NextLane,
    /// A code of a lane that is none of packing's.
    LaneCode,
    /// A packed body of records that ends inside a lane.
    LaneEnd,
    /// Records of another length than their packed body's.
    RecordLength,
    /// A packed body of records that does not end with its lanes.
    PackedTail,

    /// A chunk of columns shorter than its header or not a length of 8s.
    ColumnsChunk,
    /// A chunk of columns of more stored words than it keeps.
    ColumnsWords,
    /// A chunk of columns whose header does not mark one.
    ColumnsMagic,
    /// A chunk of columns whose header does not fit its bytes.
    ColumnsShape,
    /// An output too short for a chunk of columns packed.
    ColumnsRoom,
    /// A packed chunk of columns shorter than its counts.
    ColumnsCounts,
    /// A packed chunk of columns past the limits of a chunk.
    ColumnsLimits,
    /// A packed chunk of columns that does not unpack into its output.
    ColumnsFill,
    /// A packed chunk of columns shorter than its descriptors.
    Descriptors,
    /// A packed lane of a width packing does not write.
    LaneWidth,
    /// A packed chunk of columns that ends inside a lane.
    ColumnsLaneEnd,
    /// A packed chunk of columns with bytes past its lanes.
    ColumnsTail,

    /// The least and most partitions of a level are not powers of two in
    /// order.
    PlanPartitions,
    /// A level that expects bytes below zero or not a number.
    Expected,
    /// The least partitions of a level do not fit in the hash's bits.
    PlanBits,
    /// Chunks planned for no share or no partitions.
    Share,
    /// A least chunk shorter than a chunk's header or not a multiple of 8.
    LeastChunk,

    /// Spill words for no partitions or more than the most.
    SpillPartitions,
    /// A count of words that is no spill's state.
    SpillWords,
    /// A level whose bits pass the 32 of a hash.
    SplitLevel,
    /// Partitions that are not a power of two up to the most.
    Partitions,
    /// Counts of records for another number of partitions.
    RecordCounts,
    /// A count of a partition's records carried past 2^64 - 1.
    PartitionRecords,
    /// A partition past the partitions in force.
    Partition,
    /// A file of a partition numbered past 2^32 - 1.
    FileNumber,
    /// A count of bytes carried past 2^64 - 1.
    CounterPast,
    /// A count of bytes taken below zero.
    CounterBelow,

    /// A counter of a build carried past 2^64 - 1.
    BuildCount,
    /// A participant in a state the crate does not know.
    State,
    /// A barrier's phase past the last of a build or of a round.
    AttachPhase,

    /// The chunks of columns cannot give the chunk a row goes to.
    ChunkRead,
    /// Partition bits that pass the 32 bits of a hash.
    PartitionBits,
    /// A chunk numbered past the bits a reference keeps for it.
    ChunkNumber,
    /// The hashes, offsets, columns and mask of rows of different counts.
    RowCounts,
    /// Counts of rows for another number of partitions.
    PartitionRows,
    /// A chunk of columns whose words are not the rows'.
    ChunkWidth,
    /// A chunk of columns too narrow for the rows' columns.
    ChunkColumns,
    /// A chunk of values longer than a reference names.
    ValuesLength,
    /// Values used past their chunk.
    ValuesUsed,
    /// A source of rows by value that gave bytes.
    ByValueBytes,
    /// A source of rows that cannot give a value.
    ValueRead,
    /// A word past the mask of the rows.
    Mask,

    /// A set of no partitions.
    SetPartitions,
    /// A write buffer shorter than a header and a chunk worth packing,
    /// not a multiple of 8, or not the set's.
    SetBuffer,
    /// A writer of a set in no state a call of it leaves.
    SetState,
    /// A call of a set whose last ranges were not reported written.
    SetOutstanding,
    /// A call of a set that failed a write, but for a reset.
    SetFailed,
    /// A block written to a set that is finished.
    SetFinished,
    /// A block of a partition past the set's.
    SetPartition,
    /// Another block, or a finish, where a step left a block undone.
    SetPending,
    /// A block list that counts more blocks than its region holds.
    SetList,
    /// Ranges reported written where none wait.
    SetWritten,
    /// A partition of a shared set dropped before the set is finished.
    SetDrop,
    /// A chunk that packs into more bytes than a header can say.
    PackedLength,
    /// A finish where a step left a block undone.
    FinishPending,
}

impl Error {
    /// A wrong call: a value the caller passed breaks `rule`.
    #[inline]
    pub(crate) const fn wrong_call(rule: Rule) -> Self {
        Self::new(Kind::WrongCall, rule)
    }

    /// Damaged bytes: a byte read back breaks `rule`.
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

/// The name of a counter of a build, by its word.
fn counter(word: u32) -> &'static str {
    match word as usize {
        phases::RECORDS => "records",
        phases::CHUNKS => "chunks",
        phases::DUPLICATES => "duplicates",
        _ => "counter",
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (raw, raw_b) = self.numbers();
        let (a, b) = (Number(raw, u32::MAX), Number(raw_b, B_MAX));
        match self.rule() {
            Rule::Header => write!(
                f,
                "a spilled block header needs {HEADER_SIZE} bytes, got {a}"
            ),
            Rule::Magic => f.write_str("the bytes hold no spilled block"),
            Rule::Version => write!(
                f,
                "spilled block version {a} is not the supported {VERSION}"
            ),
            Rule::BlockKind => write!(f, "a spilled block of unknown kind {a}"),
            Rule::Fingerprint => f.write_str("a spilled block belongs to another table"),
            Rule::BlockLength => write!(
                f,
                "a spilled block of {b} bytes is not a multiple of 8 up to {a}"
            ),
            Rule::UsedMark => f.write_str("a spilled chunk of records has no used mark"),
            Rule::ColumnsBlock => write!(
                f,
                "a spilled chunk of columns of {a} bytes packed into {b} is no packed chunk"
            ),
            Rule::PackedRecords => write!(
                f,
                "a spilled block packed into {b} bytes of {a} is no packed chunk of records"
            ),
            Rule::Level => write!(f, "a spilled block at level {a} is past the hash's bits"),
            Rule::PackedCounts => write!(f, "a packed chunk of {a} bytes has no counts"),
            Rule::PackedFill => write!(
                f,
                "a packed chunk of {a} records of {b} bytes does not fill its chunk"
            ),
            Rule::LaneCodes => f.write_str("a packed chunk is shorter than its lane codes"),
            Rule::NextLane => write!(
                f,
                "a packed chunk stores its next-record lane, with code {a}"
            ),
            Rule::LaneCode => write!(f, "a packed chunk has lane code {a}"),
            Rule::LaneEnd => write!(f, "a packed chunk ends inside its lane {a}"),
            Rule::RecordLength => write!(
                f,
                "the records of a packed chunk do not have its record length of {a} bytes"
            ),
            Rule::PackedTail => write!(
                f,
                "a packed chunk of {a} bytes does not end with its lanes, at {b}"
            ),
            Rule::ColumnsChunk => write!(
                f,
                "a chunk of columns needs a header of {HEADER} bytes and a length of 8s, not {a}"
            ),
            Rule::ColumnsWords => write!(
                f,
                "a chunk of columns keeps up to {MAX_WORDS} words, not {a}"
            ),
            Rule::ColumnsMagic => f.write_str("the bytes hold no chunk of columns"),
            Rule::ColumnsShape => write!(
                f,
                "a chunk of columns of {a} places of {b} words does not fit its bytes, or \
                 holds more rows"
            ),
            Rule::ColumnsRoom => {
                write!(f, "a chunk of columns packs into up to {a} bytes, not {b}")
            }
            Rule::ColumnsCounts => f.write_str("a packed chunk of columns has no counts"),
            Rule::ColumnsLimits => write!(
                f,
                "a packed chunk of {a} rows of {b} words is past the limits of a chunk"
            ),
            Rule::ColumnsFill => write!(
                f,
                "a packed chunk of {a} rows of {b} words does not unpack into its chunk"
            ),
            Rule::Descriptors => {
                f.write_str("a packed chunk of columns is shorter than its descriptors")
            }
            Rule::LaneWidth => write!(f, "a packed lane of width {a}"),
            Rule::ColumnsLaneEnd => write!(f, "a packed chunk of columns ends inside lane {a}"),
            Rule::ColumnsTail => {
                write!(f, "a packed chunk of columns has {a} bytes past its lanes")
            }
            Rule::PlanPartitions => {
                write!(f, "{a} to {b} partitions are not powers of two in order")
            }
            Rule::Expected => f.write_str("a level expects bytes below zero or not a number"),
            Rule::PlanBits => write!(f, "{a} partitions do not fit in the hash after {b} bits"),
            Rule::Share => write!(f, "chunks of a {a} share of {b} partitions"),
            Rule::LeastChunk => write!(f, "a least chunk of {a} bytes"),
            Rule::SpillPartitions => write!(
                f,
                "a shared spill of {a} partitions is not 1 to {MAX_PARTITIONS}"
            ),
            Rule::SpillWords => write!(f, "{a} words are no spill's state"),
            Rule::SplitLevel => write!(f, "a level of {a} bits is past the 32 of the hash"),
            Rule::Partitions => write!(f, "{a} partitions are not a power of two up to {b}"),
            Rule::RecordCounts => write!(f, "{a} counts of records for {b} partitions"),
            Rule::PartitionRecords => {
                write!(f, "the records of partition {a} cannot take {b} more")
            }
            Rule::Partition => write!(f, "partition {a} is past the {b} of the table"),
            Rule::FileNumber => write!(f, "a file of partition {a} is past 2^32 - 1"),
            Rule::CounterPast => write!(
                f,
                "a spill counter of {a} bytes passed 2^64 - 1 with {b} more"
            ),
            Rule::CounterBelow => write!(
                f,
                "a spill counter of {a} bytes went below zero with {b} less"
            ),
            Rule::BuildCount => write!(f, "the {} of a build cannot take {b} more", counter(raw)),
            Rule::State => f.write_str("a participant in no known state"),
            Rule::AttachPhase => write!(f, "a participant attached at phase {a}, past the last"),
            Rule::ChunkRead => write!(f, "the chunks of columns cannot give chunk {a}"),
            Rule::PartitionBits => write!(
                f,
                "partition bits {a} to {b} lie past the 32 bits of a hash"
            ),
            Rule::ChunkNumber => write!(f, "a chunk number past {a} bits"),
            Rule::RowCounts => f.write_str(
                "the hashes, offsets, columns and mask of the rows have different row counts",
            ),
            Rule::PartitionRows => write!(f, "{a} row counts for {b} partitions"),
            Rule::ChunkWidth => write!(f, "a chunk of {a} words takes rows of {b}"),
            Rule::ChunkColumns => write!(f, "a chunk of {a} words takes no row of {b} columns"),
            Rule::ValuesLength => write!(
                f,
                "a chunk of values of {a} bytes is longer than a reference names"
            ),
            Rule::ValuesUsed => write!(f, "values used to byte {b} of {a}"),
            Rule::ByValueBytes => write!(f, "a by-value column {a} gave bytes"),
            Rule::ValueRead => write!(f, "the source of rows cannot give column {a} of row {b}"),
            Rule::Mask => f.write_str("a word past the mask of the rows"),
            Rule::SetPartitions => write!(f, "a spill set of {a} partitions"),
            Rule::SetBuffer => write!(
                f,
                "a spill set's write buffer of {a} bytes where it takes {b}"
            ),
            Rule::SetState => f.write_str("a spill set in no state its writer keeps"),
            Rule::SetOutstanding => {
                f.write_str("a spill set's last ranges were not reported written")
            }
            Rule::SetFailed => f.write_str("a spill set that failed a write takes only a reset"),
            Rule::SetFinished => f.write_str("a spill set that was finished takes no more blocks"),
            Rule::SetPartition => write!(f, "partition {a} is past the {b} of the spill set"),
            Rule::SetPending => write!(
                f,
                "a block of partition {a} where a step left one of partition {b} undone"
            ),
            Rule::SetList => write!(f, "a block list of {a} entries counts {b} blocks"),
            Rule::SetWritten => f.write_str("no ranges of the spill set wait to be written"),
            Rule::SetDrop => write!(
                f,
                "a shared spill set dropped partition {a} before it was finished"
            ),
            Rule::PackedLength => write!(f, "a chunk packs into {a} bytes, past a header's"),
            Rule::FinishPending => write!(
                f,
                "a spill set finished where a step left a block of partition {a} undone"
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
/// the error is made only then.
macro_rules! ensure {
    ($condition:expr, $error:expr $(,)?) => {
        if !$condition {
            return Err($error);
        }
    };
}

pub(crate) use ensure;

#[cfg(test)]
mod tests {
    use super::{Error, Kind, Rule};

    #[test]
    fn an_error_keeps_its_kind_rule_and_numbers() {
        let error = Error::damaged(Rule::PackedTail).with(u32::MAX).and(5);
        assert_eq!(error.kind(), Kind::Damaged);
        assert_eq!(error.rule(), Rule::PackedTail);
        assert_eq!(error.numbers(), (u32::MAX, 5));
        let error = Error::wrong_call(Rule::Partition).with(3).and(2);
        assert_eq!((error.kind(), error.numbers()), (Kind::WrongCall, (3, 2)));
        assert_eq!(error.to_string(), "partition 3 is past the 2 of the table");
    }

    #[test]
    fn every_rule_comes_back_from_its_error() {
        for &rule in super::RULES {
            for kind in [Kind::WrongCall, Kind::Damaged] {
                let error = Error::new(kind, rule).with(u32::MAX).and(usize::MAX);
                assert_eq!((error.kind(), error.rule()), (kind, rule));
                assert!(!error.to_string().is_empty());
            }
        }
    }

    #[test]
    fn a_number_wider_than_its_field_prints_or_more() {
        let error = Error::wrong_call(Rule::BlockLength)
            .with(1_u64 << 40)
            .and(usize::MAX);
        assert_eq!(error.numbers(), (u32::MAX, (1 << 23) - 1));
        assert_eq!(
            error.to_string(),
            "a spilled block of 8388607 or more bytes is not a multiple of 8 up to \
             4294967295 or more"
        );
    }
}

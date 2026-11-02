//! The file of a set, written.
//!
//! A set is the temporary file of one level of a spill: the blocks of
//! every partition, one after another in the order they were written,
//! and a block list for each partition, an [`Entry`] for each of its
//! blocks. A shared set is one such file for each participant of a
//! query. At its end, after its blocks, it holds its lists and a
//! trailer, so that the other participants can read it:
//!
//! ```text
//!  ┌────────┬───┬──────────────────┬───────────────────┬──────────┐
//!  │ blocks │ … │ blocks in each   │ for each block of │ trailer  │
//!  │        │   │ partition,       │ each partition:   │          │
//!  │        │   │ 8 bytes each     │ an Entry, 16      │ 32 bytes │
//!  └────────┴───┴──────────────────┴───────────────────┴──────────┘
//!
//!  the trailer, words of 8 bytes
//!  ┌──────────────┬───────────────────────┬────────────┬─────────────┐
//!  │ TRAILER_MAGIC│ where the lists start │ partitions │ fingerprint │
//!  └──────────────┴───────────────────────┴────────────┴─────────────┘
//! ```
//!
//! Every word is in the machine's byte order, as the headers of the
//! blocks are.
//!
//! [`Writer`] writes a set without writing a byte to a file. Its caller
//! keeps the file and three kinds of region: the write buffer, a scratch
//! region for a block packed aside, and the block lists ([`Lists`]). A
//! call takes the regions it needs and returns a [`Step`]: the
//! [`Range`]s of those regions, and of the block's own body, to write and
//! where in the file; or the size a region must grow to. The caller
//! writes the ranges in their order and reports them with
//! [`Writer::written`]. Until then the writer takes no other call, since
//! the ranges may name its buffer, and the caller changes none of the
//! regions they name. A caller that could not write them reports
//! [`Writer::failed`]; the writer then takes only a [`Writer::reset`].
//!
//! The writer packs each block into the buffer when it fits what the
//! buffer has left, so that the file is written in large pieces however
//! small the blocks: a chunk of records when packing makes it shorter
//! ([`crate::pack`]), a chunk of columns always ([`crate::columns`]).
//! A block that does not fit what is left starts a new buffer, after a
//! step that writes the old one; a block larger than the buffer goes to
//! the file on its own, its header from the buffer and its body from the
//! caller's body or the scratch region.

use crate::error::ensure;
use crate::{BlockHeader, BlockKind, Error, HEADER_SIZE, MAX_LEVEL, Result, Rule, columns};

mod reader;

pub use reader::{READER_WORDS, ReadConfig, ReadStep, Reader};

/// The first word of a shared set's trailer.
pub const TRAILER_MAGIC: u64 = 0x5445_5353_5452_4149;

/// Bytes of a shared set's trailer.
pub const TRAILER_SIZE: usize = 32;

/// Bytes of an [`Entry`] in a list, and at the end of a shared file.
pub const ENTRY_SIZE: usize = 16;

/// The shortest write buffer: a block's header and the shortest chunk of
/// records that packing takes.
pub const MIN_BUFFER: usize = HEADER_SIZE + 64;

/// The most ranges one step asks to write.
pub const MAX_RANGES: usize = 3;

/// The shortest list a block list grows to.
pub const MIN_LIST: usize = 16;

/// Where a block lies in the file of its set: where its header starts,
/// and the bytes it takes, header included. A partition's block list is
/// an array of these in the order its blocks were written, and the end of
/// a shared file holds them as they lie in memory.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Entry {
    pub offset: u64,
    pub stored: u64,
}

/// The block lists of a set, one for each partition, in regions its
/// caller keeps. The caller grows a list when a step asks
/// ([`Step::List`]): a region of more entries that begins with the old
/// one's.
pub trait Lists {
    /// The list of `partition`, below the set's partitions: its region,
    /// whose first `*count` entries are its blocks, and that count.
    fn list(&mut self, partition: usize) -> (&mut [Entry], &mut u64);
}

/// A region a range is written from.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Region {
    /// The write buffer.
    #[default]
    Buffer,
    /// The scratch region, which holds a block packed aside.
    Scratch,
    /// The body of the block the call writes, as the caller gave it.
    Body,
    /// The region of a partition's block list, its entries as bytes.
    List(u32),
}

/// Bytes to write: `len` bytes of `region` from byte `start`, to the
/// file at byte `at`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Range {
    pub region: Region,
    pub start: usize,
    pub len: usize,
    pub at: u64,
}

/// The ranges a step asks to write, in their order: at most
/// [`MAX_RANGES`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Ranges {
    count: usize,
    items: [Range; MAX_RANGES],
}

impl Ranges {
    fn one(range: Range) -> Self {
        let mut ranges = Self::default();
        ranges.push(range);
        ranges
    }

    fn push(&mut self, range: Range) {
        self.items[self.count] = range;
        self.count += 1;
    }

    fn is_full(&self) -> bool {
        self.count == MAX_RANGES
    }

    /// The ranges, in the order to write them.
    pub fn as_slice(&self) -> &[Range] {
        &self.items[..self.count]
    }
}

/// What a call of a [`Writer`] asks of its caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    /// Write the ranges, report them with [`Writer::written`], and make
    /// the same call again: it is not done.
    Again(Ranges),
    /// The call is done. Write the ranges, if there are any, and report
    /// them. A block written starts at `offset` in the file and takes
    /// `stored` bytes, header included; a finish gives 0 for both.
    Done {
        ranges: Ranges,
        offset: u64,
        stored: u64,
    },
    /// The scratch region must hold this many bytes. Nothing changed.
    Scratch(usize),
    /// The block list of the block's partition must hold this many
    /// entries. Nothing changed.
    List(usize),
}

/// A block to write: a chunk of `kind`, its `number` and its
/// `partition`, and its bytes as they lie in memory.
#[derive(Clone, Copy, Debug)]
pub struct Block<'a> {
    pub kind: BlockKind,
    pub number: u32,
    pub partition: u32,
    pub body: &'a [u8],
}

/// What a set is made with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    /// The partitions of the level, at least one.
    pub partitions: u32,
    /// The level, below [`MAX_LEVEL`].
    pub level: u32,
    /// The table's layout fingerprint, which every header carries.
    pub fingerprint: u64,
    /// The most body bytes a reader of the set accepts.
    pub max_len: u64,
    /// Bytes of the write buffer: a multiple of 8, at least
    /// [`MIN_BUFFER`].
    pub buffer_len: usize,
    /// Whether the set is a participant's of a shared set, whose finish
    /// writes the lists and the trailer.
    pub shared: bool,
}

// The phase of a set. A zeroed writer, in phase 0, takes no call.
const WRITING: u32 = 1;
const FINISHING: u32 = 2;
const FINISHED: u32 = 3;
const FAILED: u32 = 4;

// A block a step left to the next call: none; a chunk of columns to pack
// into the buffer that step emptied; a block to place after that step,
// its body packed in the scratch region, or the caller's body as it is.
const NONE: u32 = 0;
const COLUMNS: u32 = 1;
const FROM_SCRATCH: u32 = 2;
const FROM_BODY: u32 = 3;

// The stage of a finish: the counts of the lists, their entries, the
// trailer.
const COUNTS: u32 = 1;
const ENTRIES: u32 = 2;
const TRAILER: u32 = 3;

/// The writer of a set: plain numbers, which the caller keeps where it
/// likes, such as in memory it shares with C. Every call checks them
/// first, so that a writer the caller changed is a wrong call, never a
/// write out of place.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Writer {
    fingerprint: u64,
    max_len: u64,
    buffer_len: u64,
    /// The file's length, with what the buffer holds, and what it holds.
    end: u64,
    buffered: u64,
    blocks: u64,
    bytes: u64,
    /// A block a step left: its body's length as stored, the length its
    /// header gives, and the length the caller gave.
    pending_stored: u64,
    pending_len: u64,
    pending_given: u64,
    /// At a finish: where the lists start, and where the next step
    /// writes.
    lists_at: u64,
    next_at: u64,
    partitions: u32,
    level: u32,
    files: u32,
    shared: u32,
    phase: u32,
    outstanding: u32,
    pending: u32,
    pending_packed: u32,
    pending_kind: u32,
    pending_number: u32,
    pending_partition: u32,
    finish: u32,
    next_partition: u32,
}

// Its size is part of its C mirror: words of 8 bytes.
const _: () = assert!(size_of::<Writer>() == 8 * WRITER_WORDS);

/// Words of 8 bytes a [`Writer`] takes.
pub const WRITER_WORDS: usize = 19;

/// Where the body of a block lies when it is placed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Source {
    /// Packed in the buffer, past the header.
    Buffer,
    Scratch,
    Body,
}

impl Writer {
    /// A writer of an empty set made with `config`.
    pub fn new(config: Config) -> Result<Self> {
        ensure!(
            config.partitions > 0,
            Error::wrong_call(Rule::SetPartitions).with(config.partitions)
        );
        ensure!(
            config.level < MAX_LEVEL,
            Error::wrong_call(Rule::Level).with(config.level)
        );
        ensure!(
            config.buffer_len >= MIN_BUFFER && config.buffer_len.is_multiple_of(8),
            Error::wrong_call(Rule::SetBuffer)
                .with(config.buffer_len)
                .and(MIN_BUFFER)
        );
        Ok(Self {
            fingerprint: config.fingerprint,
            max_len: config.max_len,
            buffer_len: config.buffer_len as u64,
            partitions: config.partitions,
            level: config.level,
            shared: u32::from(config.shared),
            phase: WRITING,
            ..Self::default()
        })
    }

    /// The partitions of the set, and of its lists.
    pub fn partitions(&self) -> u32 {
        self.partitions
    }

    /// Blocks written.
    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    /// Bytes the blocks take in the file, headers included.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Partitions whose lists hold blocks.
    pub fn files(&self) -> u32 {
        self.files
    }

    /// Whether the set was finished, and the ranges of its last step
    /// reported written: it takes no more blocks, and its write buffer is
    /// no longer needed.
    pub fn is_finished(&self) -> bool {
        self.phase == FINISHED && self.outstanding == 0
    }

    /// The writer's numbers, checked: the phase is one it keeps, and the
    /// buffer, the file and a block a step left agree.
    fn check_state(&self) -> Result<()> {
        ensure!(
            matches!(self.phase, WRITING..=FAILED)
                && self.partitions > 0
                && self.level < MAX_LEVEL
                && self.buffer_len >= MIN_BUFFER as u64
                && self.buffered <= self.buffer_len
                && self.buffered <= self.end
                && self.bytes <= self.end
                && self.blocks <= self.bytes
                && self.lists_at <= self.next_at
                && self.outstanding <= 1
                && self.pending <= FROM_BODY
                && self.finish <= TRAILER
                && self.next_partition <= self.partitions
                && self.files <= self.partitions
                // A block left to be placed from the caller's body takes
                // the body's length; one packed aside is checked against
                // the scratch region where it lies.
                && (self.pending != FROM_BODY || self.pending_stored == self.pending_given),
            Error::wrong_call(Rule::SetState)
        );
        ensure!(self.phase != FAILED, Error::wrong_call(Rule::SetFailed));
        ensure!(
            self.outstanding == 0,
            Error::wrong_call(Rule::SetOutstanding)
        );
        Ok(())
    }

    /// As [`Self::check_state`], and `buffer` is the set's.
    fn check(&self, buffer: &[u8]) -> Result<()> {
        self.check_state()?;
        ensure!(
            buffer.len() as u64 == self.buffer_len,
            Error::wrong_call(Rule::SetBuffer)
                .with(buffer.len())
                .and(self.buffer_len)
        );
        Ok(())
    }

    /// A step that writes what the buffer holds, at its place in the
    /// file, and empties it; the caller calls again.
    fn flush(&mut self) -> Step {
        let at = self.end - self.buffered;
        let len = self.buffered as usize;
        self.buffered = 0;
        self.outstanding = 1;
        Step::Again(Ranges::one(Range {
            region: Region::Buffer,
            start: 0,
            len,
            at,
        }))
    }

    /// Leave `block` to the next call, at `stage`, and write the buffer.
    fn flush_first(&mut self, stage: u32, block: &Block<'_>, header: &BlockHeader) -> Step {
        self.pending = stage;
        self.pending_kind = block.kind as u32;
        self.pending_number = block.number;
        self.pending_partition = block.partition;
        self.pending_given = block.body.len() as u64;
        self.pending_len = header.len;
        self.pending_packed = header.packed;
        self.flush()
    }

    /// Write `block` into the set: a step that places it in the buffer,
    /// or writes it on its own, and gives where it starts; or one that
    /// writes the buffer first, after which the caller makes the same call
    /// again; or the size a region must grow to.
    ///
    /// `buffer` is the set's write buffer, of the length it was made
    /// with. `scratch` is a region the caller keeps for a block packed
    /// aside, empty until a step asks for it. Between the steps of one
    /// block the caller changes neither, nor the block.
    ///
    /// # Errors
    ///
    /// A wrong call: a set that is not writing (finished, failed, or with
    /// ranges not reported written), a partition past the set's, another
    /// buffer, a list that counts more blocks than its region holds,
    /// another block than the one a step left, a chunk of columns that is
    /// none, or a header [`BlockHeader::write`] refuses. The set is then
    /// as it was, but for the bytes of the buffer past what it holds.
    pub fn write(
        &mut self,
        buffer: &mut [u8],
        scratch: &mut [u8],
        lists: &mut impl Lists,
        block: Block<'_>,
    ) -> Result<Step> {
        self.check(buffer)?;
        ensure!(self.phase == WRITING, Error::wrong_call(Rule::SetFinished));
        ensure!(
            block.partition < self.partitions,
            Error::wrong_call(Rule::SetPartition)
                .with(block.partition)
                .and(self.partitions)
        );
        if self.pending != NONE {
            ensure!(
                self.pending_kind == block.kind as u32
                    && self.pending_number == block.number
                    && self.pending_partition == block.partition
                    && self.pending_given == block.body.len() as u64,
                Error::wrong_call(Rule::SetPending)
                    .with(block.partition)
                    .and(self.pending_partition)
            );
        }
        let (entries, count) = lists.list(block.partition as usize);
        let used = *count as usize;
        ensure!(
            *count <= entries.len() as u64,
            Error::wrong_call(Rule::SetList)
                .with(entries.len())
                .and(*count)
        );
        if used == entries.len() {
            return Ok(Step::List((2 * used).max(MIN_LIST)));
        }
        let len = block.body.len();
        let buffer_len = self.buffer_len as usize;
        let buffered = self.buffered as usize;
        let mut header = BlockHeader {
            kind: block.kind,
            number: block.number,
            partition: block.partition,
            level: self.level,
            fingerprint: self.fingerprint,
            len: len as u64,
            packed: 0,
        };
        let mut bytes = [0_u8; HEADER_SIZE];
        let (source, body_len) = match self.pending {
            FROM_SCRATCH | FROM_BODY => {
                header.len = self.pending_len;
                header.packed = self.pending_packed;
                let stored = self.pending_stored as usize;
                if self.pending == FROM_SCRATCH {
                    ensure!(
                        stored <= scratch.len(),
                        Error::wrong_call(Rule::SetPending)
                            .with(block.partition)
                            .and(self.pending_partition)
                    );
                    (Source::Scratch, stored)
                } else {
                    (Source::Body, stored)
                }
            }
            _ if block.kind == BlockKind::Columns => {
                // Packed always: into the buffer when its bytes and the
                // slack fit what is left there, else into an empty
                // buffer, and aside only when larger than the buffer.
                let words = columns::shape(block.body)?.words;
                let room = len + columns::pack_slack(words);
                if self.pending == NONE
                    && buffered > 0
                    && buffered + HEADER_SIZE + room > buffer_len
                    && HEADER_SIZE + room <= buffer_len
                {
                    return Ok(self.flush_first(COLUMNS, &block, &header));
                }
                let in_buffer = buffered + HEADER_SIZE + room <= buffer_len;
                if !in_buffer && scratch.len() < room {
                    return Ok(Step::Scratch(room));
                }
                let out = if in_buffer {
                    &mut buffer[buffered + HEADER_SIZE..buffered + HEADER_SIZE + room]
                } else {
                    &mut scratch[..room]
                };
                let (packed, unpacked) = columns::pack(block.body, out)?;
                header.len = unpacked as u64;
                header.packed = u32::try_from(packed)
                    .map_err(|_| Error::wrong_call(Rule::PackedLength).with(packed))?;
                header.write(&mut bytes, self.max_len)?;
                let source = if in_buffer {
                    Source::Buffer
                } else {
                    Source::Scratch
                };
                (source, packed)
            }
            _ => {
                // The header is checked before a byte of the block goes
                // anywhere. A chunk of records goes packed when that
                // makes it shorter: straight into the buffer when its
                // bytes fit what is left there, else aside.
                header.write(&mut bytes, self.max_len)?;
                let mut placed = (Source::Body, len);
                if block.kind == BlockKind::Records && len >= 64 && len <= u32::MAX as usize {
                    let fits = buffered + HEADER_SIZE + len <= buffer_len;
                    if !fits && scratch.len() < len {
                        return Ok(Step::Scratch(len));
                    }
                    let out = if fits {
                        &mut buffer[buffered + HEADER_SIZE..buffered + HEADER_SIZE + len]
                    } else {
                        &mut scratch[..len]
                    };
                    if let Some(packed) = crate::pack(block.body, out) {
                        header.packed = packed as u32;
                        placed = (
                            if fits {
                                Source::Buffer
                            } else {
                                Source::Scratch
                            },
                            packed,
                        );
                    }
                }
                placed
            }
        };
        header.write(&mut bytes, self.max_len)?;
        let stored = HEADER_SIZE + body_len;
        let end = self
            .end
            .checked_add(stored as u64)
            .ok_or(Error::wrong_call(Rule::SetState))?;
        // A block that does not fit what the buffer has left starts a new
        // buffer; one packed in place fit, so only a block aside comes
        // here.
        if buffered > 0 && buffered + stored > buffer_len {
            let stage = if source == Source::Scratch {
                FROM_SCRATCH
            } else {
                FROM_BODY
            };
            self.pending_stored = body_len as u64;
            return Ok(self.flush_first(stage, &block, &header));
        }
        let offset = self.end;
        let mut ranges = Ranges::default();
        if stored <= buffer_len {
            buffer[buffered..buffered + HEADER_SIZE].copy_from_slice(&bytes);
            let to = &mut buffer[buffered + HEADER_SIZE..buffered + stored];
            match source {
                Source::Buffer => {}
                Source::Scratch => to.copy_from_slice(&scratch[..body_len]),
                Source::Body => to.copy_from_slice(&block.body[..body_len]),
            }
            self.buffered += stored as u64;
        } else {
            // Larger than the buffer, which is empty: written on its own.
            buffer[..HEADER_SIZE].copy_from_slice(&bytes);
            ranges.push(Range {
                region: Region::Buffer,
                start: 0,
                len: HEADER_SIZE,
                at: offset,
            });
            if body_len > 0 {
                ranges.push(Range {
                    region: if source == Source::Scratch {
                        Region::Scratch
                    } else {
                        Region::Body
                    },
                    start: 0,
                    len: body_len,
                    at: offset + HEADER_SIZE as u64,
                });
            }
            self.outstanding = 1;
        }
        entries[used] = Entry {
            offset,
            stored: stored as u64,
        };
        *count += 1;
        if used == 0 {
            self.files = self.files.saturating_add(1);
        }
        self.end = end;
        self.blocks += 1;
        self.bytes += stored as u64;
        self.pending = NONE;
        Ok(Step::Done {
            ranges,
            offset,
            stored: stored as u64,
        })
    }

    /// The ranges of the last step were written.
    ///
    /// # Errors
    ///
    /// A wrong call when none wait to be reported, or the set failed.
    pub fn written(&mut self) -> Result<()> {
        ensure!(
            matches!(self.phase, WRITING..=FAILED),
            Error::wrong_call(Rule::SetState)
        );
        ensure!(self.phase != FAILED, Error::wrong_call(Rule::SetFailed));
        ensure!(self.outstanding == 1, Error::wrong_call(Rule::SetWritten));
        self.outstanding = 0;
        Ok(())
    }

    /// The caller could not write what a step asked, in whole or in part.
    /// The set then takes only a [`Self::reset`].
    pub fn failed(&mut self) {
        self.phase = FAILED;
        self.outstanding = 0;
        self.pending = NONE;
    }

    /// Make the set empty again, with every list empty, whatever its
    /// phase: the caller has removed its file, or will write it anew from
    /// its start.
    ///
    /// # Errors
    ///
    /// A wrong call when the writer is none a call of [`Self::new`] made.
    pub fn reset(&mut self, lists: &mut impl Lists) -> Result<()> {
        ensure!(
            matches!(self.phase, WRITING..=FAILED) && self.partitions > 0,
            Error::wrong_call(Rule::SetState)
        );
        for partition in 0..self.partitions as usize {
            *lists.list(partition).1 = 0;
        }
        *self = Self {
            fingerprint: self.fingerprint,
            max_len: self.max_len,
            buffer_len: self.buffer_len,
            partitions: self.partitions,
            level: self.level,
            shared: self.shared,
            phase: WRITING,
            ..Self::default()
        };
        Ok(())
    }

    /// Finish the set: a step at a time, write what the buffer holds and,
    /// for a shared set that wrote a block, the lists and the trailer
    /// after the blocks. The last step is [`Step::Done`]; after it the
    /// set takes no more blocks, and a finished set's finish is done at
    /// once. Between the steps the caller changes neither the buffer nor
    /// the lists.
    ///
    /// # Errors
    ///
    /// A wrong call: the set failed, has ranges not reported written, was
    /// given another buffer, a step of a block was left undone, or a list
    /// counts more blocks than its region holds.
    pub fn finish(&mut self, buffer: &mut [u8], lists: &mut impl Lists) -> Result<Step> {
        let done = Step::Done {
            ranges: Ranges::default(),
            offset: 0,
            stored: 0,
        };
        if self.phase == FINISHED {
            self.check_state()?;
            return Ok(done);
        }
        self.check(buffer)?;
        let partitions = self.partitions as usize;
        // Every list is checked before a step takes any of them, so that a
        // refused step changes nothing.
        for partition in 0..partitions {
            let (entries, count) = lists.list(partition);
            ensure!(
                *count <= entries.len() as u64,
                Error::wrong_call(Rule::SetList)
                    .with(entries.len())
                    .and(*count)
            );
        }
        // A step of the counts moves the offset by a buffer at most, the
        // trailer by its bytes: checked here, before a word moves.
        ensure!(
            self.next_at
                .max(self.end)
                .checked_add(self.buffer_len + TRAILER_SIZE as u64)
                .is_some(),
            Error::wrong_call(Rule::SetState)
        );
        if self.phase == WRITING {
            ensure!(
                self.pending == NONE,
                Error::wrong_call(Rule::FinishPending).with(self.pending_partition)
            );
            self.phase = FINISHING;
        }
        if self.buffered > 0 {
            return Ok(self.flush());
        }
        if self.shared == 0 || self.end == 0 {
            self.phase = FINISHED;
            return Ok(done);
        }
        if self.finish == NONE {
            self.finish = COUNTS;
            self.lists_at = self.end;
            self.next_at = self.end;
            self.next_partition = 0;
        }
        if self.finish == COUNTS {
            // A count for each partition, as many as the buffer holds.
            let from = self.next_partition as usize;
            let take = (partitions - from).min(self.buffer_len as usize / 8);
            for (word, partition) in buffer
                .as_chunks_mut::<8>()
                .0
                .iter_mut()
                .zip(from..from + take)
            {
                word.copy_from_slice(&lists.list(partition).1.to_ne_bytes());
            }
            let range = Range {
                region: Region::Buffer,
                start: 0,
                len: 8 * take,
                at: self.next_at,
            };
            self.next_at = self
                .next_at
                .checked_add(8 * take as u64)
                .ok_or(Error::wrong_call(Rule::SetState))?;
            self.next_partition += take as u32;
            if self.next_partition as usize == partitions {
                self.finish = ENTRIES;
                self.next_partition = 0;
            }
            self.outstanding = 1;
            return Ok(Step::Again(Ranges::one(range)));
        }
        if self.finish == ENTRIES {
            // The ranges of up to three lists, its words moved only once
            // every one is taken.
            let mut ranges = Ranges::default();
            let (mut next_at, mut next_partition) = (self.next_at, self.next_partition);
            while (next_partition as usize) < partitions && !ranges.is_full() {
                let count = *lists.list(next_partition as usize).1;
                if count > 0 {
                    let len = count as usize * ENTRY_SIZE;
                    ranges.push(Range {
                        region: Region::List(next_partition),
                        start: 0,
                        len,
                        at: next_at,
                    });
                    next_at = next_at
                        .checked_add(len as u64)
                        .ok_or(Error::wrong_call(Rule::SetState))?;
                }
                next_partition += 1;
            }
            (self.next_at, self.next_partition) = (next_at, next_partition);
            if self.next_partition as usize == partitions {
                self.finish = TRAILER;
            }
            if !ranges.as_slice().is_empty() {
                self.outstanding = 1;
                return Ok(Step::Again(ranges));
            }
        }
        // The trailer: the magic, where the lists start, the partitions
        // and the fingerprint.
        for (word, value) in buffer.as_chunks_mut::<8>().0.iter_mut().zip([
            TRAILER_MAGIC,
            self.lists_at,
            u64::from(self.partitions),
            self.fingerprint,
        ]) {
            word.copy_from_slice(&value.to_ne_bytes());
        }
        self.phase = FINISHED;
        self.outstanding = 1;
        Ok(Step::Done {
            ranges: Ranges::one(Range {
                region: Region::Buffer,
                start: 0,
                len: TRAILER_SIZE,
                at: self.next_at,
            }),
            offset: 0,
            stored: 0,
        })
    }

    /// Forget the blocks of `partition`: its list empties, and the
    /// partition no longer counts among those with blocks. Their bytes
    /// stay in the file until the set goes. A shared set drops a
    /// partition only once it is finished, as its finish writes the lists
    /// the others read.
    ///
    /// # Errors
    ///
    /// A wrong call: a partition past the set's, a shared set not
    /// finished, a set that failed or has ranges not reported written.
    pub fn drop_list(&mut self, lists: &mut impl Lists, partition: u32) -> Result<()> {
        self.check_state()?;
        ensure!(
            partition < self.partitions,
            Error::wrong_call(Rule::SetPartition)
                .with(partition)
                .and(self.partitions)
        );
        ensure!(
            self.shared == 0 || self.phase == FINISHED,
            Error::wrong_call(Rule::SetDrop).with(partition)
        );
        let (_, count) = lists.list(partition as usize);
        if *count > 0 {
            *count = 0;
            self.files = self.files.saturating_sub(1);
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::panic::Location;

    use anyhow::{Context, Result, ensure};
    use proptest::prelude::*;
    use proptest::test_runner::{FileFailurePersistence, TestCaseError, TestRunner};
    use relhash::{CHUNK_HEADER, RECORD_SIZE_OFFSET};

    use super::*;
    use crate::Kind;

    /// Run `test` on the cases of `strategy`, and fail with the smallest
    /// failing case, which `set.proptest-regressions` beside this file
    /// keeps for the next runs.
    #[track_caller]
    #[allow(
        clippy::panic,
        reason = "a failed property fails its test, as an assertion does"
    )]
    pub(crate) fn property<S: Strategy>(strategy: S, test: impl Fn(S::Value) -> Result<()>) {
        let config = ProptestConfig {
            source_file: Some(Location::caller().file()),
            failure_persistence: Some(Box::new(FileFailurePersistence::WithSource(
                "proptest-regressions",
            ))),
            ..ProptestConfig::default()
        };
        let outcome = TestRunner::new(config).run(&strategy, |case| {
            test(case).map_err(|error| TestCaseError::fail(format!("{error:?}")))
        });
        if let Err(failure) = outcome {
            panic!("{failure}");
        }
    }

    /// The block lists of a test: a region and a count for each
    /// partition.
    pub(crate) type TestLists = Vec<(Vec<Entry>, u64)>;

    impl Lists for TestLists {
        fn list(&mut self, partition: usize) -> (&mut [Entry], &mut u64) {
            let (entries, count) = &mut self[partition];
            (entries, count)
        }
    }

    /// The bytes of `entries` as a list lies in memory.
    pub(crate) fn entry_bytes(entries: &[Entry]) -> Vec<u8> {
        entries
            .iter()
            .flat_map(|entry| [entry.offset, entry.stored])
            .flat_map(u64::to_ne_bytes)
            .collect()
    }

    pub(crate) const FINGERPRINT: u64 = 0x1234_5678_9abc_def0;
    pub(crate) const MAX_LEN: u64 = 1 << 20;

    /// A block of a test, owning its body.
    #[derive(Clone, Debug)]
    pub(crate) struct Owned {
        pub(crate) kind: BlockKind,
        pub(crate) number: u32,
        pub(crate) partition: u32,
        pub(crate) body: Vec<u8>,
    }

    impl Owned {
        fn block(&self) -> Block<'_> {
            Block {
                kind: self.kind,
                number: self.number,
                partition: self.partition,
                body: &self.body,
            }
        }
    }

    /// A host that keeps the file of a set as bytes in memory, and grows
    /// its regions when a step asks.
    pub(crate) struct Host {
        pub(crate) writer: Writer,
        buffer: Vec<u8>,
        scratch: Vec<u8>,
        pub(crate) lists: TestLists,
        pub(crate) file: Vec<u8>,
        /// Ranges left to write before one is written only in part.
        cut: Option<usize>,
    }

    impl Host {
        pub(crate) fn new(config: Config) -> Result<Self> {
            Ok(Self {
                writer: Writer::new(config)?,
                buffer: vec![0; config.buffer_len],
                scratch: Vec::new(),
                lists: vec![(Vec::new(), 0); config.partitions as usize],
                file: Vec::new(),
                cut: None,
            })
        }

        /// Write `ranges` into the file: false when the cut came, after
        /// half a range, and the writer was told.
        fn put(&mut self, ranges: &Ranges, body: &[u8]) -> bool {
            for range in ranges.as_slice() {
                let list;
                let from: &[u8] = match range.region {
                    Region::Buffer => &self.buffer,
                    Region::Scratch => &self.scratch,
                    Region::Body => body,
                    Region::List(partition) => {
                        list = entry_bytes(&self.lists[partition as usize].0);
                        &list
                    }
                };
                let mut bytes = &from[range.start..range.start + range.len];
                let cut = self.cut == Some(0);
                if cut {
                    bytes = &bytes[..bytes.len() / 2];
                }
                let at = range.at as usize;
                if self.file.len() < at + bytes.len() {
                    self.file.resize(at + bytes.len(), 0);
                }
                self.file[at..at + bytes.len()].copy_from_slice(bytes);
                if cut {
                    self.writer.failed();
                    return false;
                }
                self.cut = self.cut.map(|left| left - 1);
            }
            true
        }

        /// Give a step what it asks, write its ranges and report them:
        /// whether the call is done, or `None` when a write was cut.
        fn serve(&mut self, step: Step, partition: u32, body: &[u8]) -> Result<Option<bool>> {
            let (ranges, done) = match step {
                Step::Scratch(len) => {
                    ensure!(len > self.scratch.len(), "a scratch of {len} bytes again");
                    self.scratch.resize(len, 0);
                    return Ok(Some(false));
                }
                Step::List(len) => {
                    let entries = &mut self.lists[partition as usize].0;
                    ensure!(len > entries.len(), "a list of {len} entries again");
                    entries.resize(len, Entry::default());
                    return Ok(Some(false));
                }
                Step::Again(ranges) => (ranges, false),
                Step::Done { ranges, .. } => (ranges, true),
            };
            if !self.put(&ranges, body) {
                return Ok(None);
            }
            if !ranges.as_slice().is_empty() {
                self.writer.written()?;
            }
            Ok(Some(done))
        }

        /// Write a block: its place in the file, or `None` when a write
        /// was cut.
        pub(crate) fn write(&mut self, block: &Owned) -> Result<Option<Entry>> {
            loop {
                let step = self.writer.write(
                    &mut self.buffer,
                    &mut self.scratch,
                    &mut self.lists,
                    block.block(),
                )?;
                let place = match step {
                    Step::Done { offset, stored, .. } => Some(Entry { offset, stored }),
                    _ => None,
                };
                match self.serve(step, block.partition, &block.body)? {
                    None => return Ok(None),
                    Some(true) => return place.context("a write done without its place").map(Some),
                    Some(false) => {}
                }
            }
        }

        /// Finish the set: false when a write was cut.
        pub(crate) fn finish(&mut self) -> Result<bool> {
            loop {
                let step = self.writer.finish(&mut self.buffer, &mut self.lists)?;
                match self.serve(step, 0, &[])? {
                    None => return Ok(false),
                    Some(true) => return Ok(true),
                    Some(false) => {}
                }
            }
        }

        /// A call of the writer's `write` for `block`, as it comes.
        fn try_write(&mut self, block: &Owned) -> Result<Step, Error> {
            self.writer.write(
                &mut self.buffer,
                &mut self.scratch,
                &mut self.lists,
                block.block(),
            )
        }
    }

    /// The bytes a block's body takes on disk, packed as the writer packs
    /// it, and the length its header gives.
    pub(crate) fn stored_body(block: &Owned) -> Result<(Vec<u8>, u64)> {
        let len = block.body.len();
        match block.kind {
            BlockKind::Columns => {
                let words = columns::shape(&block.body)?.words;
                let mut out = vec![0; len + columns::pack_slack(words)];
                let (packed, unpacked) = columns::pack(&block.body, &mut out)?;
                out.truncate(packed);
                Ok((out, unpacked as u64))
            }
            BlockKind::Records if len >= 64 => {
                let mut out = vec![0; len];
                match crate::pack(&block.body, &mut out) {
                    Some(packed) => {
                        out.truncate(packed);
                        Ok((out, len as u64))
                    }
                    None => Ok((block.body.clone(), len as u64)),
                }
            }
            _ => Ok((block.body.clone(), len as u64)),
        }
    }

    /// Check the file of a set against the blocks written to it, in
    /// order: each partition's list names its blocks in their order, and
    /// each block reads back as written; a shared set ends with its lists
    /// and its trailer.
    fn check_file(host: &Host, config: Config, written: &[Owned]) -> Result<()> {
        let partitions = config.partitions as usize;
        let mut end = 0_u64;
        for partition in 0..partitions {
            let (entries, count) = &host.lists[partition];
            let ours: Vec<&Owned> = written
                .iter()
                .filter(|block| block.partition as usize == partition)
                .collect();
            ensure!(
                *count as usize == ours.len(),
                "partition {partition}: {count} entries for {} blocks",
                ours.len()
            );
            for (entry, block) in entries.iter().zip(&ours) {
                let at = entry.offset as usize;
                let bytes = host
                    .file
                    .get(at..at + entry.stored as usize)
                    .context("an entry past the file")?;
                let header = BlockHeader::read(bytes, FINGERPRINT, MAX_LEN)?;
                let (body, len) = stored_body(block)?;
                ensure!(
                    (header.kind, header.number, header.partition, header.level)
                        == (block.kind, block.number, block.partition, config.level),
                    "the header of {block:?}: {header:?}"
                );
                ensure!(header.len == len, "{header:?} for a body of {len}");
                ensure!(
                    (header.packed == 0 && body == block.body)
                        || header.packed as usize == body.len(),
                    "{header:?}: {} bytes stored",
                    body.len()
                );
                ensure!(bytes[HEADER_SIZE..] == body[..], "the body of {block:?}");
                end = end.max(entry.offset + entry.stored);
            }
        }
        let mut blocks = 0_u64;
        for block in written {
            blocks += (HEADER_SIZE + stored_body(block)?.0.len()) as u64;
        }
        ensure!(end == blocks, "blocks end at {end}, take {blocks} bytes");
        ensure!(host.writer.bytes() == blocks && host.writer.blocks() == written.len() as u64);
        if !config.shared || written.is_empty() {
            ensure!(
                host.file.len() as u64 == blocks,
                "a file of {}",
                host.file.len()
            );
            return Ok(());
        }
        let word = |at: usize| -> Result<u64> {
            let bytes = host.file.get(at..at + 8).context("a word past the file")?;
            Ok(u64::from_ne_bytes(bytes.try_into()?))
        };
        let trailer = host.file.len() - TRAILER_SIZE;
        ensure!(
            [
                word(trailer)?,
                word(trailer + 8)?,
                word(trailer + 16)?,
                word(trailer + 24)?
            ] == [TRAILER_MAGIC, blocks, config.partitions.into(), FINGERPRINT],
            "the trailer"
        );
        let mut at = blocks as usize + 8 * partitions;
        for partition in 0..partitions {
            let (entries, count) = &host.lists[partition];
            ensure!(word(blocks as usize + 8 * partition)? == *count);
            let len = *count as usize * ENTRY_SIZE;
            ensure!(host.file[at..at + len] == entry_bytes(&entries[..*count as usize])[..]);
            at += len;
        }
        ensure!(
            at == trailer,
            "the lists end at {at}, the trailer at {trailer}"
        );
        Ok(())
    }

    /// A chunk of `count` records of `len` bytes that packs: a used mark,
    /// each record's length, and words from `seed` elsewhere.
    pub(crate) fn records(count: usize, len: usize, seed: u32) -> Vec<u8> {
        let mut bytes = vec![0_u8; CHUNK_HEADER + count * len];
        let total = bytes.len() as u64;
        bytes[..CHUNK_HEADER].copy_from_slice(&total.to_ne_bytes());
        for record in 0..count {
            let base = CHUNK_HEADER + record * len;
            for lane in 0..len / 4 {
                let value = if lane * 4 == RECORD_SIZE_OFFSET {
                    (len / 8) as u32
                } else {
                    seed.wrapping_mul((record + lane) as u32) >> (lane % 24)
                };
                bytes[base + 4 * lane..base + 4 * lane + 4].copy_from_slice(&value.to_ne_bytes());
            }
        }
        bytes
    }

    /// A chunk of columns of `rows` rows of `words` words, its lanes
    /// filled from `seed`.
    pub(crate) fn columns_chunk(rows: usize, words: usize, seed: u64) -> Vec<u8> {
        let mut chunk = vec![0_u8; columns::size(rows.max(1), words)];
        let made = columns::init(&mut chunk, words).and_then(|_| columns::shape(&chunk));
        let Ok(shape) = made else {
            return chunk;
        };
        columns::set_rows(&mut chunk, rows);
        for lane in 1..columns::lanes(words) {
            for row in 0..rows {
                let at = shape.lane_at(lane) + 8 * row;
                let value = seed.wrapping_mul((lane * 1000 + row + 1) as u64) >> (lane % 48);
                chunk[at..at + 8].copy_from_slice(&value.to_ne_bytes());
            }
        }
        chunk
    }

    /// Blocks of each kind and length a set meets: records that pack and
    /// bytes that do not, values, and chunks of columns.
    pub(crate) fn blocks(partitions: u32) -> impl Strategy<Value = Vec<Owned>> {
        let block = (0..partitions, 0..4_u32, 0..400_usize, any::<u32>()).prop_map(
            |(partition, kind, size, seed)| {
                let (kind, body) = match kind {
                    0 => (
                        BlockKind::Records,
                        records(size / 8 + 1, 24 + 8 * (size % 4), seed),
                    ),
                    1 => (
                        BlockKind::Records,
                        (0..size / 8 * 8 + 8)
                            .map(|byte| (byte as u32 ^ seed) as u8)
                            .collect(),
                    ),
                    2 => (
                        BlockKind::Values,
                        (0..size / 8 * 8)
                            .map(|byte| (byte as u32).wrapping_mul(seed) as u8)
                            .collect(),
                    ),
                    _ => (
                        BlockKind::Columns,
                        columns_chunk(size % 300, 1 + size % 3, u64::from(seed)),
                    ),
                };
                Owned {
                    kind,
                    number: seed % 1000,
                    partition,
                    body,
                }
            },
        );
        prop::collection::vec(block, 0..40)
    }

    pub(crate) fn config(partitions: u32, buffer_len: usize, shared: bool) -> Config {
        Config {
            partitions,
            level: 2,
            fingerprint: FINGERPRINT,
            max_len: MAX_LEN,
            buffer_len,
            shared,
        }
    }

    /// Every block of every partition, written through a buffer of any
    /// length, lies in the file where its list says and reads back as it
    /// was written; a shared set ends with its lists and its trailer.
    #[test]
    fn a_set_written_in_memory_reads_back() {
        let case =
            (1..6_u32, 0..4_usize, any::<bool>()).prop_flat_map(|(partitions, buffer, shared)| {
                (Just((partitions, buffer, shared)), blocks(partitions))
            });
        property(case, |((partitions, buffer, shared), written)| {
            let buffer_len = [MIN_BUFFER, 256, 1024, 4096][buffer];
            let config = config(partitions, buffer_len, shared);
            let mut host = Host::new(config)?;
            for block in &written {
                let place = host.write(block)?.context("a write cut")?;
                ensure!(place.stored >= HEADER_SIZE as u64);
            }
            ensure!(host.finish()?, "a finish cut");
            ensure!(host.writer.is_finished());
            check_file(&host, config, &written)
        });
    }

    /// A write cut half way leaves a set that refuses every call but a
    /// reset, unchanged; once reset, it writes and finishes as a new one.
    #[test]
    fn a_set_cut_by_a_failed_write_takes_only_a_reset() {
        let case = (1..5_u32, 0..3_usize, any::<bool>(), 0..60_usize).prop_flat_map(
            |(partitions, buffer, shared, cut)| {
                (Just((partitions, buffer, shared, cut)), blocks(partitions))
            },
        );
        property(case, |((partitions, buffer, shared, cut), written)| {
            let buffer_len = [MIN_BUFFER, 512, 2048][buffer];
            let config = config(partitions, buffer_len, shared);
            let mut host = Host::new(config)?;
            host.cut = Some(cut);
            let mut whole = true;
            for block in &written {
                if host.write(block)?.is_none() {
                    whole = false;
                    break;
                }
            }
            if whole {
                whole = host.finish()?;
            }
            if whole {
                // The cut would have come after the last write.
                return check_file(&host, config, &written);
            }
            let failed = Err(Error::wrong_call(Rule::SetFailed));
            let block = written.first().context("a cut with no block")?;
            let before = host.writer;
            ensure!(host.try_write(block) == failed);
            ensure!(host.writer.finish(&mut host.buffer, &mut host.lists) == failed);
            ensure!(host.writer.written() == Err(Error::wrong_call(Rule::SetFailed)));
            ensure!(
                host.writer.drop_list(&mut host.lists, 0)
                    == Err(Error::wrong_call(Rule::SetFailed))
            );
            ensure!(host.writer == before, "a refused call changed the set");
            host.writer.reset(&mut host.lists)?;
            host.file.clear();
            host.cut = None;
            for block in &written {
                host.write(block)?.context("a write cut")?;
            }
            ensure!(host.finish()?);
            check_file(&host, config, &written)
        });
    }

    fn values(len: usize) -> Owned {
        Owned {
            kind: BlockKind::Values,
            number: 7,
            partition: 1,
            body: vec![5; len],
        }
    }

    /// A block longer than the buffer goes to the file on its own, after
    /// what the buffer held: its header from the buffer, its body as the
    /// caller gave it.
    #[test]
    fn a_block_longer_than_the_buffer_goes_on_its_own() -> Result<()> {
        let mut host = Host::new(config(2, MIN_BUFFER, false))?;
        host.write(&values(8))?;
        let long = values(4 * MIN_BUFFER);
        let first = HEADER_SIZE + 8;
        let flush = host.try_write(&long)?;
        ensure!(
            flush
                == Step::Again(Ranges::one(Range {
                    region: Region::Buffer,
                    start: 0,
                    len: first,
                    at: 0,
                })),
            "{flush:?}"
        );
        host.writer.written()?;
        let alone = host.try_write(&long)?;
        let Step::Done {
            ranges,
            offset,
            stored,
        } = alone
        else {
            anyhow::bail!("{alone:?}");
        };
        ensure!((offset, stored) == (first as u64, (HEADER_SIZE + long.body.len()) as u64));
        ensure!(
            ranges.as_slice()
                == [
                    Range {
                        region: Region::Buffer,
                        start: 0,
                        len: HEADER_SIZE,
                        at: first as u64,
                    },
                    Range {
                        region: Region::Body,
                        start: 0,
                        len: long.body.len(),
                        at: (first + HEADER_SIZE) as u64,
                    },
                ],
            "{ranges:?}"
        );
        Ok(())
    }

    /// A block of a partition whose list is full asks for a list twice as
    /// long, of 16 entries at least, and the set is as it was.
    #[test]
    fn a_full_block_list_asks_for_room() -> Result<()> {
        let mut host = Host::new(config(2, 256, false))?;
        let block = values(8);
        let before = (host.writer, host.buffer.clone());
        ensure!(host.try_write(&block)? == Step::List(MIN_LIST));
        ensure!((host.writer, host.buffer.clone()) == before);
        host.lists[1].0.resize(MIN_LIST, Entry::default());
        host.write(&block)?;
        host.write(&block)?;
        host.lists[1].0.truncate(2);
        ensure!(host.try_write(&block)? == Step::List(MIN_LIST));
        host.lists[1].0.resize(MIN_LIST, Entry::default());
        for _ in 2..MIN_LIST {
            host.write(&block)?;
        }
        ensure!(host.try_write(&block)? == Step::List(2 * MIN_LIST));
        Ok(())
    }

    /// A shared set drops no partition before it is finished; a serial
    /// set does, and a dropped partition no longer counts.
    #[test]
    fn a_shared_set_drops_no_partition_before_its_finish() -> Result<()> {
        for shared in [true, false] {
            let mut host = Host::new(config(2, 256, shared))?;
            host.write(&values(16))?;
            ensure!(host.writer.files() == 1);
            let before = host.writer;
            let dropped = host.writer.drop_list(&mut host.lists, 1);
            if shared {
                ensure!(dropped == Err(Error::wrong_call(Rule::SetDrop).with(1)));
                ensure!(host.writer == before && host.lists[1].1 == 1);
                ensure!(host.finish()?);
                host.writer.drop_list(&mut host.lists, 1)?;
            } else {
                dropped?;
            }
            ensure!(host.lists[1].1 == 0 && host.writer.files() == 0);
        }
        Ok(())
    }

    /// Each misuse of a writer is a wrong call that names its rule, and
    /// leaves the writer as it was.
    #[test]
    fn misuse_of_a_set_is_a_wrong_call() -> Result<()> {
        let refused = |result: Result<Step, Error>| {
            let error = result.expect_err("a misuse accepted");
            assert_eq!(error.kind(), Kind::WrongCall, "{error}");
            error.rule()
        };
        let made = |partitions, level, buffer_len| {
            Writer::new(Config {
                partitions,
                level,
                buffer_len,
                ..config(1, MIN_BUFFER, false)
            })
            .map_err(Error::rule)
        };
        assert_eq!(made(0, 0, MIN_BUFFER), Err(Rule::SetPartitions));
        assert_eq!(made(1, MAX_LEVEL, MIN_BUFFER), Err(Rule::Level));
        assert_eq!(made(1, 0, MIN_BUFFER - 8), Err(Rule::SetBuffer));
        assert_eq!(made(1, 0, MIN_BUFFER + 4), Err(Rule::SetBuffer));

        let mut host = Host::new(config(2, 256, true))?;
        host.lists[1].0.resize(MIN_LIST, Entry::default());
        let block = values(16);
        let before = host.writer;
        let mut short = vec![0; 128];
        let step = host.writer.write(
            &mut short,
            &mut host.scratch,
            &mut host.lists,
            block.block(),
        );
        assert_eq!(refused(step), Rule::SetBuffer);
        let past = Owned {
            partition: 2,
            ..block.clone()
        };
        assert_eq!(refused(host.try_write(&past)), Rule::SetPartition);
        host.lists[1].1 = MIN_LIST as u64 + 1;
        assert_eq!(refused(host.try_write(&block)), Rule::SetList);
        host.lists[1].1 = 0;
        let unknown = Owned {
            kind: BlockKind::Columns,
            ..block.clone()
        };
        assert_eq!(refused(host.try_write(&unknown)), Rule::ColumnsMagic);
        assert_eq!(
            host.writer.written(),
            Err(Error::wrong_call(Rule::SetWritten))
        );
        assert_eq!(host.writer, before);

        // A block that a full buffer leaves to the next call: another
        // block, or a finish, is refused until it is written.
        host.write(&values(16))?;
        let long = values(200);
        let step = host.try_write(&long)?;
        ensure!(matches!(step, Step::Again(_)), "{step:?}");
        assert_eq!(refused(host.try_write(&long)), Rule::SetOutstanding);
        host.writer.written()?;
        assert_eq!(refused(host.try_write(&block)), Rule::SetPending);
        // Even a block whose list is full: the block left comes first.
        let other = Owned {
            partition: 0,
            ..block.clone()
        };
        assert_eq!(refused(host.try_write(&other)), Rule::SetPending);
        assert_eq!(
            refused(host.writer.finish(&mut host.buffer, &mut host.lists)),
            Rule::FinishPending
        );
        // A block left to be placed from the body takes the body's
        // length: another length in the writer's words is refused.
        let changed = host.writer;
        host.writer.pending_stored += 8;
        assert_eq!(refused(host.try_write(&long)), Rule::SetState);
        host.writer = changed;
        // Nor more partitions with blocks than the set has.
        host.writer.files = 3;
        assert_eq!(refused(host.try_write(&long)), Rule::SetState);
        host.writer = changed;
        host.write(&long)?;
        ensure!(host.finish()?);
        assert_eq!(refused(host.try_write(&block)), Rule::SetFinished);

        // A finish checks every list before it takes any, so that one it
        // refuses half way changes nothing.
        let mut set = Host::new(config(3, MIN_BUFFER, true))?;
        for partition in 0..3 {
            set.write(&Owned {
                partition,
                ..values(8)
            })?;
        }
        while set.writer.finish != ENTRIES {
            let step = set.writer.finish(&mut set.buffer, &mut set.lists)?;
            ensure!(set.serve(step, 0, &[])? == Some(false));
        }
        set.lists[2].1 = 100;
        let before = set.writer;
        assert_eq!(
            refused(set.writer.finish(&mut set.buffer, &mut set.lists)),
            Rule::SetList
        );
        assert_eq!(set.writer, before);
        set.lists[2].1 = 1;
        // The offsets of a finish do not pass 2^64, its first step's
        // either.
        let mut first = Host::new(config(2, MIN_BUFFER, true))?;
        first.write(&values(8))?;
        first.writer.end = u64::MAX - 16;
        first.writer.bytes = 0;
        first.writer.blocks = 0;
        first.writer.buffered = 0;
        let unchanged = first.writer;
        assert_eq!(
            refused(first.writer.finish(&mut first.buffer, &mut first.lists)),
            Rule::SetState
        );
        assert_eq!(first.writer, unchanged);
        set.writer.next_at = u64::MAX - 8;
        assert_eq!(
            refused(set.writer.finish(&mut set.buffer, &mut set.lists)),
            Rule::SetState
        );
        set.writer = before;
        // Finished only once the trailer is reported written.
        loop {
            let step = set.writer.finish(&mut set.buffer, &mut set.lists)?;
            if matches!(step, Step::Done { .. }) {
                ensure!(!set.writer.is_finished(), "finished before the trailer");
                set.writer.written()?;
                break;
            }
            ensure!(set.serve(step, 0, &[])? == Some(false));
        }
        ensure!(set.writer.is_finished());

        let mut zeroed = Writer::default();
        assert_eq!(
            refused(zeroed.finish(&mut host.buffer, &mut host.lists)),
            Rule::SetState
        );
        assert_eq!(
            zeroed.reset(&mut host.lists),
            Err(Error::wrong_call(Rule::SetState))
        );
        Ok(())
    }
}

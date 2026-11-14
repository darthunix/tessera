//! The file of a set, read.
//!
//! [`Reader`] gives the blocks of one partition of a set back, in the
//! order they were written, and reads no byte itself. Its caller keeps
//! the file and two regions: a page of [`Entry`]s, and a block buffer as
//! long as the partition's largest block. A call returns a [`ReadStep`]:
//! the bytes of the file to read into the page or into the block buffer,
//! from their starts, after which the caller makes the call again; or
//! what the reader found.
//!
//! For a finished set's file, the caller's own or another participant's
//! of a shared set, the caller gives the file's length
//! ([`Reader::file`]), and [`Reader::open`] asks first for the trailer at
//! the end of the file, then for the counts of the lists, then for the
//! partition's entries, a page at a time. It checks each part before it
//! asks for the bytes it names: the trailer before the counts, the
//! counts, which must fill the lists exactly, before the entries, and
//! each entry, which must lie whole before the next block of its
//! partition and before the lists, before a block is read. A file that
//! breaks any of these is damaged bytes.
//!
//! [`Reader::next`] then reads each block whole, checks its header
//! against the set and against the bytes the list gives it, and hands the
//! header back; [`Reader::body`] gives the body, unpacked when it was
//! stored packed. [`Reader::seek`] goes to the block a write placed at a
//! given byte of the file.

use super::{ENTRY_SIZE, Entry, TRAILER_MAGIC, TRAILER_SIZE};
use crate::error::ensure;
use crate::{BlockHeader, BlockKind, Bounds, Error, HEADER_SIZE, MAX_LEVEL, Result, Rule, columns};

/// What a reader reads: a partition of a set at a level, of a table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadConfig {
    /// The partitions of the set.
    pub partitions: u32,
    /// The set's level, below [`MAX_LEVEL`].
    pub level: u32,
    /// The partition to read, below `partitions`.
    pub partition: u32,
    /// The table's layout fingerprint, which every header and a trailer
    /// carry.
    pub fingerprint: u64,
    /// The most body bytes a block of each kind takes: those the set's
    /// writer gives ([`super::Writer::read_bounds`]), or, for a shared
    /// set, the largest of its participants'.
    pub bounds: Bounds,
}

/// What a call of a [`Reader`] asks of its caller, or found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadStep {
    /// Read `len` bytes of the file at byte `at` into the page, from its
    /// start, and call again.
    Page {
        /// Where the bytes start in the file.
        at: u64,
        /// How many to read.
        len: usize,
    },
    /// Read `len` bytes of the file at byte `at` into the block buffer,
    /// from its start, and call again.
    Block {
        /// Where the bytes start in the file.
        at: u64,
        /// How many to read.
        len: usize,
    },
    /// The partition has no block in the file.
    Empty,
    /// The open is done; the block buffer must hold `largest` bytes.
    Ready {
        /// The bytes of the partition's largest block, header included.
        largest: u64,
    },
    /// The next block's header, checked; its body waits in the block
    /// buffer for [`Reader::body`].
    Header(BlockHeader),
    /// Every block of the partition was read.
    End,
    /// A seek found its block: the next read is that block's.
    Found,
}

// The phase of a reader. A zeroed reader, in phase 0, takes no call.
const TRAILER: u32 = 1;
const COUNTS: u32 = 2;
const ENTRIES: u32 = 3;
const READY: u32 = 4;
const BODY: u32 = 5;
const SEEK: u32 = 6;
const EMPTY: u32 = 7;

// What a step asked to read: nothing, a page or a block.
const NOTHING: u32 = 0;
const PAGE: u32 = 1;
const BLOCK: u32 = 2;

/// The most bytes a block takes past its body: its header, and the slack
/// of a packed chunk of columns, which may be longer than its body.
fn block_slack() -> u64 {
    (HEADER_SIZE + columns::pack_slack(columns::MAX_WORDS)) as u64
}

/// The reader of one partition of a set: plain numbers, which the caller
/// keeps where it likes, such as in memory it shares with C. Every call
/// checks them first, so that a reader the caller changed is a wrong
/// call, never a read out of place.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Reader {
    fingerprint: u64,
    bounds: Bounds,
    file_len: u64,
    /// Where the lists start, and where the partition's entries do.
    lists_at: u64,
    entries_at: u64,
    /// The partition's blocks, the next to read, and the largest.
    count: u64,
    next: u64,
    largest: u64,
    /// The entries the page holds: the first one's index, and how many.
    page_first: u64,
    page_count: u64,
    /// An open's progress: the entries the lists hold, the blocks of the
    /// partitions before this one and of all of them so far, the next
    /// count or entry to check, and the last entry checked, which waits
    /// for the next one's offset.
    most: u64,
    before: u64,
    total: u64,
    cursor: u64,
    held_offset: u64,
    held_stored: u64,
    /// The read the last step asked for.
    asked_at: u64,
    asked_len: u64,
    /// A block whose header was given: its body's length.
    body_len: u64,
    /// A seek's target and its bounds.
    seek_to: u64,
    seek_low: u64,
    seek_high: u64,
    partitions: u32,
    level: u32,
    partition: u32,
    phase: u32,
    /// Kept 0.
    spare: u32,
    asked: u32,
    body_kind: u32,
    body_packed: u32,
    /// The phase a seek started from, which a refused seek goes back to.
    seek_from: u32,
}

// Its size is part of its C mirror: words of 8 bytes.
const _: () = assert!(size_of::<Reader>() == 8 * READER_WORDS);

/// Words of 8 bytes a [`Reader`] takes.
pub const READER_WORDS: usize = 28;

fn check_config(config: &ReadConfig) -> Result<()> {
    ensure!(
        config.partitions > 0,
        Error::wrong_call(Rule::SetPartitions).with(config.partitions)
    );
    ensure!(
        config.partition < config.partitions,
        Error::wrong_call(Rule::SetPartition)
            .with(config.partition)
            .and(config.partitions)
    );
    ensure!(
        config.level < MAX_LEVEL,
        Error::wrong_call(Rule::Level).with(config.level)
    );
    Ok(())
}

impl Reader {
    fn made(config: ReadConfig, phase: u32) -> Self {
        Self {
            fingerprint: config.fingerprint,
            bounds: config.bounds,
            partitions: config.partitions,
            level: config.level,
            partition: config.partition,
            phase,
            ..Self::default()
        }
    }

    /// A reader of a partition of a finished set's file, the caller's
    /// own or another participant's, of `file_len` bytes; [`Self::open`]
    /// reads its lists.
    ///
    /// # Errors
    ///
    /// A wrong call for a config no set has.
    pub fn file(config: ReadConfig, file_len: u64) -> Result<Self> {
        check_config(&config)?;
        let mut reader = Self::made(config, TRAILER);
        reader.file_len = file_len;
        Ok(reader)
    }

    /// The partition's blocks.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The bytes of the partition's largest block, which the block buffer
    /// holds; known once the open is done.
    pub fn largest(&self) -> u64 {
        self.largest
    }

    /// The reader's numbers, checked: the phase is one it keeps, and an
    /// open's progress, the page and the next block agree.
    fn check_state(&self) -> Result<()> {
        let refuse = Error::wrong_call(Rule::ReaderState);
        ensure!(
            matches!(self.phase, TRAILER..=EMPTY)
                && self.partitions > 0
                && self.partition < self.partitions
                && self.level < MAX_LEVEL
                && self.spare == 0
                && self.asked <= BLOCK
                && self.next <= self.count
                && self.total <= self.most
                && self.before <= self.total
                && self.cursor <= self.count.max(u64::from(self.partitions))
                && self.page_count <= self.count
                && self.page_first <= self.count - self.page_count
                && self.seek_low <= self.seek_high
                && self.seek_high <= self.count
                && (self.phase != SEEK || matches!(self.seek_from, READY | BODY | EMPTY))
                && self.largest <= self.bounds.largest().saturating_add(block_slack()),
            refuse
        );
        // A block asked for lies whole before the lists.
        ensure!(
            self.asked != BLOCK
                || (self.asked_len >= HEADER_SIZE as u64
                    && self.asked_len <= self.largest
                    && self
                        .asked_at
                        .checked_add(self.asked_len)
                        .is_some_and(|end| end <= self.lists_at)),
            refuse
        );
        if self.phase == TRAILER {
            ensure!(
                self.asked == NOTHING
                    || (self.asked == PAGE
                        && self.file_len >= TRAILER_SIZE as u64
                        && self.asked_at == self.file_len - TRAILER_SIZE as u64
                        && self.asked_len == TRAILER_SIZE as u64),
                refuse
            );
            return Ok(());
        }
        // The file as its trailer lays it out: the blocks, the counts, the
        // entries the lists hold, the trailer.
        let partitions = u64::from(self.partitions);
        let end = self
            .most
            .checked_mul(ENTRY_SIZE as u64)
            .and_then(|entries| entries.checked_add(8 * partitions + TRAILER_SIZE as u64))
            .and_then(|lists| lists.checked_add(self.lists_at));
        ensure!(end == Some(self.file_len), refuse);
        if self.phase == COUNTS {
            ensure!(
                self.cursor <= partitions
                    && (self.asked == NOTHING
                        || (self.asked == PAGE
                            && self.asked_at == self.lists_at + 8 * self.cursor
                            && self.asked_len > 0
                            && self.asked_len.is_multiple_of(8)
                            && self.asked_len <= 8 * (partitions - self.cursor))),
                refuse
            );
            return Ok(());
        }
        if self.phase == EMPTY && self.count == 0 {
            return Ok(());
        }
        // The partition's entries lie inside the lists.
        let first = self.lists_at + 8 * partitions + ENTRY_SIZE as u64 * self.before;
        ensure!(
            self.before + self.count <= self.most
                && self.entries_at == first
                && (self.phase != ENTRIES || self.cursor <= self.count),
            refuse
        );
        if self.asked == PAGE {
            let index = self.asked_at.checked_sub(self.entries_at).unwrap_or(1);
            let entries = self.asked_len / ENTRY_SIZE as u64;
            ensure!(
                index.is_multiple_of(ENTRY_SIZE as u64)
                    && self.asked_len.is_multiple_of(ENTRY_SIZE as u64)
                    && entries > 0
                    && index / ENTRY_SIZE as u64 + entries <= self.count,
                refuse
            );
        }
        Ok(())
    }

    /// Ask for `len` bytes at `at` into `region`.
    fn ask(&mut self, region: u32, at: u64, len: u64) -> ReadStep {
        self.asked = region;
        self.asked_at = at;
        self.asked_len = len;
        if region == PAGE {
            ReadStep::Page {
                at,
                len: len as usize,
            }
        } else {
            ReadStep::Block {
                at,
                len: len as usize,
            }
        }
    }

    /// The bytes asked for were read into `region`, `held` long: they fit.
    fn took(&mut self, region: u32, held: usize) -> Result<()> {
        ensure!(
            self.asked == region && self.asked_len <= held as u64,
            Error::wrong_call(Rule::ReaderRead)
                .with(held)
                .and(self.asked_len)
        );
        self.asked = NOTHING;
        Ok(())
    }

    /// Check that `entry` is a block's length, and that it lies whole
    /// before `end`, the next block of its partition or the lists, which
    /// lie before the trailer; the largest block grows to it.
    fn check_entry(&mut self, entry: &Entry, end: u64) -> Result<()> {
        let slack = block_slack();
        // Once the open gave the largest block, no entry passes it.
        let most = if self.phase >= READY {
            self.largest
        } else {
            self.bounds.largest().saturating_add(slack)
        };
        ensure!(
            entry.stored >= HEADER_SIZE as u64 && entry.stored <= most,
            Error::damaged(Rule::EntryLength).with(entry.stored)
        );
        ensure!(
            end <= self.lists_at && entry.offset <= end && entry.stored <= end - entry.offset,
            Error::damaged(Rule::EntryPlace)
                .with(entry.offset)
                .and(entry.stored)
        );
        self.largest = self.largest.max(entry.stored);
        Ok(())
    }

    /// A step of opening another participant's file: the bytes the last
    /// step asked for are at the start of `page`, which holds two entries
    /// at least. Its last step is [`ReadStep::Empty`] or
    /// [`ReadStep::Ready`].
    ///
    /// # Errors
    ///
    /// A wrong call: a reader not opening, a page too short, or one that
    /// does not hold the bytes asked for. Damaged bytes: a file shorter
    /// than a trailer; a trailer without its magic, of another count of
    /// partitions, of another table, or whose lists do not lie in the
    /// file; counts that do not fill the lists exactly; an entry of a
    /// length no block takes, or one that does not lie whole before the
    /// next block of its partition and the lists.
    pub fn open(&mut self, page: &[Entry]) -> Result<ReadStep> {
        self.check_state()?;
        ensure!(
            matches!(self.phase, TRAILER..=ENTRIES),
            Error::wrong_call(Rule::ReaderState)
        );
        ensure!(
            page.len() >= 2,
            Error::wrong_call(Rule::ReaderPage).with(page.len())
        );
        if self.phase == TRAILER {
            if self.asked == NOTHING {
                ensure!(
                    self.file_len >= TRAILER_SIZE as u64,
                    Error::damaged(Rule::Trailer).with(self.file_len)
                );
                let at = self.file_len - TRAILER_SIZE as u64;
                return Ok(self.ask(PAGE, at, TRAILER_SIZE as u64));
            }
            self.took(PAGE, page.len() * ENTRY_SIZE)?;
            self.read_trailer(page[0], page[1])?;
        }
        if self.phase == COUNTS {
            if self.asked == PAGE {
                self.took(PAGE, page.len() * ENTRY_SIZE)?;
                self.read_counts(page)?;
            }
            let partitions = u64::from(self.partitions);
            if self.cursor < partitions {
                let take = (2 * page.len() as u64).min(partitions - self.cursor);
                let at = self.lists_at + 8 * self.cursor;
                return Ok(self.ask(PAGE, at, 8 * take));
            }
            ensure!(
                self.total == self.most,
                Error::damaged(Rule::ListCounts)
                    .with(self.total)
                    .and(self.most)
            );
            if self.count == 0 {
                self.phase = EMPTY;
                return Ok(ReadStep::Empty);
            }
            self.entries_at = self.lists_at + 8 * partitions + ENTRY_SIZE as u64 * self.before;
            self.cursor = 0;
            self.phase = ENTRIES;
        }
        if self.asked == PAGE {
            self.took(PAGE, page.len() * ENTRY_SIZE)?;
            let taken = (self.asked_len / ENTRY_SIZE as u64) as usize;
            for entry in &page[..taken] {
                if self.cursor > 0 {
                    let held = Entry {
                        offset: self.held_offset,
                        stored: self.held_stored,
                    };
                    self.check_entry(&held, entry.offset)?;
                }
                self.held_offset = entry.offset;
                self.held_stored = entry.stored;
                self.cursor += 1;
            }
            self.page_first = self.cursor - taken as u64;
            self.page_count = taken as u64;
        }
        if self.cursor < self.count {
            let take = (page.len() as u64).min(self.count - self.cursor);
            let at = self.entries_at + ENTRY_SIZE as u64 * self.cursor;
            return Ok(self.ask(PAGE, at, ENTRY_SIZE as u64 * take));
        }
        let held = Entry {
            offset: self.held_offset,
            stored: self.held_stored,
        };
        self.check_entry(&held, self.lists_at)?;
        self.cursor = 0;
        self.phase = READY;
        Ok(ReadStep::Ready {
            largest: self.largest,
        })
    }

    /// Check a trailer, its words the two entries `first` and `second`.
    fn read_trailer(&mut self, first: Entry, second: Entry) -> Result<()> {
        let [magic, lists_at] = [first.offset, first.stored];
        let [partitions, fingerprint] = [second.offset, second.stored];
        ensure!(magic == TRAILER_MAGIC, Error::damaged(Rule::TrailerMagic));
        ensure!(
            partitions == u64::from(self.partitions),
            Error::damaged(Rule::TrailerPartitions)
                .with(partitions)
                .and(self.partitions)
        );
        let blocks_end = self.file_len - TRAILER_SIZE as u64;
        ensure!(
            lists_at <= blocks_end,
            Error::damaged(Rule::TrailerLists)
                .with(lists_at)
                .and(blocks_end)
        );
        ensure!(
            fingerprint == self.fingerprint,
            Error::damaged(Rule::FileFingerprint)
        );
        let lists = blocks_end - lists_at;
        let counts = 8 * partitions;
        ensure!(
            lists >= counts && (lists - counts).is_multiple_of(ENTRY_SIZE as u64),
            Error::damaged(Rule::ListsRoom).with(lists).and(partitions)
        );
        self.lists_at = lists_at;
        self.most = (lists - counts) / ENTRY_SIZE as u64;
        self.total = 0;
        self.before = 0;
        self.count = 0;
        self.cursor = 0;
        self.phase = COUNTS;
        Ok(())
    }

    /// Take the counts the last step asked for, from the words of `page`.
    fn read_counts(&mut self, page: &[Entry]) -> Result<()> {
        let taken = self.asked_len / 8;
        for index in 0..taken as usize {
            let entry = page[index / 2];
            let blocks = if index % 2 == 0 {
                entry.offset
            } else {
                entry.stored
            };
            ensure!(
                blocks <= self.most - self.total,
                Error::damaged(Rule::ListCounts)
                    .with(self.total.saturating_add(blocks))
                    .and(self.most)
            );
            let partition = self.cursor + index as u64;
            if partition < u64::from(self.partition) {
                self.before += blocks;
            } else if partition == u64::from(self.partition) {
                self.count = blocks;
            }
            self.total += blocks;
        }
        self.cursor += taken;
        Ok(())
    }

    /// The entry of block `index`, when the page holds it.
    fn entry(&self, page: &[Entry], index: u64) -> Option<Entry> {
        let first = self.page_first;
        (first <= index && index < first + self.page_count)
            .then(|| page.get((index - first) as usize).copied())
            .flatten()
    }

    /// Take a page of entries the last step asked for: they are checked
    /// again, as the file was read again.
    fn took_page(&mut self, page: &[Entry]) -> Result<()> {
        self.took(PAGE, page.len() * ENTRY_SIZE)?;
        let taken = (self.asked_len / ENTRY_SIZE as u64) as usize;
        let first = (self.asked_at - self.entries_at) / ENTRY_SIZE as u64;
        for index in 0..taken {
            let end = if index + 1 < taken {
                page[index + 1].offset
            } else if first + taken as u64 == self.count {
                self.lists_at
            } else {
                self.lists_at
                    .min(page[index].offset.saturating_add(self.largest))
            };
            self.check_entry(&page[index], end)?;
        }
        self.page_first = first;
        self.page_count = taken as u64;
        Ok(())
    }

    /// Ask for the page that holds block `index`: it starts there.
    fn ask_page(&mut self, page: &[Entry], index: u64) -> ReadStep {
        let take = (page.len() as u64).min(self.count - index);
        self.page_count = 0;
        let at = self.entries_at + ENTRY_SIZE as u64 * index;
        self.ask(PAGE, at, ENTRY_SIZE as u64 * take)
    }

    /// A step of reading the next block: `page` is the page and `block`
    /// the block buffer, each
    /// holding what the last step asked for. Its last step is
    /// [`ReadStep::Header`], or [`ReadStep::End`] past the last block.
    ///
    /// # Errors
    ///
    /// A wrong call: a reader not open, a body not taken, or regions that
    /// do not hold what was asked. Damaged bytes: an entry read again
    /// that no longer holds; a block whose header fails its checks, is of
    /// another partition or level, takes other bytes on disk than its
    /// entry, or whose packed body's counts give another length.
    pub fn next(&mut self, page: &[Entry], block: &[u8]) -> Result<ReadStep> {
        self.check_state()?;
        ensure!(self.phase != BODY, Error::wrong_call(Rule::BodyNotRead));
        ensure!(
            matches!(self.phase, READY | EMPTY),
            Error::wrong_call(Rule::ReaderState)
        );
        ensure!(
            !page.is_empty(),
            Error::wrong_call(Rule::ReaderPage).with(page.len())
        );
        if self.asked == BLOCK {
            self.took(BLOCK, block.len())?;
            return self.read_block(&block[..self.asked_len as usize]);
        }
        if self.asked == PAGE {
            self.took_page(page)?;
        }
        if self.next == self.count {
            return Ok(ReadStep::End);
        }
        let Some(entry) = self.entry(page, self.next) else {
            return Ok(self.ask_page(page, self.next));
        };
        ensure!(
            entry.stored <= self.largest,
            Error::damaged(Rule::EntryLength).with(entry.stored)
        );
        Ok(self.ask(BLOCK, entry.offset, entry.stored))
    }

    /// Check the block read whole into `bytes`, and give its header.
    fn read_block(&mut self, bytes: &[u8]) -> Result<ReadStep> {
        let header = BlockHeader::read(bytes, self.fingerprint, self.bounds)?;
        ensure!(
            header.partition == self.partition,
            Error::damaged(Rule::BlockPartition)
                .with(header.partition)
                .and(self.partition)
        );
        ensure!(
            header.level == self.level,
            Error::damaged(Rule::BlockLevel)
                .with(header.level)
                .and(self.level)
        );
        let stored = if header.packed > 0 {
            u64::from(header.packed)
        } else {
            header.len
        };
        ensure!(
            stored.checked_add(HEADER_SIZE as u64) == Some(bytes.len() as u64),
            Error::damaged(Rule::BlockStored)
                .with(bytes.len())
                .and(stored)
        );
        // A packed body begins with the counts of what it unpacks into:
        // the header's length must be the length they give, before the
        // caller allocates by it.
        if header.packed > 0 {
            let word = |at: usize| {
                let mut word = [0; 4];
                word.copy_from_slice(&bytes[HEADER_SIZE + at..HEADER_SIZE + at + 4]);
                u64::from(u32::from_ne_bytes(word))
            };
            let (count, each) = (word(0), word(4));
            let agrees = if header.kind == BlockKind::Columns {
                // A row takes 8 bytes in every lane; divided, nothing
                // overflows.
                let row = 8 * (columns::null_lanes(each as usize) as u64 + each);
                let lanes = header.len - columns::HEADER as u64;
                lanes.is_multiple_of(row) && lanes / row == count
            } else {
                header.len == 8 + count * each
            };
            ensure!(
                agrees,
                Error::damaged(Rule::PackedHeader)
                    .with(header.len)
                    .and(count)
            );
        }
        self.next += 1;
        self.phase = BODY;
        self.body_kind = header.kind as u32;
        self.body_len = header.len;
        self.body_packed = header.packed;
        Ok(ReadStep::Header(header))
    }

    /// Give the body of the block whose header the last step gave, from
    /// `block`, into `out`, of the length its header gives: unpacked when
    /// it was stored packed, copied otherwise.
    ///
    /// # Errors
    ///
    /// A wrong call when no header waits for its body, or `out` is of
    /// another length; damaged bytes when a packed body does not unpack.
    pub fn body(&mut self, block: &[u8], out: &mut [u8]) -> Result<()> {
        self.check_state()?;
        ensure!(self.phase == BODY, Error::wrong_call(Rule::BodyRead));
        ensure!(
            out.len() as u64 == self.body_len,
            Error::wrong_call(Rule::BodyLength)
                .with(out.len())
                .and(self.body_len)
        );
        let stored = if self.body_packed > 0 {
            u64::from(self.body_packed)
        } else {
            self.body_len
        };
        let body = block
            .get(HEADER_SIZE..HEADER_SIZE + stored as usize)
            .ok_or(Error::wrong_call(Rule::ReaderRead).with(block.len()))?;
        if self.body_kind == BlockKind::Columns as u32 {
            columns::unpack(body, out)?;
        } else if self.body_packed > 0 {
            crate::unpack(body, out)?;
        } else {
            out.copy_from_slice(body);
        }
        self.phase = READY;
        Ok(())
    }

    /// A step of going to the block a write placed at byte `offset` of the
    /// file: [`ReadStep::Found`] when the next read is that block's, or a
    /// page to read first. A body not taken is dropped.
    ///
    /// # Errors
    ///
    /// A wrong call when the reader is not open, or no block of the
    /// partition starts at `offset`.
    pub fn seek(&mut self, page: &[Entry], offset: u64) -> Result<ReadStep> {
        self.check_state()?;
        ensure!(
            matches!(self.phase, READY | BODY | SEEK | EMPTY),
            Error::wrong_call(Rule::ReaderState)
        );
        let position = Error::wrong_call(Rule::SeekPosition)
            .with(self.partition)
            .and(offset);
        if self.phase == SEEK && self.seek_to != offset {
            // Another seek before this one found its block.
            self.phase = self.seek_from;
            return Err(Error::wrong_call(Rule::ReaderState));
        }
        // A page holds an entry.
        ensure!(
            !page.is_empty(),
            Error::wrong_call(Rule::ReaderPage).with(page.len())
        );
        if self.phase != SEEK {
            ensure!(self.asked == NOTHING, Error::wrong_call(Rule::ReaderState));
            self.seek_from = self.phase;
            self.seek_to = offset;
            self.seek_low = 0;
            self.seek_high = self.count;
            self.phase = SEEK;
        }
        if self.asked == PAGE {
            self.took_page(page)?;
        }
        // The blocks of a partition lie in the order of their offsets.
        while self.seek_low < self.seek_high {
            let middle = self.seek_low + (self.seek_high - self.seek_low) / 2;
            let Some(entry) = self.entry(page, middle) else {
                return Ok(self.ask_page(page, middle));
            };
            if entry.offset < self.seek_to {
                self.seek_low = middle + 1;
            } else {
                self.seek_high = middle;
            }
        }
        let found = self.seek_low;
        let entry = if found < self.count {
            match self.entry(page, found) {
                Some(entry) => Some(entry),
                None => return Ok(self.ask_page(page, found)),
            }
        } else {
            None
        };
        if !entry.is_some_and(|entry| entry.offset == self.seek_to) {
            // A refused seek leaves the reader where it was, a body
            // waiting included.
            self.phase = self.seek_from;
            return Err(position);
        }
        self.phase = READY;
        self.next = found;
        Ok(ReadStep::Found)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::{Context, Result, bail, ensure};
    use proptest::prelude::*;

    use super::super::tests::{
        FINGERPRINT, Host, MAX_LEN, Owned, blocks, config, entry_bytes, property, records,
    };
    use super::*;
    use crate::Kind;
    use crate::set::MIN_BUFFER;

    fn read_config(partitions: u32, partition: u32) -> ReadConfig {
        ReadConfig {
            partitions,
            level: 2,
            partition,
            fingerprint: FINGERPRINT,
            bounds: Bounds::all(MAX_LEN),
        }
    }

    /// The entries of `bytes` read into a page, as a host reads a file.
    fn fill(page: &mut [Entry], bytes: &[u8]) {
        for (entry, pair) in page.iter_mut().zip(bytes.as_chunks::<16>().0) {
            let word = |at: usize| {
                let mut word = [0; 8];
                word.copy_from_slice(&pair[at..at + 8]);
                u64::from_ne_bytes(word)
            };
            *entry = Entry {
                offset: word(0),
                stored: word(8),
            };
        }
        // A tail of a single word, a count, goes into the next offset.
        let whole = bytes.len() / 16;
        if bytes.len() % 16 == 8 {
            let mut word = [0; 8];
            word.copy_from_slice(&bytes[16 * whole..]);
            page[whole].offset = u64::from_ne_bytes(word);
        }
    }

    /// A host that reads one partition of a file back.
    struct Back<'a> {
        reader: Reader,
        file: &'a [u8],
        page: Vec<Entry>,
        block: Vec<u8>,
    }

    impl Back<'_> {
        /// Serve a read step: false when it is none.
        fn serve(&mut self, step: ReadStep) -> Result<bool> {
            match step {
                ReadStep::Page { at, len } => {
                    let bytes = self
                        .file
                        .get(at as usize..at as usize + len)
                        .context("a page past the file")?;
                    fill(&mut self.page, bytes);
                    Ok(true)
                }
                ReadStep::Block { at, len } => {
                    let bytes = self
                        .file
                        .get(at as usize..at as usize + len)
                        .context("a block past the file")?;
                    self.block[..len].copy_from_slice(bytes);
                    Ok(true)
                }
                _ => Ok(false),
            }
        }

        /// Open another participant's file: its block buffer, or `None`
        /// for a partition without blocks.
        fn open(&mut self) -> Result<Option<()>, Error> {
            loop {
                let step = self.reader.open(&self.page)?;
                match step {
                    ReadStep::Empty => return Ok(None),
                    ReadStep::Ready { largest } => {
                        self.block = vec![0; largest as usize];
                        return Ok(Some(()));
                    }
                    _ => {
                        self.serve(step).expect("a read in the file");
                    }
                }
            }
        }

        /// The next block's header and body, or `None` at the end.
        fn next(&mut self) -> Result<Option<(BlockHeader, Vec<u8>)>, Error> {
            loop {
                let step = self.reader.next(&self.page, &self.block)?;
                match step {
                    ReadStep::End => return Ok(None),
                    ReadStep::Header(header) => {
                        let mut body = vec![0; header.len as usize];
                        self.reader.body(&self.block, &mut body)?;
                        return Ok(Some((header, body)));
                    }
                    _ => {
                        self.serve(step).expect("a read in the file");
                    }
                }
            }
        }

        /// The steps of the next block served until its header: the
        /// reader then waits for the body to be taken.
        fn header(&mut self) -> Result<()> {
            loop {
                let step = self.reader.next(&self.page, &self.block)?;
                if matches!(step, ReadStep::Header(_)) {
                    return Ok(());
                }
                ensure!(self.serve(step)?, "{step:?} before a header");
            }
        }

        fn seek(&mut self, offset: u64) -> Result<(), Error> {
            loop {
                let step = self.reader.seek(&self.page, offset)?;
                if step == ReadStep::Found {
                    return Ok(());
                }
                self.serve(step).expect("a read in the file");
            }
        }
    }

    /// The body a block reads back as: a chunk of records loses its links
    /// when packed, a chunk of columns keeps the lanes of its rows.
    fn read_back(block: &Owned) -> Result<Vec<u8>> {
        let (stored, len) = super::super::tests::stored_body(block)?;
        let mut out = vec![0; len as usize];
        match block.kind {
            BlockKind::Columns => columns::unpack(&stored, &mut out)?,
            _ if stored.len() < len as usize => crate::unpack(&stored, &mut out)?,
            _ => out.copy_from_slice(&stored),
        }
        Ok(out)
    }

    /// Read every block of `partition` back and compare it with what was
    /// written there; seek to each and read it again.
    fn read_partition(back: &mut Back<'_>, written: &[&Owned], places: &[u64]) -> Result<()> {
        for block in written {
            let (header, body) = back.next()?.context("a block missing")?;
            ensure!(
                (header.kind, header.number, header.partition)
                    == (block.kind, block.number, block.partition)
            );
            ensure!(body == read_back(block)?, "the body of {block:?}");
        }
        ensure!(back.next()?.is_none(), "a block past the last");
        for (index, place) in places.iter().enumerate().rev() {
            back.seek(*place)?;
            let (header, _) = back.next()?.context("no block where it was")?;
            ensure!(header.number == written[index].number);
        }
        Ok(())
    }

    /// Every partition of a set reads back from its file in the order its
    /// blocks were written, a page of 2 to 9 entries at a time; a seek
    /// goes to each block by its place.
    #[test]
    fn a_set_reads_back_by_its_lists_and_from_its_file() {
        // Up to 12 partitions, so that a page of two entries reads the
        // counts in steps.
        let case = (1..13_u32, 0..3_usize, any::<bool>(), 2..10_usize).prop_flat_map(
            |(partitions, buffer, shared, page)| {
                (Just((partitions, buffer, shared, page)), blocks(partitions))
            },
        );
        property(case, |((partitions, buffer, shared, page), written)| {
            let mut host = Host::new(config(partitions, [MIN_BUFFER, 512, 4096][buffer], shared))?;
            let mut places = vec![Vec::new(); partitions as usize];
            for block in &written {
                let place = host.write(block)?.context("a write cut")?;
                places[block.partition as usize].push(place.offset);
            }
            ensure!(host.finish()?);
            // Read with the bounds the writer gives, as a node reads.
            let bounds = host.writer.read_bounds();
            let config = |partition| ReadConfig {
                bounds,
                ..read_config(partitions, partition)
            };
            for partition in 0..partitions {
                let ours: Vec<&Owned> = written
                    .iter()
                    .filter(|block| block.partition == partition)
                    .collect();
                if written.is_empty() {
                    continue;
                }
                let mut other = Back {
                    reader: Reader::file(config(partition), host.file.len() as u64)?,
                    file: &host.file,
                    page: vec![Entry::default(); page],
                    block: Vec::new(),
                };
                match other.open()? {
                    None => ensure!(ours.is_empty(), "a partition with blocks opens empty"),
                    Some(()) => read_partition(&mut other, &ours, &places[partition as usize])?,
                }
            }
            Ok(())
        });
    }

    /// A shared set of three partitions, its file and its lists.
    fn shared_file() -> Result<(Host, Vec<u8>)> {
        let mut host = Host::new(config(3, 512, true))?;
        for (partition, len) in [(0, 16), (1, 24), (0, 40), (2, 8), (1, 8)] {
            host.write(&Owned {
                kind: BlockKind::Values,
                number: len,
                partition,
                body: vec![len as u8; len as usize],
            })?;
        }
        // A chunk of records that packs, last in partition 2.
        host.write(&Owned {
            kind: BlockKind::Records,
            number: 9,
            partition: 2,
            body: records(8, 40, 7),
        })?;
        ensure!(host.finish()?);
        let file = host.file.clone();
        Ok((host, file))
    }

    /// A block past the bound of its kind is damaged bytes, though its
    /// entry is within the larger bound: values past the longest the
    /// writers gave, a chunk past its plan.
    #[test]
    fn a_block_past_the_bound_of_its_kind_is_damaged() -> Result<()> {
        let (host, file) = shared_file()?;
        ensure!(
            host.writer.read_bounds()
                == Bounds {
                    chunk: MAX_LEN,
                    values: 40
                }
        );
        let read = |partition, bounds| -> Result<Vec<u32>, Error> {
            let mut back = Back {
                reader: Reader::file(
                    ReadConfig {
                        bounds,
                        ..read_config(3, partition)
                    },
                    file.len() as u64,
                )?,
                file: &file,
                page: vec![Entry::default(); 2],
                block: Vec::new(),
            };
            back.open()?;
            let mut numbers = Vec::new();
            while let Some((header, _)) = back.next()? {
                numbers.push(header.number);
            }
            Ok(numbers)
        };
        let short_values = Bounds {
            chunk: MAX_LEN,
            values: 32,
        };
        let short_chunk = Bounds {
            chunk: 8,
            values: MAX_LEN,
        };
        let past = |len| Err(Error::damaged(Rule::BlockLength).with(32).and(len));
        ensure!(read(0, short_values) == past(40));
        ensure!(read(1, short_values) == Ok(vec![24, 8]));
        let chunk = read(2, short_chunk).map_err(Error::rule);
        ensure!(chunk == Err(Rule::BlockLength), "{chunk:?}");
        Ok(())
    }

    /// The rule a damaged file breaks when partition `partition` of it is
    /// opened and read to its end.
    fn damage(file: &[u8], partition: u32) -> Rule {
        let mut back = Back {
            reader: Reader::file(read_config(3, partition), file.len() as u64).expect("made"),
            file,
            page: vec![Entry::default(); 2],
            block: Vec::new(),
        };
        let error = match back.open() {
            Err(error) => error,
            Ok(_) => loop {
                match back.next() {
                    Err(error) => break error,
                    Ok(None) => panic!("the damage was read"),
                    Ok(Some(_)) => {}
                }
            },
        };
        assert_eq!(error.kind(), Kind::Damaged, "{error}");
        error.rule()
    }

    fn put(file: &mut [u8], at: usize, word: u64) {
        file[at..at + 8].copy_from_slice(&word.to_ne_bytes());
    }

    /// Each part of a file a reader reads is checked before the bytes it
    /// names are asked for, and each damage is refused by its rule: the
    /// trailer, the counts, the entries, and the blocks they name.
    #[test]
    fn every_damaged_part_of_a_file_is_refused() -> Result<()> {
        let (host, file) = shared_file()?;
        let len = file.len();
        let trailer = len - TRAILER_SIZE;
        let lists = host.writer.bytes() as usize;
        let entries = lists + 8 * 3;
        let changed = |at: usize, word: u64| {
            let mut file = file.clone();
            put(&mut file, at, word);
            file
        };
        assert_eq!(damage(&file[..TRAILER_SIZE - 8], 0), Rule::Trailer);
        assert_eq!(damage(&changed(trailer, 7), 0), Rule::TrailerMagic);
        assert_eq!(
            damage(&changed(trailer + 16, 4), 0),
            Rule::TrailerPartitions
        );
        assert_eq!(
            damage(&changed(trailer + 8, len as u64), 0),
            Rule::TrailerLists
        );
        assert_eq!(damage(&changed(trailer + 24, 7), 0), Rule::FileFingerprint);
        assert_eq!(
            damage(&changed(trailer + 8, lists as u64 + 8), 0),
            Rule::ListsRoom
        );
        // A count of more blocks than the lists hold, in the partition read
        // and in another.
        assert_eq!(damage(&changed(lists, 3), 0), Rule::ListCounts);
        assert_eq!(damage(&changed(lists + 16, 5), 0), Rule::ListCounts);
        // And of fewer blocks than they hold.
        assert_eq!(damage(&changed(lists + 16, 1), 0), Rule::ListCounts);
        // Partition 0's entries: its first block runs into its second,
        // its last into the lists; one shorter than a header, one longer
        // than any block.
        assert_eq!(damage(&changed(entries + 8, 1000), 0), Rule::EntryPlace);
        assert_eq!(
            damage(&changed(entries + 16, lists as u64), 0),
            Rule::EntryPlace
        );
        assert_eq!(damage(&changed(entries + 8, 8), 0), Rule::EntryLength);
        assert_eq!(
            damage(&changed(entries + 24, MAX_LEN << 2), 0),
            Rule::EntryLength
        );
        // A block of another partition, or level, or another length on
        // disk than its header gives.
        let first = &host.lists[0].0[0];
        let at = first.offset as usize;
        let mut other = file.clone();
        other[at + 20..at + 24].copy_from_slice(&1_u32.to_ne_bytes());
        assert_eq!(damage(&other, 0), Rule::BlockPartition);
        let mut other = file.clone();
        other[at + 24..at + 28].copy_from_slice(&3_u32.to_ne_bytes());
        assert_eq!(damage(&other, 0), Rule::BlockLevel);
        let mut other = file.clone();
        other[at + 40..at + 48].copy_from_slice(&8_u64.to_ne_bytes());
        assert_eq!(damage(&other, 0), Rule::BlockStored);
        // A packed chunk whose header gives another length than its counts.
        let packed = &host.lists[2].0[1];
        let at = packed.offset as usize;
        let mut other = file.clone();
        let body = u64::from_ne_bytes(other[at + 40..at + 48].try_into()?);
        other[at + 40..at + 48].copy_from_slice(&(body + 8).to_ne_bytes());
        assert_eq!(damage(&other, 2), Rule::PackedHeader);
        // The file read whole is not damaged.
        let mut back = Back {
            reader: Reader::file(read_config(3, 1), len as u64)?,
            file: &file,
            page: vec![Entry::default(); 2],
            block: Vec::new(),
        };
        ensure!(back.open()? == Some(()));
        ensure!(back.next()?.is_some() && back.next()?.is_some() && back.next()?.is_none());
        Ok(())
    }

    /// A count that does not hold is refused before an entry is asked
    /// for, and a damaged trailer before a count is.
    #[test]
    fn a_part_is_checked_before_what_it_names_is_read() -> Result<()> {
        let (host, mut file) = shared_file()?;
        let lists = host.writer.bytes();
        put(&mut file, lists as usize, 9);
        let mut reader = Reader::file(read_config(3, 1), file.len() as u64)?;
        let mut page = vec![Entry::default(); 4];
        let mut asked = Vec::new();
        let error = loop {
            match reader.open(&page) {
                Err(error) => break error,
                Ok(ReadStep::Page { at, len }) => {
                    asked.push(at);
                    fill(&mut page, &file[at as usize..at as usize + len]);
                }
                Ok(step) => bail!("{step:?}"),
            }
        };
        ensure!(error.rule() == Rule::ListCounts, "{error}");
        // The trailer, then the counts: no entry between them and the
        // trailer.
        let trailer = (file.len() - TRAILER_SIZE) as u64;
        ensure!(
            asked.iter().all(|&at| at < lists + 8 * 3 || at == trailer),
            "an entry asked for: {asked:?}"
        );
        ensure!(asked.first() == Some(&trailer), "{asked:?}");
        Ok(())
    }

    /// A file laid out as the spec draws it, by hand: two blocks of
    /// partition 1, the counts, the entries and the trailer, read back.
    #[test]
    fn a_file_laid_out_by_hand_reads_back() -> Result<()> {
        let mut file = Vec::new();
        let mut entries = Vec::new();
        for (number, body) in [(4_u32, vec![1_u8; 16]), (5, vec![2; 8])] {
            let header = BlockHeader {
                kind: BlockKind::Values,
                number,
                partition: 1,
                level: 2,
                fingerprint: FINGERPRINT,
                len: body.len() as u64,
                packed: 0,
            };
            let mut bytes = [0; HEADER_SIZE];
            header.write(&mut bytes, Bounds::all(MAX_LEN))?;
            entries.push(Entry {
                offset: file.len() as u64,
                stored: (HEADER_SIZE + body.len()) as u64,
            });
            file.extend_from_slice(&bytes);
            file.extend_from_slice(&body);
        }
        let lists = file.len() as u64;
        for count in [0_u64, 2] {
            file.extend_from_slice(&count.to_ne_bytes());
        }
        file.extend_from_slice(&entry_bytes(&entries));
        for word in [TRAILER_MAGIC, lists, 2, FINGERPRINT] {
            file.extend_from_slice(&word.to_ne_bytes());
        }
        let mut back = Back {
            reader: Reader::file(read_config(2, 1), file.len() as u64)?,
            file: &file,
            page: vec![Entry::default(); 2],
            block: Vec::new(),
        };
        ensure!(back.open()? == Some(()));
        let (first, body) = back.next()?.context("the first block")?;
        ensure!(first.number == 4 && body == vec![1; 16]);
        let (second, body) = back.next()?.context("the second block")?;
        ensure!(second.number == 5 && body == vec![2; 8]);
        ensure!(back.next()?.is_none());
        let mut empty = Back {
            reader: Reader::file(read_config(2, 0), file.len() as u64)?,
            file: &file,
            page: vec![Entry::default(); 2],
            block: Vec::new(),
        };
        ensure!(empty.open()?.is_none(), "partition 0 has no blocks");
        Ok(())
    }

    /// Each misuse of a reader is a wrong call that names its rule.
    #[test]
    fn misuse_of_a_reader_is_a_wrong_call() -> Result<()> {
        let wrong = |result: Result<ReadStep, Error>| {
            let error = result.expect_err("a misuse accepted");
            assert_eq!(error.kind(), Kind::WrongCall, "{error}");
            error.rule()
        };
        let (_, file) = shared_file()?;
        assert_eq!(
            Reader::file(read_config(3, 3), file.len() as u64).map_err(Error::rule),
            Err(Rule::SetPartition)
        );
        // Partition 0 of the file, opened, a page of two entries.
        let opened = || -> Result<Back<'_>> {
            let mut back = Back {
                reader: Reader::file(read_config(3, 0), file.len() as u64)?,
                file: &file,
                page: vec![Entry::default(); 2],
                block: Vec::new(),
            };
            ensure!(back.open()? == Some(()), "partition 0 has blocks");
            Ok(back)
        };
        let mut back = opened()?;
        let mut out = vec![0; 16];
        assert_eq!(
            back.reader.body(&back.block, &mut out).map_err(Error::rule),
            Err(Rule::BodyRead)
        );
        back.header()?;
        assert_eq!(
            wrong(back.reader.next(&back.page, &back.block)),
            Rule::BodyNotRead
        );
        let mut short = vec![0; 8];
        assert_eq!(
            back.reader
                .body(&back.block, &mut short)
                .map_err(Error::rule),
            Err(Rule::BodyLength)
        );
        back.reader.body(&back.block, &mut out)?;
        assert_eq!(wrong(back.reader.seek(&back.page, 3)), Rule::SeekPosition);
        // A block asked for into a buffer too short for it.
        let mut back = opened()?;
        loop {
            let step = back.reader.next(&back.page, &back.block)?;
            if matches!(step, ReadStep::Block { .. }) {
                break;
            }
            ensure!(back.serve(step)?, "{step:?} before a block");
        }
        assert_eq!(
            wrong(back.reader.next(&back.page, &[0; 8])),
            Rule::ReaderRead
        );
        // A page too short to read a file's trailer into, and a zeroed
        // reader.
        let mut reader = Reader::file(read_config(3, 0), file.len() as u64)?;
        assert_eq!(wrong(reader.open(&[Entry::default()])), Rule::ReaderPage);
        assert_eq!(
            wrong(Reader::default().open(&[Entry::default(); 2])),
            Rule::ReaderState
        );

        // A refused seek leaves a body waiting, and a seek with an empty
        // page leaves it as it was.
        let mut back = opened()?;
        back.header()?;
        assert_eq!(back.seek(1).map_err(Error::rule), Err(Rule::SeekPosition));
        back.reader.body(&back.block, &mut out)?;
        assert_eq!(wrong(back.reader.seek(&[], 1)), Rule::ReaderPage);
        ensure!(
            back.next()?.is_some(),
            "the second block after a refused seek"
        );

        // A file's reader refuses an empty page rather than asking for
        // nothing.
        let mut other = Back {
            reader: Reader::file(read_config(3, 0), file.len() as u64)?,
            file: &file,
            page: vec![Entry::default(); 2],
            block: Vec::new(),
        };
        ensure!(other.open()? == Some(()));
        assert_eq!(
            wrong(other.reader.next(&[], &other.block)),
            Rule::ReaderPage
        );

        // Words the caller changed are refused, at each phase, before any
        // read is asked for: a file shorter than the trailer asked, lists
        // past the file, entries past the lists, a page asked before the
        // entries.
        let mut changed = Reader::file(read_config(3, 0), file.len() as u64)?;
        let page = [Entry::default(); 2];
        ensure!(matches!(changed.open(&page)?, ReadStep::Page { .. }));
        let mut cut = changed;
        cut.file_len = 0;
        assert_eq!(wrong(cut.open(&page)), Rule::ReaderState);
        let mut ready = other.reader;
        ready.lists_at = u64::MAX;
        assert_eq!(
            wrong(ready.next(&other.page, &other.block)),
            Rule::ReaderState
        );
        let mut ready = other.reader;
        ready.entries_at = u64::MAX;
        assert_eq!(
            wrong(ready.next(&other.page, &other.block)),
            Rule::ReaderState
        );
        let mut asked = other.reader;
        let step = asked.seek(&other.page, u64::MAX);
        ensure!(
            step.is_err() || matches!(step, Ok(ReadStep::Page { .. })),
            "{step:?}"
        );
        let mut asked = other.reader;
        asked.asked = PAGE;
        asked.asked_at = 0;
        asked.asked_len = 16;
        assert_eq!(
            wrong(asked.next(&other.page, &other.block)),
            Rule::ReaderState
        );
        let mut counting = changed;
        counting.cursor = u64::MAX;
        assert_eq!(wrong(counting.open(&page)), Rule::ReaderState);
        Ok(())
    }
}

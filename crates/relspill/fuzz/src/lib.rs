//! What the fuzz targets of relspill share: a reader of the fuzzer's
//! bytes; a host that reads a set's file through the crate, every read a
//! step asks for checked to lie in the file and in its region; a set
//! written from the bytes; and the machines of the crate, stepped by
//! events the bytes choose over regions that hold any numbers. A target
//! passes when no call panics, none runs without end, each error is of
//! the kind its call promises, and a machine a step refused is as it was.

use core::cell::Cell;
use core::sync::atomic::AtomicU64;

use relhash::{CHUNK_HEADER, RECORD_SIZE_OFFSET};
use relspill::group::{self, GROUP_WORDS, Group, ROWS_WORDS, RowsAction, RowsEvent, RowsHost};
use relspill::join::{self, JOIN_WORDS, Join, Side};
use relspill::phases::COUNTER_WORDS;
use relspill::plan::Shape;
use relspill::set::{
    Block, Config, ENTRY_SIZE, Entry, Lists, MIN_BUFFER, READER_WORDS, ReadConfig, ReadStep,
    Reader, Region, Step, WRITER_WORDS, Writer,
};
use relspill::shared::{self, BUILD_WORDS, Build};
use relspill::shared_spill::words_for;
use relspill::sort::{self, RUNS_WORDS, Runs};
use relspill::spill_columns::{self, ValueEnds};
use relspill::{BlockHeader, BlockKind, Bounds, Error, HEADER_SIZE, Kind, Rule, columns};

/// Steps of a loop over a call, past which the loop is taken for one
/// without end.
const MOST_STEPS: usize = 100_000;

/// The most bytes a bound the fuzzer gives lets a block take, so that a
/// body a header names fits in memory.
const MOST_BOUND: u64 = 1 << 20;

/// The fuzzer's bytes, read from the front; past their end, zeros.
pub struct Input<'a>(&'a [u8]);

impl<'a> Input<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self(data)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn byte(&mut self) -> u8 {
        match self.0.split_first() {
            Some((&byte, rest)) => {
                self.0 = rest;
                byte
            }
            None => 0,
        }
    }

    pub fn bool(&mut self) -> bool {
        self.byte() & 1 == 1
    }

    /// A number below `n`, 0 when `n` is 0.
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { self.u32() as usize % n }
    }

    pub fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.byte(), self.byte(), self.byte(), self.byte()])
    }

    pub fn u64(&mut self) -> u64 {
        u64::from(self.u32()) | u64::from(self.u32()) << 32
    }

    /// A number near the edges the crate checks: small, a power of two
    /// or one off it, a word of 32 bits, two such numbers of 32 bits in
    /// one word, as two fields of a machine lie, or any.
    pub fn number(&mut self) -> u64 {
        match self.byte() % 5 {
            0 => u64::from(self.byte()),
            1 => {
                let power = 1_u64 << (self.byte() % 64);
                power
                    .wrapping_add(u64::from(self.byte() % 3))
                    .wrapping_sub(1)
            }
            2 => u64::from(self.u32()),
            3 => u64::from(self.edge()) | u64::from(self.edge()) << 32,
            _ => self.u64(),
        }
    }

    /// A number of 32 bits near its edges: small, or a power of two or
    /// one off it.
    fn edge(&mut self) -> u32 {
        if self.bool() {
            u32::from(self.byte())
        } else {
            let power = 1_u32 << (self.byte() % 32);
            power
                .wrapping_add(u32::from(self.byte() % 3))
                .wrapping_sub(1)
        }
    }

    /// A share or a weight: one the nodes use, a fraction, a whole
    /// number, or any bits.
    pub fn float(&mut self) -> f64 {
        match self.byte() % 4 {
            0 => [0.0, 0.25, 0.5, 2.0 / 3.0, 0.875, 0.9, 1.0, 2.0][self.below(8)],
            1 => f64::from(self.byte()) / 255.0,
            2 => self.number() as f64,
            _ => f64::from_bits(self.u64()),
        }
    }

    /// The bytes left.
    pub fn rest(&mut self) -> &'a [u8] {
        core::mem::take(&mut self.0)
    }
}

/// The entries of `bytes` read into a page, as a host reads a file:
/// words of 8 bytes, two to an entry, its `offset` then its `stored`.
fn fill(page: &mut [Entry], bytes: &[u8]) {
    for (index, word) in bytes.as_chunks::<8>().0.iter().enumerate() {
        let value = u64::from_ne_bytes(*word);
        let entry = &mut page[index / 2];
        if index.is_multiple_of(2) {
            entry.offset = value;
        } else {
            entry.stored = value;
        }
    }
}

/// An error of a read of a file whose host keeps to the protocol: damaged
/// bytes, or a seek to a place no block of the lists starts at.
fn damaged(error: Error) {
    assert!(
        error.kind() == Kind::Damaged || error.rule() == Rule::SeekPosition,
        "a wrong call of a host that keeps to the protocol: {error}"
    );
}

/// A host that reads one partition of a file back.
struct Back<'a> {
    reader: Reader,
    file: &'a [u8],
    page: Vec<Entry>,
    block: Vec<u8>,
    bounds: Bounds,
    /// Where the last block read starts.
    at: u64,
}

impl Back<'_> {
    /// Read what a step asks into its region, which must hold it, from a
    /// file that must hold it: false for a step that asks for nothing.
    fn serve(&mut self, step: ReadStep) -> bool {
        let (at, len, region) = match step {
            ReadStep::Page { at, len } => (at, len, self.page.len() * ENTRY_SIZE),
            ReadStep::Block { at, len } => (at, len, self.block.len()),
            _ => return false,
        };
        let end = at.checked_add(len as u64).expect("a read past every byte");
        assert!(
            end <= self.file.len() as u64,
            "{step:?} past the file of {} bytes",
            self.file.len()
        );
        assert!(len <= region, "{step:?} past its region of {region} bytes");
        let bytes = &self.file[at as usize..end as usize];
        match step {
            ReadStep::Page { .. } => fill(&mut self.page, bytes),
            _ => {
                self.block[..len].copy_from_slice(bytes);
                self.at = at;
            }
        }
        true
    }

    /// The steps a call may take: each reads a word of the file at
    /// least, so a file of its words and some more.
    fn most_steps(&self) -> usize {
        MOST_STEPS.max(self.file.len() / 8 + 16)
    }

    /// Make a call again, serving its steps, until it gives what it found.
    fn run(
        &mut self,
        call: impl Fn(&mut Reader, &[Entry], &[u8]) -> relspill::Result<ReadStep>,
    ) -> relspill::Result<ReadStep> {
        for _ in 0..self.most_steps() {
            let step = call(&mut self.reader, &self.page, &self.block)?;
            if let ReadStep::Ready { largest } = step {
                let slack = HEADER_SIZE + columns::pack_slack(columns::MAX_WORDS);
                let most = self.bounds.largest() + slack as u64;
                assert!(largest <= most, "a block of {largest} bytes, past {most}");
                self.block = vec![0; largest as usize];
                return Ok(step);
            }
            if !self.serve(step) {
                return Ok(step);
            }
        }
        panic!("a call that does not end")
    }

    /// The next block's header and body, or `None` at the end.
    fn next(&mut self) -> relspill::Result<Option<(BlockHeader, Vec<u8>)>> {
        let header = match self.run(|reader, page, block| reader.next(page, block))? {
            ReadStep::End => return Ok(None),
            ReadStep::Header(header) => header,
            step => panic!("{step:?} for a block"),
        };
        let mut body = vec![0; header.len as usize];
        self.reader.body(&self.block, &mut body)?;
        Ok(Some((header, body)))
    }
}

/// A host's rule of where a value ends: anywhere, or after the count of
/// bytes its first byte gives, as a varlena's length tells.
enum Ends {
    Any,
    Counted,
}

impl ValueEnds for Ends {
    fn ends(&self, _: usize, rest: &[u8]) -> bool {
        match self {
            Self::Any => true,
            Self::Counted => rest.first().is_some_and(|&len| rest.len() > len as usize),
        }
    }
}

/// A chunk of columns read back, checked as a node checks it, with the
/// values read before it and a count of words and of rows the bytes
/// choose, then read by windows when it passes: damaged bytes or its
/// words, never a wrong call; each window the words and NULLs its lanes
/// hold, a NULL's word 0.
fn read_columns(chunk: &[u8], values: &[u8], ends: &Ends, input: &mut Input) {
    let words = if input.bool() {
        columns::shape(chunk).map_or(0, |shape| shape.words)
    } else {
        input.below(8)
    };
    let flags = input.below(words + 1);
    let by_value: Vec<bool> = (0..flags).map(|_| input.bool()).collect();
    let rows = if input.bool() {
        Some(input.below(64))
    } else {
        None
    };
    let shape = match spill_columns::check(chunk, words, rows, &by_value, values, ends) {
        Ok(shape) => shape,
        Err(error) => return assert_eq!(error.kind(), Kind::Damaged, "{error}"),
    };
    for word in 0..shape.words {
        for first in (0..shape.rows).step_by(64) {
            let count = 64.min(shape.rows - first);
            let (mut out, mut nulls) = (vec![7; count], vec![true; count]);
            spill_columns::window(chunk, word, first, &mut out, &mut nulls)
                .expect("a window of a chunk the check passed");
            for (at, (&got, &null)) in out.iter().zip(&nulls).enumerate() {
                let row = first + at;
                let lane = |lane: usize| {
                    let at = shape.lane_at(lane) + 8 * row;
                    u64::from_ne_bytes(chunk[at..at + 8].try_into().expect("a word"))
                };
                let is_null = lane(word / 64) >> (word % 64) & 1 != 0;
                let word = if is_null {
                    0
                } else {
                    lane(shape.word_lane(word))
                };
                assert_eq!((got, null), (word, is_null), "row {row} of a window");
            }
        }
    }
}

/// Every read of the crate over one partition of `file`: the partition
/// read to its end, each body taken, each chunk of columns checked and
/// read; then seeks, to the blocks read and to places the bytes choose,
/// and the block each finds, which for a block read before must be that
/// block. Until a call fails, each error is damaged bytes; after it, the
/// calls go on, and must only end and keep to the file and the regions.
pub fn read_partition(file: &[u8], config: ReadConfig, input: &mut Input) {
    let Ok(reader) = Reader::file(config, file.len() as u64) else {
        return;
    };
    let mut back = Back {
        reader,
        file,
        page: vec![Entry::default(); 2 + input.below(7)],
        block: Vec::new(),
        bounds: config.bounds,
        at: 0,
    };
    match back.run(|reader, page, _| reader.open(page)) {
        Err(error) => return damaged(error),
        Ok(ReadStep::Empty) => return,
        Ok(ReadStep::Ready { .. }) => {}
        Ok(step) => panic!("{step:?} for an open"),
    }
    let ends = if input.bool() {
        Ends::Counted
    } else {
        Ends::Any
    };
    let mut failed = false;
    let mut values = Vec::new();
    let mut read = Vec::new();
    // A block takes a header at least.
    let most = file.len() / HEADER_SIZE + 1;
    loop {
        match back.next() {
            Ok(None) => break,
            Ok(Some((header, body))) => {
                assert!(read.len() < most, "more blocks than the file holds");
                match header.kind {
                    BlockKind::Values => values.clone_from(&body),
                    BlockKind::Columns => read_columns(&body, &values, &ends, input),
                    BlockKind::Records => {}
                }
                read.push((back.at, header, body));
            }
            Err(error) => {
                damaged(error);
                failed = true;
                break;
            }
        }
    }
    for _ in 0..input.below(8) {
        let offset = match read.len() {
            0 => input.number(),
            count => {
                if input.bool() {
                    read[input.below(count)].0
                } else {
                    input.number()
                }
            }
        };
        let found = back
            .run(|reader, page, _| reader.seek(page, offset))
            .and_then(|_| back.next());
        match found {
            Ok(Some((header, body))) if !failed => {
                if let Some((_, was, was_body)) = read.iter().find(|(at, ..)| *at == offset) {
                    assert!(
                        header == *was && body == *was_body,
                        "a seek to {offset} found another block"
                    );
                }
            }
            Err(error) if !failed => {
                damaged(error);
                failed = true;
            }
            _ => {}
        }
    }
}

/// Any bytes as a set's file, of a set the bytes before it choose or,
/// half the time, of the count of partitions and the fingerprint its
/// trailer names, every partition read; then the bytes as a block and as
/// packed bodies of any length, each read by its call, which leaves the
/// chunk it refuses as it was.
pub fn read_bytes(input: &mut Input) {
    let mut partitions = 1 + input.below(16) as u32;
    let level = input.below(33) as u32;
    let mut fingerprint = if input.bool() { 0 } else { input.u64() };
    let bounds = Bounds {
        chunk: input.number() % MOST_BOUND,
        values: input.number() % MOST_BOUND,
    };
    let trailer = input.bool();
    let file = input.rest();
    if trailer && file.len() >= 32 {
        let word = |at: usize| u64::from_ne_bytes(file[at..at + 8].try_into().expect("a word"));
        let named = word(file.len() - 16);
        partitions = if (1..=64).contains(&named) {
            named as u32
        } else {
            partitions
        };
        fingerprint = word(file.len() - 8);
    }
    let mut input = Input::new(file);
    for partition in 0..partitions {
        let config = ReadConfig {
            partitions,
            level,
            partition,
            fingerprint,
            bounds,
        };
        read_partition(file, config, &mut input);
    }
    // The file as a block and as the bodies a block holds: damaged bytes,
    // or a buffer shorter than a header, the caller's.
    let mut input = Input::new(file);
    if let Err(error) = BlockHeader::read(file, fingerprint, bounds) {
        assert!(
            error.kind() == Kind::Damaged || error.rule() == Rule::Header,
            "{error}"
        );
    }
    let len = input.below(1 << 16) & !7;
    let mut chunk = vec![0xa5; len];
    if let Err(error) = relspill::unpack(file, &mut chunk) {
        assert_eq!(error.kind(), Kind::Damaged, "{error}");
        assert!(
            chunk.iter().all(|&byte| byte == 0xa5),
            "a refused unpack wrote"
        );
    }
    let mut chunk = vec![0xa5; len];
    if let Err(error) = columns::unpack(file, &mut chunk) {
        assert_eq!(error.kind(), Kind::Damaged, "{error}");
        assert!(
            chunk.iter().all(|&byte| byte == 0xa5),
            "a refused unpack wrote"
        );
    }
    let split = input.below(file.len() + 1);
    let (chunk, values) = file.split_at(split);
    let ends = if input.bool() {
        Ends::Counted
    } else {
        Ends::Any
    };
    read_columns(chunk, values, &ends, &mut input);
}

/// A host's block lists: a region and a count for each partition, and
/// an empty one for a partition past them, which a writer of any words
/// may name.
struct HostLists(Vec<(Vec<Entry>, u64)>, (Vec<Entry>, u64));

impl Lists for HostLists {
    fn list(&mut self, partition: usize) -> (&mut [Entry], &mut u64) {
        let (entries, count) = match self.0.get_mut(partition) {
            Some(list) => list,
            None => &mut self.1,
        };
        (entries, count)
    }
}

/// A host that writes a set's file in memory, and grows its regions when
/// a step asks.
struct Writing {
    writer: Writer,
    buffer: Vec<u8>,
    scratch: Vec<u8>,
    lists: HostLists,
    file: Vec<u8>,
}

impl Writing {
    /// Do what a step asks: grow a region, or write the ranges into the
    /// file and report them. True when the call is done.
    fn serve(&mut self, step: Step, partition: u32, body: &[u8]) -> relspill::Result<bool> {
        let (ranges, done) = match step {
            Step::Scratch(len) => {
                self.scratch.resize(len, 0);
                return Ok(false);
            }
            Step::List(len) => {
                self.lists.0[partition as usize]
                    .0
                    .resize(len, Entry::default());
                return Ok(false);
            }
            Step::Again(ranges) => (ranges, false),
            Step::Done { ranges, .. } => (ranges, true),
        };
        for range in ranges.as_slice() {
            let list: Vec<u8>;
            let from: &[u8] = match range.region {
                Region::Buffer => &self.buffer,
                Region::Scratch => &self.scratch,
                Region::Body => body,
                Region::List(partition) => {
                    list = self.lists.0[partition as usize]
                        .0
                        .iter()
                        .flat_map(|entry| [entry.offset, entry.stored])
                        .flat_map(u64::to_ne_bytes)
                        .collect();
                    &list
                }
            };
            let (at, len) = (range.at as usize, range.len);
            if self.file.len() < at + len {
                self.file.resize(at + len, 0);
            }
            self.file[at..at + len].copy_from_slice(&from[range.start..][..len]);
        }
        if !ranges.as_slice().is_empty() {
            self.writer.written()?;
        }
        Ok(done)
    }
}

/// A chunk of `count` records of `len` bytes: a used mark, each record's
/// length, and words from `seed` elsewhere, as wide as `mask` lets them.
fn records(count: usize, len: usize, seed: u32, mask: u32) -> Vec<u8> {
    let mut bytes = vec![0_u8; CHUNK_HEADER + count * len];
    let total = bytes.len() as u64;
    bytes[..CHUNK_HEADER].copy_from_slice(&total.to_ne_bytes());
    for record in 0..count {
        let base = CHUNK_HEADER + record * len;
        for lane in 0..len / 4 {
            let value = if lane * 4 == RECORD_SIZE_OFFSET {
                (len / 8) as u32
            } else {
                seed.wrapping_mul((record + lane) as u32 + 1) & mask
            };
            bytes[base + 4 * lane..base + 4 * lane + 4].copy_from_slice(&value.to_ne_bytes());
        }
    }
    bytes
}

/// A chunk of columns of `rows` rows of `words` words, its lanes filled
/// from `seed` as wide as `mask` lets them.
fn columns_chunk(rows: usize, words: usize, seed: u64, mask: u64) -> Vec<u8> {
    let mut chunk = vec![0_u8; columns::size(rows.max(1), words)];
    columns::init(&mut chunk, words).expect("a chunk of its own size");
    let shape = columns::shape(&chunk).expect("a chunk made");
    columns::set_rows(&mut chunk, rows);
    for lane in 0..columns::lanes(words) {
        for row in 0..rows {
            let at = shape.lane_at(lane) + 8 * row;
            let value = seed.wrapping_mul((lane * 1000 + row + 1) as u64) & mask;
            chunk[at..at + 8].copy_from_slice(&value.to_ne_bytes());
        }
    }
    chunk
}

/// A set's file written from the bytes, and the config it was written
/// with: blocks of every kind in the partitions the bytes choose.
pub struct Written {
    pub file: Vec<u8>,
    pub config: Config,
    pub bounds: Bounds,
}

impl Written {
    pub fn new(input: &mut Input) -> Option<Self> {
        let config = Config {
            partitions: 1 + input.below(8) as u32,
            level: input.below(32) as u32,
            fingerprint: input.u64(),
            bounds: Bounds::all(MOST_BOUND),
            buffer_len: MIN_BUFFER + 8 * input.below(512),
            shared: input.bool(),
        };
        let mut host = Writing {
            writer: Writer::new(config).ok()?,
            buffer: vec![0; config.buffer_len],
            scratch: Vec::new(),
            lists: HostLists(
                vec![(Vec::new(), 0); config.partitions as usize],
                (Vec::new(), 0),
            ),
            file: Vec::new(),
        };
        let masks = [0, 0xff, 0xffff, u32::MAX];
        for number in 0..input.below(24) as u32 {
            let partition = input.below(config.partitions as usize) as u32;
            let seed = input.u32();
            let mask = masks[input.below(4)];
            let (kind, body) = match input.byte() % 3 {
                0 => (
                    BlockKind::Values,
                    (0..8 * input.below(64))
                        .map(|byte| (seed >> (byte % 24)) as u8 ^ byte as u8)
                        .collect(),
                ),
                1 => (
                    BlockKind::Records,
                    records(1 + input.below(16), 24 + 8 * input.below(6), seed, mask),
                ),
                _ => (
                    BlockKind::Columns,
                    columns_chunk(
                        input.below(40),
                        1 + input.below(4),
                        u64::from(seed),
                        u64::from(mask) | u64::from(mask) << 32,
                    ),
                ),
            };
            let block = Block {
                kind,
                number,
                partition,
                body: &body,
            };
            loop {
                let step = host
                    .writer
                    .write(&mut host.buffer, &mut host.scratch, &mut host.lists, block)
                    .expect("a block of its kind written");
                if host
                    .serve(step, partition, &body)
                    .expect("a write reported")
                {
                    break;
                }
            }
        }
        loop {
            let step = host
                .writer
                .finish(&mut host.buffer, &mut host.lists)
                .expect("a set finished");
            if host.serve(step, 0, &[]).expect("a write reported") {
                break;
            }
        }
        Some(Self {
            bounds: host.writer.read_bounds(),
            file: host.file,
            config,
        })
    }

    /// Damage the file in one to eight places the bytes choose: a byte
    /// changed, or a word set or moved by a little.
    pub fn damage(&mut self, input: &mut Input) {
        if self.file.is_empty() {
            return;
        }
        for _ in 0..1 + input.below(8) {
            let at = input.below(self.file.len());
            match input.byte() % 3 {
                0 => self.file[at] ^= 1 | input.byte(),
                1 if at + 8 <= self.file.len() => {
                    let at = at & !7;
                    let value = input.number();
                    self.file[at..at + 8].copy_from_slice(&value.to_ne_bytes());
                }
                _ if at + 4 <= self.file.len() => {
                    let at = at & !3;
                    let word = u32::from_ne_bytes(self.file[at..at + 4].try_into().expect("4"));
                    let moved = word
                        .wrapping_add(u32::from(input.byte() % 17))
                        .wrapping_sub(8);
                    self.file[at..at + 4].copy_from_slice(&moved.to_ne_bytes());
                }
                _ => self.file[at] = input.byte(),
            }
        }
    }

    /// Every partition read back by every read of the crate.
    pub fn read(&self, input: &mut Input) {
        for partition in 0..self.config.partitions {
            let config = ReadConfig {
                partitions: self.config.partitions,
                level: self.config.level,
                partition,
                fingerprint: self.config.fingerprint,
                bounds: self.bounds,
            };
            read_partition(&self.file, config, input);
        }
    }
}

/// The most partitions a host makes regions for: those a spill's words
/// count. An action that asks for more leaves the regions as they were,
/// and the steps after it are refused.
const MOST_PARTITIONS: usize = 1 << 16;

/// Words the bytes give: any numbers.
fn words<const N: usize>(input: &mut Input) -> [u64; N] {
    core::array::from_fn(|_| input.number())
}

/// Numbers for a region of `len` the bytes choose, at most `most`.
fn numbers(input: &mut Input, most: usize) -> Vec<u64> {
    (0..input.below(most + 1)).map(|_| input.number()).collect()
}

/// A shape of a level's plan the bytes give.
fn shape(input: &mut Input) -> Shape {
    let min = 1_u32 << input.below(6);
    Shape {
        reserve: input.number() as usize,
        min_partitions: if input.bool() {
            min
        } else {
            input.number() as u32
        },
        max_partitions: min << input.below(5),
        share: input.number() as usize,
        min_chunk: 8 * input.below(64),
        max_chunk: input.number() as usize,
        record: 8 * input.below(16),
    }
}

/// Step a machine with events the bytes choose, refused ones included: a
/// refusal is a wrong call, and leaves the machine's `N` words as they
/// were. `step` makes one step, and does what its action asks of the
/// host's regions.
fn drive<M: Copy, const N: usize>(
    machine: &mut M,
    input: &mut Input,
    mut step: impl FnMut(&mut M, &mut Input) -> relspill::Result<()>,
) {
    for _ in 0..64 {
        if input.is_empty() {
            break;
        }
        // SAFETY: every machine is `repr(C)` of integers and floats with no
        // padding, its words those of a C host: any words are one, and it
        // is its words.
        let before: [u64; N] = unsafe { words_of(machine) };
        if let Err(error) = step(machine, input) {
            assert_eq!(error.kind(), Kind::WrongCall, "{error}");
            // SAFETY: as above.
            let after: [u64; N] = unsafe { words_of(machine) };
            assert!(
                before == after,
                "a refused step changed the machine: {error}"
            );
        }
    }
}

/// The words of a machine, of its size.
///
/// # Safety
///
/// `M` is `N` words of integers and floats with no padding.
unsafe fn words_of<M: Copy, const N: usize>(machine: &M) -> [u64; N] {
    assert_eq!(size_of::<M>(), 8 * N);
    // SAFETY: the caller's.
    unsafe { core::mem::transmute_copy(machine) }
}

/// A machine of the bytes: made from params, or from any words.
fn machine<M, const N: usize>(
    input: &mut Input,
    new: impl FnOnce(&mut Input) -> relspill::Result<M>,
) -> Option<M> {
    if input.bool() {
        match new(input) {
            Ok(machine) => Some(machine),
            Err(error) => {
                assert_eq!(error.kind(), Kind::WrongCall, "{error}");
                None
            }
        }
    } else {
        let words: [u64; N] = words(input);
        assert_eq!(size_of::<M>(), size_of::<[u64; N]>());
        // SAFETY: a machine is words of integers and floats, any value of
        // which is one; the sizes are equal.
        Some(unsafe { core::mem::transmute_copy(&words) })
    }
}

/// The machine of a sort's runs.
fn sort_runs(input: &mut Input) {
    let Some(mut runs) = machine::<Runs, RUNS_WORDS>(input, |input| {
        let params = sort::Params {
            limit: input.number(),
            row: input.number(),
            item: input.number(),
            chunk: input.number(),
            set_runs: input.number() as u32,
            max_merge: input.number() as u32,
            batch_rows: input.number() as u32,
            backward: input.byte() as u32 % 3,
        };
        Runs::new(&params).map(|(runs, _)| runs)
    }) else {
        return;
    };
    drive::<_, RUNS_WORDS>(&mut runs, input, |runs, input| {
        let event = match input.byte() % 4 {
            0 => sort::Event::Grown {
                memory: input.number(),
                rows: input.number(),
                limit: input.number(),
            },
            1 => sort::Event::InputEnd {
                rows: input.number(),
            },
            2 => sort::Event::Next,
            _ => sort::Event::Rescan,
        };
        runs.step(event).map(|_| ())
    });
}

/// The regions of a join's host.
struct JoinHost {
    words: Vec<Cell<u64>>,
    rows: Vec<u64>,
    total: u64,
    outer: Vec<u64>,
    disk: Vec<u64>,
    routes: Vec<u8>,
}

/// The machine of a join's spill.
fn join_spill(input: &mut Input) {
    let Some(mut join) = machine::<Join, JOIN_WORDS>(input, |input| {
        let params = join::Params {
            limit: input.number(),
            shape: shape(input),
            outer_words: input.number(),
            outer_values: input.byte() as u32 % 3,
            preserve_inner: input.byte() as u32 % 3,
            unmatched_outer: input.byte() as u32 % 3,
            spare: 0,
            page: input.number(),
            start: input.float(),
            target: input.float(),
            spilled: input.float(),
            resident: input.float(),
            tail: input.float(),
            split_room: input.float(),
            split_key: input.float(),
        };
        Join::new(&params)
    }) else {
        return;
    };
    let mut host = JoinHost {
        words: numbers(input, 64).into_iter().map(Cell::new).collect(),
        rows: numbers(input, 16),
        total: input.number(),
        outer: numbers(input, 16),
        disk: numbers(input, 16),
        routes: (0..input.below(17)).map(|_| input.byte() % 4).collect(),
    };
    drive::<_, JOIN_WORDS>(&mut join, input, |join, input| {
        poke(input, &host.words);
        let partition = input.number() as u32;
        let event = match input.byte() % 15 {
            0 => join::Event::Inner {
                held: input.number(),
                rows: input.number(),
                estimate: input.float(),
                needed: input.number(),
            },
            1 => join::Event::Made,
            2 => join::Event::Full {
                side: if input.bool() {
                    Side::Inner
                } else {
                    Side::Outer
                },
                partition,
                tail: input.bool(),
            },
            3 => join::Event::Grown {
                memory: input.number(),
            },
            4 => join::Event::Sent {
                freed: input.bool(),
                memory: input.number(),
            },
            5 => join::Event::BuildEnd,
            6 => join::Event::OuterEnd,
            7 => join::Event::Next {
                memory: input.number(),
            },
            8 => join::Event::Take {
                partition,
                memory: input.number(),
            },
            9 => join::Event::Piece {
                memory: input.number(),
            },
            10 => join::Event::Block {
                values: input.bool(),
                len: input.number(),
            },
            11 => join::Event::FileEnd,
            12 => join::Event::PassEnd,
            13 => join::Event::TableEnd { held: input.bool() },
            _ => join::Event::TailEnd,
        };
        let action = join.step(
            event,
            &mut join::Host {
                words: &host.words,
                rows: &host.rows,
                total: host.total,
                outer: &host.outer,
                disk: &host.disk,
                routes: &mut host.routes,
            },
        )?;
        if let join::Action::Start(start) = action
            && start.plan.partitions as usize <= MOST_PARTITIONS
        {
            let partitions = start.plan.partitions as usize;
            host.words = (0..start.plan.words).map(|_| Cell::new(0)).collect();
            host.rows.resize(partitions, 0);
            host.outer.resize(partitions, 0);
            host.disk.resize(partitions, 0);
            host.routes.resize(partitions, 0);
        }
        Ok(())
    });
}

/// Set a word of `words` to a number the bytes give, now and then: the
/// host's words may hold any.
fn poke(input: &mut Input, words: &[Cell<u64>]) {
    if input.byte().is_multiple_of(8) && !words.is_empty() {
        words[input.below(words.len())].set(input.number());
    }
}

/// The machine of a grouping's spill.
fn group_spill(input: &mut Input) {
    let Some(mut group) = machine::<Group, GROUP_WORDS>(input, |input| {
        let params = group::Params {
            limit: input.number(),
            shape: shape(input),
            merged: input.number(),
            fill: input.number(),
            start: input.float(),
            target: input.float(),
            spilled: input.float(),
            split_room: input.float(),
        };
        Group::new(&params)
    }) else {
        return;
    };
    let mut words: Vec<Cell<u64>> = numbers(input, 64).into_iter().map(Cell::new).collect();
    let mut records = numbers(input, 16);
    let mut disk = numbers(input, 16);
    drive::<_, GROUP_WORDS>(&mut group, input, |group, input| {
        poke(input, &words);
        let partition = input.number() as u32;
        let event = match input.byte() % 9 {
            0 => group::Event::Inner {
                bytes: input.number(),
                records: input.number(),
                estimate: input.float(),
                force: input.bool(),
            },
            1 => group::Event::Made,
            2 => group::Event::Full {
                partition,
                tail: input.bool(),
            },
            3 => group::Event::Partial {
                memory: input.number(),
                records: input.number(),
                rows: input.number(),
                spills: input.bool(),
            },
            4 => group::Event::Grown {
                memory: input.number(),
            },
            5 => group::Event::Sent {
                memory: input.number(),
            },
            6 => group::Event::InputEnd,
            7 => group::Event::Next,
            _ => group::Event::Weighed {
                groups: input.number(),
                others: input.number(),
                memory: input.number(),
            },
        };
        let host = group::Host {
            words: &words,
            records: &records,
            disk: &disk,
        };
        if let group::Action::Start(plan) = group.step(event, &host)?
            && plan.partitions as usize <= MOST_PARTITIONS
        {
            let partitions = plan.partitions as usize;
            words = (0..plan.words).map(|_| Cell::new(0)).collect();
            records.resize(partitions, 0);
            disk.resize(partitions, 0);
        }
        Ok(())
    });
}

/// The machine of a grouping's rows.
fn group_rows(input: &mut Input) {
    let Some(mut rows) = machine::<group::Rows, ROWS_WORDS>(input, |input| {
        let params = group::RowsParams {
            limit: input.number(),
            shape: shape(input),
            fill: input.number(),
            least: input.number(),
        };
        if input.bool() {
            group::Rows::new(&params)
        } else {
            let table = group::Table {
                partition: input.number() as u32,
                level: input.number() as u32,
                shift: input.number() as u32,
                last: input.bool(),
            };
            group::Rows::read(&params, &table)
        }
    }) else {
        return;
    };
    let mut written = numbers(input, 16);
    drive::<_, ROWS_WORDS>(&mut rows, input, |rows, input| {
        let event = match input.byte() % 3 {
            0 => RowsEvent::Grown {
                memory: input.number(),
                records: input.number(),
            },
            1 => RowsEvent::Written,
            _ => RowsEvent::Next,
        };
        let action = rows.step(event, &RowsHost { written: &written })?;
        if let RowsAction::Freeze { plan, .. } = action
            && plan.partitions as usize <= MOST_PARTITIONS
        {
            written.resize(plan.partitions as usize, 0);
        }
        Ok(())
    });
}

/// A participant's machine of a build by several processes.
fn shared_build(input: &mut Input) {
    let Some(mut build) = machine::<Build, BUILD_WORDS>(input, |input| {
        let params = shared::Params {
            start: input.float(),
            target: input.float(),
            spilled: input.float(),
            limit: input.number(),
            max_chunks: input.number(),
            per_check: input.number() as u32,
            preserve: input.byte() as u32 % 3,
        };
        Build::new(&params)
    }) else {
        return;
    };
    let counters: [AtomicU64; COUNTER_WORDS] =
        core::array::from_fn(|_| AtomicU64::new(input.number()));
    let spill_len = match input.byte() % 4 {
        0 => input.below(64),
        _ => words_for(1 + input.below(16)).expect("words for up to 16 partitions"),
    };
    let spill: Vec<AtomicU64> = (0..spill_len)
        .map(|word| {
            AtomicU64::new(if word < 5 || input.bool() {
                input.number()
            } else {
                0
            })
        })
        .collect();
    let mut resident: Vec<u8> = (0..input.below(17)).map(|_| input.byte() % 2).collect();
    drive::<_, BUILD_WORDS>(&mut build, input, |build, input| {
        let event = match input.byte() % 11 {
            0 => shared::Event::Done,
            1 => shared::Event::Attached {
                phase: input.number() as u32,
            },
            2 => shared::Event::Arrived {
                elected: input.bool(),
            },
            3 => shared::Event::Left { last: input.bool() },
            4 => shared::Event::Batch { over: input.bool() },
            5 => shared::Event::Planned {
                partitions: input.number() as u32,
            },
            6 => shared::Event::Weighed {
                bytes: input.number(),
                blocks: input.number(),
                index: input.number(),
            },
            7 => shared::Event::Next,
            8 => shared::Event::Visited {
                outer: input.bool(),
            },
            9 => shared::Event::Sent {
                freed: input.bool(),
            },
            _ => shared::Event::Done,
        };
        let host = shared::Host {
            counters: &counters,
            spill: &spill,
            resident: &resident,
        };
        if let shared::Action::Switch { partitions } = build.step(event, &host)?
            && partitions as usize <= MOST_PARTITIONS
        {
            resident.resize(partitions as usize, 0);
        }
        Ok(())
    });
}

/// A set's reader of any words, called with regions of any bytes: each
/// call ends, and a page it asks for fits the page.
fn reader_words(input: &mut Input) {
    let words: [u64; READER_WORDS] = words(input);
    // SAFETY: a reader is words of integers, any value of which is one,
    // and the sizes are equal, as `READER_WORDS` says.
    let mut reader: Reader = unsafe { core::mem::transmute_copy(&words) };
    let page: Vec<Entry> = (0..input.below(8))
        .map(|_| Entry {
            offset: input.number(),
            stored: input.number(),
        })
        .collect();
    let block: Vec<u8> = (0..input.below(512)).map(|_| input.byte()).collect();
    for _ in 0..32 {
        if input.is_empty() {
            break;
        }
        let step = match input.byte() % 4 {
            0 => reader.open(&page),
            1 => reader.next(&page, &block),
            2 => {
                let mut out = vec![0; input.below(512)];
                reader.body(&block, &mut out).map(|()| ReadStep::End)
            }
            _ => reader.seek(&page, input.number()),
        };
        if let Ok(ReadStep::Page { len, .. }) = step {
            assert!(len <= page.len() * ENTRY_SIZE, "a page of {len} bytes");
        }
    }
}

/// A set's writer of any words, called with regions and blocks of any
/// bytes: each call ends, and refuses only as a wrong call, a refused
/// write leaving the set as it was.
fn writer_words(input: &mut Input) {
    let words: [u64; WRITER_WORDS] = words(input);
    // SAFETY: a writer is words of integers, any value of which is one,
    // and the sizes are equal, as `WRITER_WORDS` says.
    let mut writer: Writer = unsafe { core::mem::transmute_copy(&words) };
    // A reset and a finish walk every partition's list. A writer of more
    // partitions than a spill's words count is a host's that keeps no such
    // lists; a call of it takes long, though it ends (roadmap, "Not
    // placed").
    if writer.partitions() as usize > MOST_PARTITIONS {
        return;
    }
    let mut buffer = vec![0; input.below(1024)];
    let mut scratch = vec![0; input.below(1024)];
    let mut lists = HostLists(
        (0..input.below(9))
            .map(|_| {
                let entries = (0..input.below(8))
                    .map(|_| Entry {
                        offset: input.number(),
                        stored: input.number(),
                    })
                    .collect();
                (entries, input.number())
            })
            .collect(),
        (Vec::new(), 0),
    );
    let body: Vec<u8> = (0..input.below(512)).map(|_| input.byte()).collect();
    for _ in 0..32 {
        if input.is_empty() {
            break;
        }
        let done = match input.byte() % 6 {
            0 => {
                let kind =
                    [BlockKind::Records, BlockKind::Values, BlockKind::Columns][input.below(3)];
                let block = Block {
                    kind,
                    number: input.number() as u32,
                    partition: input.number() as u32,
                    body: &body,
                };
                // A refused write leaves the set as it was.
                let before = writer;
                let wrote = writer.write(&mut buffer, &mut scratch, &mut lists, block);
                if wrote.is_err() {
                    assert!(writer == before, "a refused write changed the writer");
                }
                wrote.map(|_| ())
            }
            1 => writer.finish(&mut buffer, &mut lists).map(|_| ()),
            2 => writer.written(),
            3 => {
                writer.failed();
                Ok(())
            }
            4 => writer.reset(&mut lists),
            _ => writer.drop_list(&mut lists, input.number() as u32),
        };
        if let Err(error) = done {
            assert_eq!(error.kind(), Kind::WrongCall, "{error}");
        }
    }
}

/// A machine the bytes choose, stepped by events they choose.
pub fn machines(input: &mut Input) {
    match input.byte() % 7 {
        0 => sort_runs(input),
        1 => join_spill(input),
        2 => group_spill(input),
        3 => group_rows(input),
        4 => shared_build(input),
        5 => reader_words(input),
        _ => writer_words(input),
    }
}

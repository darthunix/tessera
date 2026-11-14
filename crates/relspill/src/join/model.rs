//! A host of the join's machine for the tests: memory is a count of
//! bytes, a file is a vector of blocks, and a chunk holds as many rows as
//! its bytes do. It joins two lists of rows through the machine, doing
//! what each action says, and checks the machine's promises as it goes.

use core::cell::Cell;

use super::*;

/// A row of a side: its key and its number among the side's rows.
#[derive(Clone, Copy, Debug)]
pub(super) struct Row {
    pub(super) key: u32,
    pub(super) id: u32,
}

/// The hash of a key, which picks its partition.
pub(super) fn hash(key: u32) -> u32 {
    key.wrapping_mul(0x9e37_79b1).rotate_left(7)
}

/// The bytes an outer row takes in a chunk of columns: two words and
/// its lane of NULL bits.
const OUTER_ROW: u64 = 24;

/// The pairs of a join, the ids of an inner row and an outer row.
pub(super) type Pairs = Vec<(u32, u32)>;

/// What a model ran, by [`Ran`]'s indexes.
pub(super) type Counts = [usize; 10];

/// The indexes of [`Counts`].
pub(super) struct Ran;

impl Ran {
    pub(super) const DEMOTED: usize = 0;
    pub(super) const EVICTED: usize = 1;
    pub(super) const FLUSHED: usize = 2;
    pub(super) const PROBED: usize = 3;
    pub(super) const WRITTEN: usize = 4;
    pub(super) const ANSWERED: usize = 5;
    pub(super) const RELEASED: usize = 6;
    pub(super) const OPENED: usize = 7;
    pub(super) const SPLIT: usize = 8;
    /// The most levels at once, not a count of actions.
    pub(super) const DEEPEST: usize = 9;
}

/// The pairs of a join in memory.
pub(super) fn pairs(inner: &[Row], outer: &[Row], out: &mut Pairs) {
    for o in outer {
        for i in inner {
            if i.key == o.key {
                out.push((i.id, o.id));
            }
        }
    }
}

/// A host's regions for a level, or for none.
pub(super) struct Regions {
    pub(super) words: Vec<Cell<u64>>,
    pub(super) rows: Vec<u64>,
    pub(super) total: u64,
    pub(super) outer: Vec<u64>,
    pub(super) disk: Vec<u64>,
    pub(super) routes: Vec<u8>,
}

impl Regions {
    pub(super) fn none() -> Self {
        Self {
            words: Vec::new(),
            rows: Vec::new(),
            total: 0,
            outer: Vec::new(),
            disk: Vec::new(),
            routes: Vec::new(),
        }
    }

    pub(super) fn of(start: &Start) -> Self {
        let partitions = start.plan.partitions as usize;
        Self {
            words: (0..start.plan.words).map(|_| Cell::new(0)).collect(),
            rows: vec![0; partitions],
            total: 0,
            outer: vec![0; partitions],
            disk: vec![0; partitions],
            routes: vec![u8::MAX; partitions],
        }
    }

    pub(super) fn host(&mut self) -> Host<'_> {
        Host {
            words: &self.words,
            rows: &self.rows,
            total: self.total,
            outer: &self.outer,
            disk: &self.disk,
            routes: &mut self.routes,
        }
    }

    pub(super) fn spill(&self) -> LocalSpill<'_> {
        LocalSpill::new(&self.words).expect("the level's words")
    }
}

/// A side of a level: each partition's chunks in memory, the last one
/// its tail, and its file, a list of blocks.
#[derive(Default)]
struct Held {
    chunks: Vec<Vec<Vec<Row>>>,
    files: Vec<Vec<Vec<Row>>>,
}

impl Held {
    fn of(partitions: usize) -> Self {
        Self {
            chunks: vec![Vec::new(); partitions],
            files: vec![Vec::new(); partitions],
        }
    }
}

/// A level of the spill: its machine, its regions and its sides.
struct Level {
    join: Join,
    regions: Regions,
    chunk_len: u64,
    outer_len: u64,
    shift: u32,
    inner: Held,
    outer: Held,
    on_disk: Vec<bool>,
    /// The bytes its chunks take in memory.
    memory: u64,
}

impl Level {
    fn of(join: Join, start: &Start) -> Self {
        let partitions = start.plan.partitions as usize;
        Self {
            join,
            regions: Regions::of(start),
            chunk_len: start.plan.chunk_len as u64,
            outer_len: start.outer_len as u64,
            shift: start.plan.shift,
            inner: Held::of(partitions),
            outer: Held::of(partitions),
            on_disk: vec![false; partitions],
            memory: 0,
        }
    }

    fn partition(&self, key: u32) -> usize {
        ((hash(key) >> self.shift) as usize) & (self.regions.rows.len() - 1)
    }

    fn step(&mut self, event: Event) -> Result<Action> {
        let mut host = self.regions.host();
        self.join.step(event, &mut host)
    }

    fn side(&mut self, side: Side) -> &mut Held {
        match side {
            Side::Inner => &mut self.inner,
            Side::Outer => &mut self.outer,
        }
    }

    /// Count a chunk of `len` bytes of an inner partition, or take it
    /// back: in the level's memory and in its words.
    fn count(&mut self, partition: usize, len: u64, add: bool) -> Result<()> {
        let delta = if add { len as i64 } else { -(len as i64) };
        self.memory = self.memory.wrapping_add_signed(delta);
        self.regions
            .spill()
            .add_bytes(delta, Some(partition as u32))?;
        Ok(())
    }

    /// Write a block of a side's partition to its file.
    fn write(&mut self, side: Side, partition: usize, rows: Vec<Row>, record: u64) {
        if rows.is_empty() {
            return;
        }
        if side == Side::Inner {
            self.regions.disk[partition] += rows.len() as u64 * record;
        }
        self.side(side).files[partition].push(rows);
    }
}

/// The host of a join in the tests.
pub(super) struct Model {
    params: Params,
    record: u64,
    /// The table while it fits, then the levels of the spill, the first
    /// at the bottom.
    table: Vec<Row>,
    levels: Vec<Level>,
    pub(super) pairs: Pairs,
    pub(super) ran: Counts,
}

impl Model {
    fn top(&mut self) -> &mut Level {
        self.levels.last_mut().expect("a level")
    }

    /// The memory of every level, as the host measures it.
    fn memory(&self) -> u64 {
        self.levels.iter().map(|level| level.memory).sum()
    }

    /// A row into its partition of a side of the top level, a chunk at a
    /// time.
    fn append(&mut self, side: Side, row: Row) -> Result<()> {
        let record = self.record;
        let level = self.top();
        let partition = level.partition(row.key);
        let (len, per) = match side {
            Side::Inner => (level.chunk_len, level.chunk_len / record),
            Side::Outer => (level.outer_len, level.outer_len / OUTER_ROW),
        };
        let tail = level.side(side).chunks[partition].last().map(Vec::len);
        if tail.is_none_or(|rows| rows as u64 >= per) {
            let event = Event::Full {
                side,
                partition: partition as u32,
                tail: tail.is_some(),
            };
            match level.step(event)? {
                Action::AddChunk => {
                    level.side(side).chunks[partition].push(Vec::new());
                    if side == Side::Inner {
                        level.count(partition, len, true)?;
                    } else {
                        level.memory += len;
                    }
                }
                Action::Flush => {
                    let rows = core::mem::take(
                        level.side(side).chunks[partition]
                            .last_mut()
                            .expect("a tail"),
                    );
                    level.write(side, partition, rows, record);
                    self.ran[Ran::FLUSHED] += 1;
                }
                action => panic!("{action:?} for a full partition"),
            }
        }
        let level = self.top();
        level.side(side).chunks[partition]
            .last_mut()
            .expect("a chunk")
            .push(row);
        match side {
            Side::Inner => {
                level.regions.rows[partition] += 1;
                level.regions.total += 1;
            }
            Side::Outer => level.regions.outer[partition] += 1,
        }
        Ok(())
    }

    /// Send partitions of the top level to disk as its machine says, from
    /// `event` on.
    fn send(&mut self, event: Event, limited: bool) -> Result<()> {
        let mut action = self.top().step(event)?;
        loop {
            let (partition, demote) = match action {
                Action::Done => {
                    if limited {
                        self.check_memory();
                    }
                    return Ok(());
                }
                Action::Demote(partition) => (partition as usize, true),
                Action::Evict(partition) => (partition as usize, false),
                action => panic!("{action:?} while sending"),
            };
            let record = self.record;
            let level = self.top();
            // A partition goes to disk once, and only one there is evicted.
            assert_eq!(level.on_disk[partition], !demote);
            let before = level.memory;
            let chunks = core::mem::take(&mut level.inner.chunks[partition]);
            let count = chunks.len();
            for (index, rows) in chunks.into_iter().enumerate() {
                level.write(Side::Inner, partition, rows, record);
                // A partition sent keeps its last chunk, emptied, as its tail.
                if demote && index + 1 == count {
                    level.inner.chunks[partition].push(Vec::new());
                } else {
                    let len = level.chunk_len;
                    level.count(partition, len, false)?;
                }
            }
            level.on_disk[partition] = true;
            let freed = level.memory < before || demote;
            self.ran[usize::from(!demote)] += 1;
            let memory = self.memory();
            action = self.top().step(Event::Sent { freed, memory })?;
        }
    }

    /// Past a check, the memory fits, or what is left is the tails of
    /// partitions on disk, which the rule weighs as nothing.
    fn check_memory(&self) {
        let level = self.levels.last().expect("a level");
        let on_disk = level.on_disk.iter().filter(|&&on| on).count() as u64;
        let held = self.memory() as f64 + level.join.reserve as f64 * on_disk as f64;
        let resident: usize = (0..level.on_disk.len())
            .filter(|&partition| !level.on_disk[partition])
            .map(|partition| level.inner.chunks[partition].len())
            .sum();
        let params = &self.params;
        assert!(
            held <= params.start.max(params.target) * params.limit as f64 || resident == 0,
            "{held} bytes past the limit with {resident} chunks in memory"
        );
    }

    /// Make the level the machine started and report it made.
    fn make(&mut self, join: Join, start: &Start) -> Result<()> {
        self.levels.push(Level::of(join, start));
        self.ran[Ran::DEEPEST] = self.ran[Ran::DEEPEST].max(self.levels.len());
        assert_eq!(self.top().step(Event::Made)?, Action::Done);
        Ok(())
    }

    /// Inner rows into the top level, then the check of its memory.
    fn build(&mut self, rows: &[Row]) -> Result<()> {
        for &row in rows {
            self.append(Side::Inner, row)?;
        }
        let memory = self.memory();
        self.send(Event::Grown { memory }, true)
    }

    /// The end of the top level's build: its inner rows, `rows` of them,
    /// all in memory or on disk once, and a route for each partition.
    fn end_build(&mut self, rows: &[u32]) -> Result<()> {
        self.send(Event::BuildEnd, false)?;
        let level = self.top();
        let mut kept: Vec<u32> = (level.inner.chunks.iter().flatten())
            .chain(level.inner.files.iter().flatten())
            .flatten()
            .map(|row| row.id)
            .collect();
        kept.sort_unstable();
        let mut expected = rows.to_vec();
        expected.sort_unstable();
        assert_eq!(kept, expected);
        for (partition, &route) in level.regions.routes.iter().enumerate() {
            let expected = if !level.on_disk[partition] {
                Route::Probe
            } else if level.regions.rows[partition] == 0 {
                Route::Answer
            } else {
                Route::Write
            };
            assert_eq!(route, expected as u8);
        }
        Ok(())
    }

    /// Outer rows through the top level's routes.
    fn probe(&mut self, rows: &[Row]) -> Result<()> {
        for &row in rows {
            let level = self.top();
            let partition = level.partition(row.key);
            let route = level.regions.routes[partition];
            let table: Vec<Row> = level.inner.chunks[partition].concat();
            self.ran[Ran::PROBED + usize::from(route)] += 1;
            if route == Route::Probe as u8 {
                pairs(&table, &[row], &mut self.pairs);
            } else if route == Route::Write as u8 {
                self.append(Side::Outer, row)?;
            }
        }
        Ok(())
    }

    /// The end of the top level's outer rows: what each partition on disk
    /// holds in memory is written, and the partitions in memory go.
    fn end_outer(&mut self) -> Result<()> {
        let record = self.record;
        let level = self.top();
        assert_eq!(level.step(Event::OuterEnd)?, Action::Done);
        for partition in 0..level.on_disk.len() {
            for side in [Side::Inner, Side::Outer] {
                let chunks = core::mem::take(&mut level.side(side).chunks[partition]);
                for rows in chunks {
                    if level.on_disk[partition] {
                        level.write(side, partition, rows, record);
                    }
                    if side == Side::Inner {
                        let len = level.chunk_len;
                        level.count(partition, len, false)?;
                    } else {
                        level.memory -= level.outer_len;
                    }
                }
            }
        }
        Ok(())
    }

    /// Join the partitions on disk of the top level as its machine says.
    fn join(&mut self) -> Result<()> {
        loop {
            let memory = self.memory();
            let action = self.top().step(Event::Next { memory })?;
            let partition = match action {
                Action::LevelDone => return Ok(()),
                Action::Release(partition) | Action::Open(partition) => partition as usize,
                Action::Split(below) => below.partition as usize,
                action => panic!("{action:?} for the next partition"),
            };
            let level = self.top();
            assert!(level.on_disk[partition], "partition {partition} in memory");
            let inner: Vec<Row> = core::mem::take(&mut level.inner.files[partition]).concat();
            let outer: Vec<Row> = core::mem::take(&mut level.outer.files[partition]).concat();
            assert_eq!(inner.len() as u64, level.regions.rows[partition]);
            assert_eq!(outer.len() as u64, level.regions.outer[partition]);
            match action {
                Action::Release(_) => {
                    // Nothing to join: no outer row, and no inner row to give out alone.
                    assert!(outer.is_empty());
                    self.ran[Ran::RELEASED] += 1;
                }
                Action::Open(_) => {
                    pairs(&inner, &outer, &mut self.pairs);
                    self.ran[Ran::OPENED] += 1;
                }
                Action::Split(below) => {
                    self.ran[Ran::SPLIT] += 1;
                    let (join, start) = Join::below(&self.params, &below)?;
                    assert_eq!(start.plan.shift, below.shift);
                    self.make(join, &start)?;
                    for block in inner.chunks(64) {
                        self.build(block)?;
                    }
                    self.top().regions.total = inner.len() as u64;
                    let ids: Vec<u32> = inner.iter().map(|row| row.id).collect();
                    self.end_build(&ids)?;
                    self.probe(&outer)?;
                    self.end_outer()?;
                    self.join()?;
                    let level = self.levels.pop().expect("the level below");
                    assert!(
                        level
                            .inner
                            .files
                            .iter()
                            .chain(&level.outer.files)
                            .all(Vec::is_empty),
                        "a level below left files"
                    );
                }
                _ => unreachable!(),
            }
        }
    }

    /// Join `inner` and `outer` through the machine, `batch` inner rows
    /// at a time: the pairs, and what ran.
    pub(super) fn run(
        params: &Params,
        inner: &[Row],
        outer: &[Row],
        batch: usize,
    ) -> Result<(Pairs, Counts)> {
        let mut model = Self {
            params: *params,
            record: params.shape.record as u64,
            table: Vec::new(),
            levels: Vec::new(),
            pairs: Vec::new(),
            ran: [0; 10],
        };
        let mut join = Some(Join::new(params)?);
        for rows in inner.chunks(batch) {
            if let Some(machine) = join.as_mut() {
                let held = model.table.len() as u64 * model.record;
                let event = Event::Inner {
                    held,
                    rows: model.table.len() as u64,
                    estimate: inner.len() as f64,
                    needed: held + 2 * rows.len() as u64 * model.record,
                };
                match machine.step(event, &mut Regions::none().host())? {
                    Action::Done => {
                        model.table.extend_from_slice(rows);
                        continue;
                    }
                    Action::Start(start) => {
                        let machine = join.take().expect("the machine");
                        model.make(machine, &start)?;
                        let table = core::mem::take(&mut model.table);
                        model.build(&table)?;
                    }
                    action => panic!("{action:?} before a batch"),
                }
            }
            model.build(rows)?;
        }
        if join.is_some() {
            pairs(&model.table, outer, &mut model.pairs);
            return Ok((model.pairs, model.ran));
        }
        let ids: Vec<u32> = inner.iter().map(|row| row.id).collect();
        model.end_build(&ids)?;
        model.probe(outer)?;
        model.end_outer()?;
        model.join()?;
        let level = model.levels.pop().expect("the first level");
        assert!(
            level
                .inner
                .files
                .iter()
                .chain(&level.outer.files)
                .all(Vec::is_empty),
            "the first level left files"
        );
        assert_eq!(level.memory, 0, "memory left");
        Ok((model.pairs, model.ran))
    }
}

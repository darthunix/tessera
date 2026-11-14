use core::cell::Cell;
use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;
use crate::Kind;
use crate::set::tests::property;

/// The rule of a wrong call's error, which a test expects.
#[track_caller]
fn wrong<T: core::fmt::Debug>(result: Result<T>) -> Rule {
    let error = result.expect_err("a wrong call");
    assert_eq!(error.kind(), Kind::WrongCall, "{error:?}");
    error.rule()
}

const RECORD: u64 = 32;

fn params(limit: u64) -> Params {
    Params {
        limit,
        shape: Shape {
            reserve: 1024 + 512,
            min_partitions: 4,
            max_partitions: 64,
            share: 8,
            min_chunk: 1024,
            max_chunk: 64 * 1024,
            record: RECORD as usize,
        },
        merged: RECORD + 16,
        fill: 0.875,
        start: 0.875,
        target: 0.5,
        spilled: 1.0,
        split_room: 1.0,
    }
}

/// A host's regions for a level, or for none.
struct Regions {
    words: Vec<Cell<u64>>,
    records: Vec<u64>,
    disk: Vec<u64>,
}

impl Regions {
    fn none() -> Self {
        Self {
            words: Vec::new(),
            records: Vec::new(),
            disk: Vec::new(),
        }
    }

    fn of(plan: &Plan) -> Self {
        let partitions = plan.partitions as usize;
        Self {
            words: (0..plan.words).map(|_| Cell::new(0)).collect(),
            records: vec![0; partitions],
            disk: vec![0; partitions],
        }
    }

    fn host(&self) -> Host<'_> {
        Host {
            words: &self.words,
            records: &self.records,
            disk: &self.disk,
        }
    }

    fn spill(&self) -> LocalSpill<'_> {
        LocalSpill::new(&self.words).expect("the level's words")
    }
}

/// A machine past its limit, its first level made.
fn spilling(limit: u64) -> (Group, Plan, Regions) {
    let mut group = Group::new(&params(limit)).expect("parameters");
    let inner = Event::Inner {
        bytes: limit,
        records: 1000,
        estimate: 1000.0,
        force: false,
    };
    let Ok(Action::Start(plan)) = group.step(inner, &Regions::none().host()) else {
        panic!("a start");
    };
    let regions = Regions::of(&plan);
    assert_eq!(group.step(Event::Made, &regions.host()), Ok(Action::Done));
    (group, plan, regions)
}

#[test]
fn a_table_spills_past_its_fill_or_when_asked() {
    let limit = 1 << 20;
    let mut group = Group::new(&params(limit)).expect("parameters");
    let before = group;
    let inner = |bytes, force| Event::Inner {
        bytes,
        records: 100,
        estimate: 1000.0,
        force,
    };
    let fill = (0.875 * limit as f64) as u64;
    assert_eq!(
        group.step(inner(fill, false), &Regions::none().host()),
        Ok(Action::Done)
    );
    assert_eq!(group, before);
    let Ok(Action::Start(plan)) = group.step(inner(fill + 1, false), &Regions::none().host())
    else {
        panic!("a start");
    };
    let expected = plan::plan(
        &params(limit).shape,
        &Level {
            expected: plan::expected((fill + 1) as f64, 100.0, 1000.0, 1),
            ..Level::first(limit as usize)
        },
    );
    assert_eq!(Ok(plan), expected);
    // A partial grouping asks for the spill whatever its bytes.
    let mut asked = Group::new(&params(limit)).expect("parameters");
    assert!(matches!(
        asked.step(inner(10, true), &Regions::none().host()),
        Ok(Action::Start(_))
    ));
    // An estimate of no group is an estimate still.
    let mut none = Group::new(&params(limit)).expect("parameters");
    let empty = Event::Inner {
        bytes: limit,
        records: 0,
        estimate: 0.0,
        force: false,
    };
    assert!(matches!(
        none.step(empty, &Regions::none().host()),
        Ok(Action::Start(_))
    ));
    // Words of another level are refused, the machine waiting still.
    let short = Regions {
        words: (0..crate::shared_spill::words_for(2).expect("words"))
            .map(|_| Cell::new(0))
            .collect(),
        ..Regions::of(&plan)
    };
    assert_eq!(
        wrong(group.step(Event::Made, &short.host())),
        Rule::SpillWords
    );
    assert_eq!(
        group.step(Event::Made, &Regions::of(&plan).host()),
        Ok(Action::Done)
    );
}

#[test]
fn an_event_out_of_turn_is_refused_and_changes_nothing() {
    let (mut group, _, regions) = spilling(1 << 20);
    for event in [
        Event::Made,
        Event::Next,
        Event::Sent { memory: 0 },
        Event::Weighed {
            groups: 0,
            others: 0,
        },
    ] {
        let before = group;
        assert_eq!(wrong(group.step(event, &regions.host())), Rule::JoinEvent);
        assert_eq!(group, before);
    }
    let mut none = Group { phase: 0, ..group };
    assert_eq!(
        wrong(none.step(Event::InputEnd, &regions.host())),
        Rule::JoinState
    );
    let full = Event::Full {
        partition: 64,
        tail: true,
    };
    assert_eq!(wrong(group.step(full, &regions.host())), Rule::Partition);
}

#[test]
fn the_first_level_keeps_its_chunks_and_a_level_below_writes_them() {
    let (mut group, plan, regions) = spilling(1 << 20);
    let full = |tail| Event::Full { partition: 1, tail };
    assert_eq!(
        group.step(full(false), &regions.host()),
        Ok(Action::AddChunk)
    );
    assert_eq!(
        group.step(full(true), &regions.host()),
        Ok(Action::AddChunk)
    );
    let below = Below {
        partition: 1,
        level: 1,
        expected: 4 << 20,
        shift: plan.below,
    };
    let (mut level, plan) = Group::below(&params(1 << 20), &below).expect("a level");
    assert_eq!(plan.shift, below.shift);
    let regions = Regions::of(&plan);
    assert_eq!(level.step(Event::Made, &regions.host()), Ok(Action::Done));
    assert_eq!(
        level.step(full(false), &regions.host()),
        Ok(Action::AddChunk)
    );
    assert_eq!(level.step(full(true), &regions.host()), Ok(Action::Flush));
    let first = Below { level: 0, ..below };
    assert_eq!(
        wrong(Group::below(&params(1 << 20), &first)),
        Rule::JoinLevel
    );
}

#[test]
fn the_largest_partitions_go_from_start_to_target() {
    let limit = 1 << 20;
    let (mut group, _, regions) = spilling(limit);
    for (partition, bytes) in [(0, 300_000), (1, 400_000), (2, 200_000), (3, 100_000)] {
        regions
            .spill()
            .add_bytes(bytes, Some(partition))
            .expect("bytes");
    }
    // Under seven eighths: nothing.
    let grown = |memory| Event::Grown { memory };
    assert_eq!(
        group.step(grown(900_000), &regions.host()),
        Ok(Action::Done)
    );
    // Past it, the largest, then down to half.
    assert_eq!(
        group.step(grown(1_000_000), &regions.host()),
        Ok(Action::Write(1))
    );
    regions.spill().add_bytes(-400_000, Some(1)).expect("bytes");
    let sent = |memory| Event::Sent { memory };
    assert_eq!(
        group.step(sent(600_000), &regions.host()),
        Ok(Action::Write(0))
    );
    regions.spill().add_bytes(-300_000, Some(0)).expect("bytes");
    assert_eq!(group.step(sent(300_000), &regions.host()), Ok(Action::Done));
}

#[test]
fn the_input_ends_with_writes_then_gives_out_in_memory_first() {
    let limit = 1 << 20;
    let (mut group, plan, mut regions) = spilling(limit);
    // Partitions 0 and 2 have records on disk; 0 and 1 in memory.
    regions.disk = vec![1000, 0, 2000, 0];
    regions.records = vec![10, 20, 0, 0];
    regions.spill().add_bytes(4096, Some(0)).expect("bytes");
    regions.spill().add_bytes(4096, Some(1)).expect("bytes");
    assert_eq!(
        group.step(Event::InputEnd, &regions.host()),
        Ok(Action::Write(0))
    );
    regions.spill().add_bytes(-4096, Some(0)).expect("bytes");
    regions.records[0] = 0;
    regions.disk[0] = 1500;
    assert_eq!(group.step(Event::Next, &regions.host()), Ok(Action::Done));
    // In memory alone first; then by weight.
    assert_eq!(
        group.step(Event::Next, &regions.host()),
        Ok(Action::Merge(1))
    );
    regions.records[1] = 0;
    assert_eq!(
        group.step(Event::Next, &regions.host()),
        Ok(Action::Weigh(0))
    );
    let weighed = |groups, others| Event::Weighed { groups, others };
    assert_eq!(
        group.step(weighed(100, 0), &regions.host()),
        Ok(Action::Merge(0))
    );
    assert_eq!(
        group.step(Event::Next, &regions.host()),
        Ok(Action::Weigh(2))
    );
    // Groups merged past what the limit leaves: a level below.
    let groups = limit / (RECORD + 16);
    let split = Below {
        partition: 2,
        level: 1,
        expected: 2000,
        shift: plan.below,
    };
    assert_eq!(
        group.step(weighed(groups, 0), &regions.host()),
        Ok(Action::Split(split))
    );
    assert_eq!(
        group.step(Event::Next, &regions.host()),
        Ok(Action::LevelDone)
    );
    assert_eq!(
        wrong(group.step(Event::Next, &regions.host())),
        Rule::JoinEvent
    );
}

#[test]
fn a_partition_weighs_what_the_others_take() {
    let limit = 1 << 20;
    let (mut group, _, mut regions) = spilling(limit);
    regions.disk = vec![1000, 0, 0, 0];
    assert_eq!(
        group.step(Event::InputEnd, &regions.host()),
        Ok(Action::Done)
    );
    assert_eq!(
        group.step(Event::Next, &regions.host()),
        Ok(Action::Weigh(0))
    );
    // A tenth of the limit merged, past what nine tenths in use leave.
    let groups = limit / 10 / (RECORD + 16);
    let weighed = Event::Weighed {
        groups,
        others: limit / 10 * 9,
    };
    let weighing = group;
    assert!(matches!(
        group.step(weighed, &regions.host()),
        Ok(Action::Split(_))
    ));
    // Words that are no machine: a cursor at no partition weighed, a
    // level with none below it.
    let broken = [
        Group {
            cursor: 0,
            ..weighing
        },
        Group {
            cursor: weighing.partitions + 1,
            ..weighing
        },
        Group {
            level: u32::MAX,
            ..weighing
        },
    ];
    for mut group in broken {
        let before = group;
        assert_eq!(wrong(group.step(weighed, &regions.host())), Rule::JoinState);
        assert_eq!(group, before);
    }
}

#[test]
fn a_refused_end_of_input_leaves_the_machine_building() {
    let (mut group, plan, regions) = spilling(1 << 20);
    let short = Regions {
        words: (0..crate::shared_spill::words_for(2).expect("words"))
            .map(|_| Cell::new(0))
            .collect(),
        ..Regions::of(&plan)
    };
    let building = group;
    assert_eq!(
        wrong(group.step(Event::InputEnd, &short.host())),
        Rule::SpillWords
    );
    assert_eq!(group, building);
    assert_eq!(
        group.step(Event::InputEnd, &regions.host()),
        Ok(Action::Done)
    );
}

#[test]
fn parameters_are_checked() {
    let good = params(1 << 20);
    assert!(Group::new(&good).is_ok());
    for bad in [
        Params { limit: 0, ..good },
        Params {
            fill: f64::NAN,
            ..good
        },
        Params {
            split_room: -1.0,
            ..good
        },
    ] {
        assert_eq!(wrong(Group::new(&bad)), Rule::JoinParams);
    }
}

/// The hash of a key, which picks its partition.
fn hash(key: u32) -> u32 {
    key.wrapping_mul(0x9e37_79b1).rotate_left(11)
}

/// A level of the model: its machine, regions, and each partition's
/// groups in memory, chunk by chunk, and its file of records.
struct ModelLevel {
    group: Group,
    regions: Regions,
    chunk_len: u64,
    shift: u32,
    chunks: Vec<Vec<Vec<(u32, u64)>>>,
    files: Vec<Vec<(u32, u64)>>,
    memory: u64,
}

impl ModelLevel {
    fn of(group: Group, plan: &Plan) -> Self {
        let partitions = plan.partitions as usize;
        Self {
            group,
            regions: Regions::of(plan),
            chunk_len: plan.chunk_len as u64,
            shift: plan.shift,
            chunks: vec![Vec::new(); partitions],
            files: vec![Vec::new(); partitions],
            memory: 0,
        }
    }

    fn partition(&self, key: u32) -> usize {
        ((hash(key) >> self.shift) as usize) & (self.chunks.len() - 1)
    }

    fn step(&mut self, event: Event) -> Result<Action> {
        self.group.step(event, &self.regions.host())
    }

    /// Write and free every chunk of a partition.
    fn write(&mut self, partition: usize) -> Result<()> {
        let chunks = core::mem::take(&mut self.chunks[partition]);
        for chunk in chunks {
            self.regions.disk[partition] += chunk.len() as u64 * RECORD;
            self.files[partition].extend(chunk);
            self.memory -= self.chunk_len;
            self.regions
                .spill()
                .add_bytes(-(self.chunk_len as i64), Some(partition as u32))?;
        }
        self.regions.records[partition] = 0;
        Ok(())
    }

    /// A new record of a group into its partition, a chunk at a time.
    fn append(&mut self, key: u32, count: u64) -> Result<()> {
        let partition = self.partition(key);
        let per = (self.chunk_len / RECORD) as usize;
        let tail = self.chunks[partition].last().map(Vec::len);
        if tail.is_none_or(|records| records >= per) {
            let event = Event::Full {
                partition: partition as u32,
                tail: tail.is_some(),
            };
            match self.step(event)? {
                Action::AddChunk => {
                    self.chunks[partition].push(Vec::new());
                    self.memory += self.chunk_len;
                    self.regions
                        .spill()
                        .add_bytes(self.chunk_len as i64, Some(partition as u32))?;
                }
                Action::Flush => {
                    let chunk =
                        core::mem::take(self.chunks[partition].last_mut().expect("a chunk"));
                    self.regions.disk[partition] += chunk.len() as u64 * RECORD;
                    self.regions.records[partition] -= chunk.len() as u64;
                    self.files[partition].extend(chunk);
                }
                action => panic!("{action:?} for a full partition"),
            }
        }
        self.chunks[partition]
            .last_mut()
            .expect("a chunk")
            .push((key, count));
        self.regions.records[partition] += 1;
        Ok(())
    }
}

/// The host of a grouping that counts its rows by key.
struct Model {
    params: Params,
    levels: Vec<ModelLevel>,
    counts: BTreeMap<u32, u64>,
    splits: usize,
    writes: usize,
}

impl Model {
    fn top(&mut self) -> &mut ModelLevel {
        self.levels.last_mut().expect("a level")
    }

    fn memory(&self) -> u64 {
        self.levels.iter().map(|level| level.memory).sum()
    }

    /// Send partitions to disk as the machine says, from `event` on.
    fn send(&mut self, event: Event) -> Result<()> {
        let mut action = self.top().step(event)?;
        loop {
            match action {
                Action::Done => return Ok(()),
                Action::Write(partition) => {
                    self.writes += 1;
                    self.top().write(partition as usize)?;
                    let memory = self.memory();
                    action = self.top().step(Event::Sent { memory })?;
                }
                action => panic!("{action:?} while sending"),
            }
        }
    }

    /// A row of the first level: its group's record in memory, or a new
    /// one.
    fn add(&mut self, key: u32) -> Result<()> {
        let level = self.top();
        let partition = level.partition(key);
        if let Some(record) = level.chunks[partition]
            .iter_mut()
            .flatten()
            .find(|(of, _)| *of == key)
        {
            record.1 += 1;
            return Ok(());
        }
        level.append(key, 1)
    }

    /// Flush, then give the top level out as its machine says.
    fn give(&mut self) -> Result<()> {
        let mut action = self.top().step(Event::InputEnd)?;
        while let Action::Write(partition) = action {
            self.top().write(partition as usize)?;
            action = self.top().step(Event::Next)?;
        }
        assert_eq!(action, Action::Done);
        loop {
            let partition = match self.top().step(Event::Next)? {
                Action::LevelDone => return Ok(()),
                Action::Merge(partition) => partition as usize,
                Action::Weigh(partition) => {
                    let level = self.top();
                    let index = partition as usize;
                    let mut keys: Vec<u32> = (level.chunks[index].iter().flatten())
                        .chain(&level.files[index])
                        .map(|(key, _)| *key)
                        .collect();
                    keys.sort_unstable();
                    keys.dedup();
                    let others = self.memory()
                        - self.levels.last().map_or(0, |level| {
                            level.chunks[index].len() as u64 * level.chunk_len
                        });
                    let weighed = Event::Weighed {
                        groups: keys.len() as u64,
                        others,
                    };
                    match self.top().step(weighed)? {
                        Action::Merge(partition) => partition as usize,
                        Action::Split(below) => {
                            self.split(below)?;
                            continue;
                        }
                        action => panic!("{action:?} for a weighed partition"),
                    }
                }
                action => panic!("{action:?} for the next partition"),
            };
            let level = self.top();
            let records: Vec<(u32, u64)> = (level.chunks[partition].iter().flatten())
                .chain(&level.files[partition])
                .copied()
                .collect();
            let mut merged: BTreeMap<u32, u64> = BTreeMap::new();
            for (key, count) in records {
                *merged.entry(key).or_default() += count;
            }
            for (key, count) in merged {
                // Each group once, all of its rows merged.
                assert!(
                    self.counts.insert(key, count).is_none(),
                    "group {key} twice"
                );
            }
            let level = self.top();
            for _ in 0..level.chunks[partition].len() {
                level.memory -= level.chunk_len;
                level
                    .regions
                    .spill()
                    .add_bytes(-(level.chunk_len as i64), Some(partition as u32))?;
            }
            level.chunks[partition].clear();
            level.files[partition].clear();
            level.regions.records[partition] = 0;
            level.regions.disk[partition] = 0;
        }
    }

    /// Split a partition of the top level into a level below, and give
    /// that out.
    fn split(&mut self, below: Below) -> Result<()> {
        self.splits += 1;
        let level = self.top();
        let index = below.partition as usize;
        let records: Vec<(u32, u64)> = (level.files[index].iter())
            .chain(level.chunks[index].iter().flatten())
            .copied()
            .collect();
        for _ in 0..level.chunks[index].len() {
            level.memory -= level.chunk_len;
            level
                .regions
                .spill()
                .add_bytes(-(level.chunk_len as i64), Some(below.partition))?;
        }
        level.chunks[index].clear();
        level.files[index].clear();
        level.regions.records[index] = 0;
        level.regions.disk[index] = 0;
        let (group, plan) = Group::below(&self.params, &below)?;
        self.levels.push(ModelLevel::of(group, &plan));
        assert_eq!(self.top().step(Event::Made)?, Action::Done);
        for (key, count) in records {
            self.top().append(key, count)?;
        }
        self.give()?;
        let level = self.levels.pop().expect("the level below");
        assert!(level.files.iter().all(Vec::is_empty) && level.memory == 0);
        Ok(())
    }

    /// Count `keys` through the machine, `batch` rows at a time.
    fn run(
        params: &Params,
        keys: &[u32],
        batch: usize,
    ) -> Result<(BTreeMap<u32, u64>, usize, usize)> {
        let mut model = Self {
            params: *params,
            levels: Vec::new(),
            counts: BTreeMap::new(),
            splits: 0,
            writes: 0,
        };
        let mut table: BTreeMap<u32, u64> = BTreeMap::new();
        let mut machine = Some(Group::new(params)?);
        for rows in keys.chunks(batch) {
            if let Some(group) = machine.as_mut() {
                for &key in rows {
                    *table.entry(key).or_default() += 1;
                }
                let inner = Event::Inner {
                    bytes: table.len() as u64 * RECORD,
                    records: table.len() as u64,
                    estimate: keys.len() as f64,
                    force: false,
                };
                match group.step(inner, &Regions::none().host())? {
                    Action::Done => continue,
                    Action::Start(plan) => {
                        let group = machine.take().expect("the machine");
                        model.levels.push(ModelLevel::of(group, &plan));
                        assert_eq!(model.top().step(Event::Made)?, Action::Done);
                        for (key, count) in core::mem::take(&mut table) {
                            model.top().append(key, count)?;
                        }
                        let memory = model.memory();
                        model.send(Event::Grown { memory })?;
                        continue;
                    }
                    action => panic!("{action:?} after a batch"),
                }
            }
            for &key in rows {
                model.add(key)?;
            }
            let memory = model.memory();
            model.send(Event::Grown { memory })?;
        }
        if machine.is_some() {
            return Ok((table, 0, 0));
        }
        model.give()?;
        let level = model.levels.pop().expect("the first level");
        assert!(level.files.iter().all(Vec::is_empty) && level.memory == 0);
        Ok((model.counts, model.splits, model.writes))
    }
}

/// Count through the machine and check the counts against a grouping
/// in memory: the splits and the writes.
fn ran(params: &Params, keys: &[u32], batch: usize) -> Result<(usize, usize)> {
    let (got, splits, writes) = Model::run(params, keys, batch)?;
    let mut expected = BTreeMap::new();
    for &key in keys {
        *expected.entry(key).or_insert(0_u64) += 1;
    }
    assert_eq!(got, expected);
    Ok((splits, writes))
}

#[test]
fn a_grouping_through_the_machine_gives_the_groups_of_one_in_memory() {
    let strategy = (
        prop::collection::vec(0_u32..3000, 0..4000),
        4_u64..64,
        1_usize..128,
        (0.5_f64..=1.0, 0.25_f64..0.75, 0.0_f64..=1.0, 0.25_f64..2.0),
    );
    property(
        strategy,
        |(keys, kib, batch, (start, target, spilled, room))| {
            let params = Params {
                start,
                target,
                spilled,
                split_room: room,
                ..params(kib * 1024)
            };
            ran(&params, &keys, batch)?;
            Ok(())
        },
    );
}

#[test]
fn a_grouping_that_spills_writes_and_splits() -> Result<()> {
    let keys: Vec<u32> = (0..20_000).map(|row| row * 7919 % 9000).collect();
    let (splits, writes) = ran(&params(16 * 1024), &keys, 64)?;
    assert!(splits > 0 && writes > 0, "{splits} splits, {writes} writes");
    Ok(())
}

#[test]
fn no_partition_splits_where_no_level_of_the_shape_fits() {
    // Eight partitions at the least: three bits, of which two are left.
    let limit = 1 << 20;
    let mut params = params(limit);
    params.shape.min_partitions = 8;
    let below = Below {
        partition: 1,
        level: 1,
        expected: 1000,
        shift: 27,
    };
    let (mut group, plan) = Group::below(&params, &below).expect("a level");
    assert!(!plan.deeper && plan.below == 30);
    let mut regions = Regions::of(&plan);
    assert_eq!(group.step(Event::Made, &regions.host()), Ok(Action::Done));
    regions.disk[1] = 1000;
    assert_eq!(
        group.step(Event::InputEnd, &regions.host()),
        Ok(Action::Done)
    );
    assert_eq!(
        group.step(Event::Next, &regions.host()),
        Ok(Action::Weigh(1))
    );
    let weighed = Event::Weighed {
        groups: limit,
        others: 0,
    };
    assert_eq!(group.step(weighed, &regions.host()), Ok(Action::Merge(1)));
}

#[test]
fn a_partition_splits_only_with_two_bits_left_past_its_level() {
    // Two partitions at the least: a level of one bit fits below shift
    // 31, but a split asks for two bits past the level's own.
    let limit = 1 << 20;
    let mut params = params(limit);
    params.shape.min_partitions = 2;
    let below = Below {
        partition: 1,
        level: 1,
        expected: 1000,
        shift: 30,
    };
    let (mut group, plan) = Group::below(&params, &below).expect("a level");
    assert!(plan.deeper && plan.below == 31, "{plan:?}");
    let mut regions = Regions::of(&plan);
    assert_eq!(group.step(Event::Made, &regions.host()), Ok(Action::Done));
    regions.disk[1] = 1000;
    assert_eq!(
        group.step(Event::InputEnd, &regions.host()),
        Ok(Action::Done)
    );
    assert_eq!(
        group.step(Event::Next, &regions.host()),
        Ok(Action::Weigh(1))
    );
    let weighed = Event::Weighed {
        groups: limit,
        others: 0,
    };
    assert_eq!(group.step(weighed, &regions.host()), Ok(Action::Merge(1)));
}

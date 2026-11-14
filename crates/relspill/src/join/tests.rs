use core::cell::Cell;

use proptest::prelude::*;

use super::*;
use crate::Kind;
use crate::set::tests::property;
use crate::shared_spill::words_for;

/// The rule of a wrong call's error, which a test expects.
#[track_caller]
fn wrong<T: core::fmt::Debug>(result: Result<T>) -> Rule {
    let error = result.expect_err("a wrong call");
    assert_eq!(error.kind(), Kind::WrongCall, "{error:?}");
    error.rule()
}

fn params(limit: u64) -> Params {
    Params {
        limit,
        shape: Shape {
            reserve: 4 * 1024,
            min_partitions: 4,
            max_partitions: 64,
            share: 16,
            min_chunk: 1024,
            max_chunk: 64 * 1024,
            record: 32,
        },
        outer_words: 2,
        outer_values: 0,
        spare: 0,
        page: 512,
        start: 1.0,
        target: 1.0,
        spilled: 0.0,
        resident: 0.25,
        tail: 1.0,
    }
}

/// A host's regions for a level of `partitions`, or for none.
struct Regions {
    words: Vec<Cell<u64>>,
    rows: Vec<u64>,
    total: u64,
    routes: Vec<u8>,
}

impl Regions {
    fn none() -> Self {
        Self {
            words: Vec::new(),
            rows: Vec::new(),
            total: 0,
            routes: Vec::new(),
        }
    }

    fn of(start: &Start) -> Self {
        let partitions = start.plan.partitions as usize;
        Self {
            words: (0..start.plan.words).map(|_| Cell::new(0)).collect(),
            rows: vec![0; partitions],
            total: 0,
            routes: vec![u8::MAX; partitions],
        }
    }

    fn host(&mut self) -> Host<'_> {
        Host {
            words: &self.words,
            rows: &self.rows,
            total: self.total,
            routes: &mut self.routes,
        }
    }

    fn spill(&self) -> LocalSpill<'_> {
        LocalSpill::new(&self.words).expect("the level's words")
    }
}

/// A machine past the limit, its level made: the machine, its level and
/// its regions.
fn spilling(limit: u64) -> (Join, Start, Regions) {
    let mut join = Join::new(&params(limit)).expect("parameters");
    let inner = Event::Inner {
        held: limit,
        rows: 1000,
        estimate: 1000.0,
        needed: limit + 1,
    };
    let Ok(Action::Start(start)) = join.step(inner, &mut Regions::none().host()) else {
        panic!("a start");
    };
    let mut regions = Regions::of(&start);
    assert_eq!(
        join.step(Event::Made, &mut regions.host()),
        Ok(Action::Done)
    );
    (join, start, regions)
}

#[test]
fn a_table_that_fits_takes_its_batch() {
    let mut join = Join::new(&params(1 << 20)).expect("parameters");
    let before = join;
    let inner = Event::Inner {
        held: 1000,
        rows: 10,
        estimate: 0.0,
        needed: 1 << 20,
    };
    assert_eq!(
        join.step(inner, &mut Regions::none().host()),
        Ok(Action::Done)
    );
    assert_eq!(join, before);
}

#[test]
fn past_the_limit_the_first_level_is_planned_and_made() {
    let limit = 1 << 20;
    let mut join = Join::new(&params(limit)).expect("parameters");
    let inner = Event::Inner {
        held: 600_000,
        rows: 3000,
        estimate: 30_000.0,
        needed: limit + 1,
    };
    let Ok(Action::Start(start)) = join.step(inner, &mut Regions::none().host()) else {
        panic!("a start");
    };
    let expected = plan::plan(
        &params(limit).shape,
        &Level {
            expected: plan::expected(600_000.0, 3000.0, 30_000.0, 1),
            ..Level::first(limit as usize)
        },
    );
    assert_eq!(Ok(start.plan), expected);
    assert_eq!((join.level(), start.plan.shift), (0, 0));
    // A row of 2 words and its lane of NULL bits, four of them at least.
    assert_eq!(
        start.outer_len,
        start.plan.chunk_len.max(columns::HEADER + 8 * 4 * 3)
    );
    // Words of another level are refused, and the machine waits still.
    let mut regions = Regions::of(&start);
    let mut short = Regions {
        words: (0..words_for(2).expect("words"))
            .map(|_| Cell::new(0))
            .collect(),
        ..Regions::of(&start)
    };
    assert_eq!(
        wrong(join.step(Event::Made, &mut short.host())),
        Rule::SpillWords
    );
    assert_eq!(
        join.step(Event::Made, &mut regions.host()),
        Ok(Action::Done)
    );
    assert_eq!(regions.spill().partitions(), start.plan.partitions);
}

#[test]
fn an_event_out_of_turn_is_refused_and_changes_nothing() {
    let (mut join, _, mut regions) = spilling(1 << 20);
    let events = [
        Event::Inner {
            held: 0,
            rows: 0,
            estimate: 0.0,
            needed: u64::MAX,
        },
        Event::Made,
        Event::Full {
            side: Side::Outer,
            partition: 0,
            tail: true,
        },
        Event::Sent {
            freed: true,
            memory: 0,
        },
        Event::OuterEnd,
    ];
    for event in events {
        let before = join;
        assert_eq!(
            wrong(join.step(event, &mut regions.host())),
            Rule::JoinEvent
        );
        assert_eq!(join, before);
    }
    // Memory that is no machine.
    let mut none = Join { phase: 0, ..join };
    assert_eq!(
        wrong(none.step(Event::BuildEnd, &mut regions.host())),
        Rule::JoinState
    );
    let mut fits = Join::new(&params(1 << 20)).expect("parameters");
    assert_eq!(
        wrong(fits.step(Event::BuildEnd, &mut Regions::none().host())),
        Rule::JoinEvent
    );
}

#[test]
fn a_full_partition_takes_a_chunk_in_memory_and_writes_on_disk() {
    let (mut join, start, mut regions) = spilling(1 << 20);
    let full = |partition, tail| Event::Full {
        side: Side::Inner,
        partition,
        tail,
    };
    // In memory, with a chunk or not: another chunk.
    assert_eq!(
        join.step(full(1, true), &mut regions.host()),
        Ok(Action::AddChunk)
    );
    assert_eq!(
        join.step(full(1, false), &mut regions.host()),
        Ok(Action::AddChunk)
    );
    // On disk: its tail goes, or it takes one.
    regions.rows[1] = 10;
    regions.total = 10;
    regions.spill().add_bytes(1 << 20, Some(1)).expect("bytes");
    assert_eq!(
        join.step(Event::Grown { memory: 2 << 20 }, &mut regions.host()),
        Ok(Action::Demote(1))
    );
    let sent = Event::Sent {
        freed: true,
        memory: 0,
    };
    assert_eq!(join.step(sent, &mut regions.host()), Ok(Action::Done));
    assert_eq!(
        join.step(full(1, true), &mut regions.host()),
        Ok(Action::Flush)
    );
    assert_eq!(
        join.step(full(1, false), &mut regions.host()),
        Ok(Action::AddChunk)
    );
    // Past the level's partitions.
    let past = full(start.plan.partitions, true);
    assert_eq!(wrong(join.step(past, &mut regions.host())), Rule::Partition);
}

#[test]
fn the_rule_sends_the_largest_until_the_memory_fits() {
    let (mut join, start, mut regions) = spilling(1 << 20);
    let partitions = start.plan.partitions;
    for (partition, bytes) in (0..partitions).zip([300_000_i64, 500_000, 200_000, 100_000]) {
        regions
            .spill()
            .add_bytes(bytes, Some(partition))
            .expect("bytes");
        regions.rows[partition as usize] = bytes as u64 / 100;
    }
    regions.total = regions.rows.iter().sum();
    let memory = 1_100_000;
    assert_eq!(
        join.step(Event::Grown { memory }, &mut regions.host()),
        Ok(Action::Demote(1))
    );
    // The host wrote it: what it frees counts.
    regions.spill().add_bytes(-500_000, Some(1)).expect("bytes");
    let sent = |memory| Event::Sent {
        freed: true,
        memory,
    };
    // Within the limit, with the reserve of the partition on disk.
    assert!(600_000 + join.reserve < 1 << 20);
    assert_eq!(
        join.step(sent(600_000), &mut regions.host()),
        Ok(Action::Done)
    );
    assert_eq!(
        join.step(Event::Grown { memory: 2 << 20 }, &mut regions.host()),
        Ok(Action::Demote(0))
    );
    // A send that frees nothing ends the check.
    let nothing = Event::Sent {
        freed: false,
        memory: 2 << 20,
    };
    assert_eq!(join.step(nothing, &mut regions.host()), Ok(Action::Done));
}

#[test]
fn the_build_ends_with_where_the_outer_rows_go() {
    let (mut join, start, mut regions) = spilling(1 << 20);
    assert_eq!(start.plan.partitions, 4);
    regions.rows = vec![3000, 2000, 0, 10];
    regions.total = 5010;
    // Partitions 1 and 2 go to disk while the build goes on.
    regions.spill().add_bytes(2 << 20, Some(1)).expect("bytes");
    regions.spill().add_bytes(3 << 19, Some(2)).expect("bytes");
    assert_eq!(
        join.step(Event::Grown { memory: 4 << 20 }, &mut regions.host()),
        Ok(Action::Demote(1))
    );
    let sent = |memory| Event::Sent {
        freed: true,
        memory,
    };
    assert_eq!(
        join.step(sent(3 << 19), &mut regions.host()),
        Ok(Action::Demote(2))
    );
    assert_eq!(join.step(sent(0), &mut regions.host()), Ok(Action::Done));
    // Routes of another count are refused before anything is sent.
    let mut short = Regions {
        words: regions.words.clone(),
        rows: regions.rows.clone(),
        total: regions.total,
        routes: vec![0; 2],
    };
    let before = join;
    assert_eq!(
        wrong(join.step(Event::BuildEnd, &mut short.host())),
        Rule::JoinRoutes
    );
    assert_eq!(join, before);
    // The partitions in memory hold enough of the rows: they stay.
    assert_eq!(
        join.step(Event::BuildEnd, &mut regions.host()),
        Ok(Action::Done)
    );
    let routes = [Route::Probe, Route::Write, Route::Answer, Route::Probe];
    assert_eq!(regions.routes, routes.map(|route| route as u8));
    // The outer rows written: their tails go when full; then the end.
    let outer = |tail| Event::Full {
        side: Side::Outer,
        partition: 1,
        tail,
    };
    assert_eq!(
        join.step(outer(false), &mut regions.host()),
        Ok(Action::AddChunk)
    );
    assert_eq!(
        join.step(outer(true), &mut regions.host()),
        Ok(Action::Flush)
    );
    assert_eq!(
        join.step(Event::OuterEnd, &mut regions.host()),
        Ok(Action::Done)
    );
    assert_eq!(
        wrong(join.step(outer(true), &mut regions.host())),
        Rule::JoinEvent
    );
}

#[test]
fn few_rows_left_in_memory_go_to_disk_at_the_end() {
    let (mut join, _, mut regions) = spilling(1 << 20);
    regions.rows = vec![100, 10, 0, 50];
    regions.total = 160;
    regions.spill().add_bytes(2 << 20, Some(1)).expect("bytes");
    assert_eq!(
        join.step(Event::Grown { memory: 2 << 20 }, &mut regions.host()),
        Ok(Action::Demote(1))
    );
    let sent = Event::Sent {
        freed: true,
        memory: 0,
    };
    assert_eq!(join.step(sent, &mut regions.host()), Ok(Action::Done));
    // The last rows went to the partition on disk: a quarter of the rows
    // is the least the partitions in memory keep.
    regions.rows[1] = 2000;
    regions.total = 2150;
    assert_eq!(
        join.step(Event::BuildEnd, &mut regions.host()),
        Ok(Action::Demote(0))
    );
    assert_eq!(join.step(sent, &mut regions.host()), Ok(Action::Demote(2)));
    assert_eq!(join.step(sent, &mut regions.host()), Ok(Action::Demote(3)));
    assert_eq!(join.step(sent, &mut regions.host()), Ok(Action::Done));
    let routes = [Route::Write, Route::Write, Route::Answer, Route::Write];
    assert_eq!(regions.routes, routes.map(|route| route as u8));
}

#[test]
fn a_level_below_is_planned_from_its_bits() {
    let limit = 1 << 20;
    let (join, start) =
        Join::below(&params(limit), 1, 4.0 * limit as f64, limit / 2, 2).expect("a level below");
    assert_eq!((join.level(), start.plan.shift), (1, 2));
    assert_eq!(join.phase, PLANNED);
    assert_eq!(
        wrong(Join::below(&params(limit), 0, 0.0, limit, 2)),
        Rule::JoinLevel
    );
    assert_eq!(
        wrong(Join::below(&params(limit), 1, 0.0, limit, 40)),
        Rule::PlanBits
    );
}

#[test]
fn a_shared_level_takes_the_partitions_given() {
    let limit = 1 << 20;
    let start = Join::given(&params(limit), 16).expect("a level");
    assert_eq!((start.plan.partitions, start.plan.shift), (16, 0));
    let planned = plan::plan(
        &params(limit).shape,
        &Level {
            partitions: 16,
            ..Level::first(limit as usize)
        },
    );
    assert_eq!(Ok(start.plan), planned);
    assert_eq!(wrong(Join::given(&params(limit), 3)), Rule::GivenPartitions);
}

#[test]
fn parameters_are_checked() {
    let good = params(1 << 20);
    assert!(Join::new(&good).is_ok());
    for bad in [
        Params { limit: 0, ..good },
        Params { spare: 1, ..good },
        Params {
            start: f64::NAN,
            ..good
        },
        Params { tail: -1.0, ..good },
        Params {
            resident: f64::INFINITY,
            ..good
        },
    ] {
        assert_eq!(wrong(Join::new(&bad)), Rule::JoinParams);
    }
    let wide = Params {
        outer_words: columns::MAX_WORDS as u64 + 1,
        ..good
    };
    assert_eq!(wrong(Join::new(&wide)), Rule::ColumnsWords);
    let shape = Shape {
        share: 0,
        ..good.shape
    };
    assert_eq!(wrong(Join::new(&Params { shape, ..good })), Rule::Share);
}

/// A machine past the limit of `params`, its level made.
fn spilling_with(params: &Params) -> (Join, Start, Regions) {
    let mut join = Join::new(params).expect("parameters");
    let inner = Event::Inner {
        held: params.limit,
        rows: 1000,
        estimate: 1000.0,
        needed: params.limit + 1,
    };
    let Ok(Action::Start(start)) = join.step(inner, &mut Regions::none().host()) else {
        panic!("a start");
    };
    let mut regions = Regions::of(&start);
    assert_eq!(
        join.step(Event::Made, &mut regions.host()),
        Ok(Action::Done)
    );
    (join, start, regions)
}

/// The host sent a partition: what its words count of it goes, and the
/// machine hears of it.
fn sent(join: &mut Join, regions: &mut Regions, partition: u32, bytes: i64, memory: u64) -> Action {
    regions
        .spill()
        .add_bytes(-bytes, Some(partition))
        .expect("bytes");
    let sent = Event::Sent {
        freed: true,
        memory,
    };
    join.step(sent, &mut regions.host()).expect("a step")
}

#[test]
fn the_weights_set_the_reserve_and_the_shares_of_a_check() {
    let limit = 1 << 20;
    // Wide outer rows that keep values: chunks of four of them at least.
    let params = Params {
        start: 0.9,
        target: 0.7,
        tail: 0.5,
        outer_values: 1,
        outer_words: 1000,
        ..params(limit)
    };
    let (mut join, start, mut regions) = spilling_with(&params);
    let outer_len = columns::HEADER + 8 * 4 * columns::lanes(1000);
    assert!(outer_len > start.plan.chunk_len);
    assert_eq!(start.outer_len, outer_len);
    // A chunk of outer rows, one of their values and a page, half of it.
    let reserve = ((2 * outer_len + 512) as f64 * 0.5) as u64;
    assert_eq!(join.reserve, reserve);
    for (partition, bytes) in [(0, 400_000), (1, 300_000), (2, 200_000), (3, 100_000)] {
        regions
            .spill()
            .add_bytes(bytes, Some(partition))
            .expect("bytes");
        regions.rows[partition as usize] = 1000;
    }
    regions.total = 4000;
    // The first send past start of the limit, the next past target.
    let grown = Event::Grown { memory: 1_000_000 };
    assert!(1_000_000.0 > 0.9 * limit as f64);
    assert_eq!(join.step(grown, &mut regions.host()), Ok(Action::Demote(0)));
    let memory = 800_000;
    assert!(((memory + reserve) as f64) < 0.9 * limit as f64);
    assert!(((memory + reserve) as f64) > 0.7 * limit as f64);
    assert_eq!(
        sent(&mut join, &mut regions, 0, 400_000, memory),
        Action::Demote(1)
    );
    assert_eq!(
        sent(&mut join, &mut regions, 1, 300_000, 500_000),
        Action::Done
    );
}

#[test]
fn the_end_of_the_build_weighs_rows_not_memory() {
    // A small limit that the reserves of three partitions on disk pass.
    let limit = 64 * 1024;
    let params = Params {
        start: 0.5,
        target: 0.5,
        tail: 10.0,
        ..params(limit)
    };
    let (mut join, _, mut regions) = spilling_with(&params);
    assert!(3 * join.reserve > limit / 2);
    regions.rows = vec![1000, 300, 200, 100];
    regions.total = 1600;
    for (partition, bytes) in [(0, 100), (1, 30_000), (2, 20_000), (3, 10_000)] {
        regions
            .spill()
            .add_bytes(bytes, Some(partition))
            .expect("bytes");
    }
    let grown = Event::Grown { memory: 100_000 };
    assert_eq!(join.step(grown, &mut regions.host()), Ok(Action::Demote(1)));
    assert_eq!(
        sent(&mut join, &mut regions, 1, 30_000, 70_000),
        Action::Demote(2)
    );
    assert_eq!(
        sent(&mut join, &mut regions, 2, 20_000, 50_000),
        Action::Demote(3)
    );
    // A send that freed nothing ends the check, the reserves still past.
    regions.spill().add_bytes(-10_000, Some(3)).expect("bytes");
    let nothing = Event::Sent {
        freed: false,
        memory: 100,
    };
    assert_eq!(join.step(nothing, &mut regions.host()), Ok(Action::Done));
    // The end weighs the rows alone: partition 0 holds enough of them.
    assert_eq!(
        join.step(Event::BuildEnd, &mut regions.host()),
        Ok(Action::Done)
    );
    assert_eq!(regions.routes, [0, 1, 1, 1]);
}

#[test]
fn a_level_weighs_the_rows_its_host_counts_for_it() {
    // A level below knows its rows before its partitions hold them all.
    let (mut join, _, mut regions) = spilling(1 << 20);
    regions.rows = vec![100, 0, 0, 0];
    regions.total = 10_000;
    regions.spill().add_bytes(2 << 20, Some(1)).expect("bytes");
    let grown = Event::Grown { memory: 2 << 20 };
    assert_eq!(join.step(grown, &mut regions.host()), Ok(Action::Demote(1)));
    // The partitions in memory hold 100 of the 10 000 rows: they go too.
    assert_eq!(
        sent(&mut join, &mut regions, 1, 2 << 20, 0),
        Action::Demote(0)
    );
}

/// A row of a side: its key and its number among the side's rows.
#[derive(Clone, Copy, Debug)]
struct Row {
    key: u32,
    id: u32,
}

/// The hash of a key, which picks its partition.
fn hash(key: u32) -> u32 {
    key.wrapping_mul(0x9e37_79b1).rotate_left(7)
}

/// A side of a level in the test's host: each partition's chunks in
/// memory, the last one its tail, and its file, a list of blocks.
#[derive(Default)]
struct Held {
    chunks: Vec<Vec<Vec<Row>>>,
    files: Vec<Vec<Vec<Row>>>,
}

/// The host of a join in the test: memory is a count of bytes, a file is
/// a vector of blocks, and the rows of a chunk are as many as its bytes
/// hold.
struct Model {
    join: Join,
    record: u64,
    chunk_len: u64,
    outer_len: u64,
    shift: u32,
    table: Vec<Row>,
    regions: Regions,
    inner: Held,
    outer: Held,
    memory: u64,
    on_disk: Vec<bool>,
    pairs: Pairs,
    ran: Ran,
}

impl Model {
    fn partition(&self, key: u32) -> usize {
        ((hash(key) >> self.shift) as usize) & (self.regions.rows.len() - 1)
    }

    fn step(&mut self, event: Event) -> Result<Action> {
        let mut host = self.regions.host();
        self.join.step(event, &mut host)
    }

    /// Count a chunk of `len` bytes of a partition of the inner side, or
    /// take it back.
    fn count(&mut self, partition: usize, len: u64, add: bool) -> Result<()> {
        let delta = if add { len as i64 } else { -(len as i64) };
        self.memory = self.memory.wrapping_add_signed(delta);
        self.regions
            .spill()
            .add_bytes(delta, Some(partition as u32))?;
        Ok(())
    }

    /// A row into its partition of a side, a chunk at a time.
    fn append(&mut self, side: Side, row: Row) -> Result<()> {
        let partition = self.partition(row.key);
        let (len, per) = match side {
            Side::Inner => (self.chunk_len, self.chunk_len / self.record),
            Side::Outer => (self.outer_len, self.outer_len / 24),
        };
        let held = match side {
            Side::Inner => &self.inner,
            Side::Outer => &self.outer,
        };
        let tail = held.chunks[partition].last().map(Vec::len);
        if tail.is_none_or(|rows| rows as u64 >= per) {
            let event = Event::Full {
                side,
                partition: partition as u32,
                tail: tail.is_some(),
            };
            match self.step(event)? {
                Action::AddChunk => {
                    self.side(side).chunks[partition].push(Vec::new());
                    if side == Side::Inner {
                        self.count(partition, len, true)?;
                    } else {
                        self.memory += len;
                    }
                }
                Action::Flush => {
                    self.ran[2] += 1;
                    let side = self.side(side);
                    let rows = core::mem::take(side.chunks[partition].last_mut().expect("a tail"));
                    side.files[partition].push(rows);
                }
                action => panic!("{action:?} for a full partition"),
            }
        }
        self.side(side).chunks[partition]
            .last_mut()
            .expect("a chunk")
            .push(row);
        if side == Side::Inner {
            self.regions.rows[partition] += 1;
            self.regions.total += 1;
        }
        Ok(())
    }

    fn side(&mut self, side: Side) -> &mut Held {
        match side {
            Side::Inner => &mut self.inner,
            Side::Outer => &mut self.outer,
        }
    }

    /// Send partitions to disk as the machine says, from `event` on.
    fn send(&mut self, event: Event, limited: bool) -> Result<()> {
        let mut action = self.step(event)?;
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
            // A partition goes to disk once, and only one there is evicted.
            assert_eq!(self.on_disk[partition], !demote);
            let before = self.memory;
            let chunks = core::mem::take(&mut self.inner.chunks[partition]);
            let count = chunks.len();
            for (index, rows) in chunks.into_iter().enumerate() {
                if !rows.is_empty() {
                    self.inner.files[partition].push(rows);
                }
                // A partition sent keeps its last chunk, emptied, as its tail.
                if demote && index + 1 == count {
                    self.inner.chunks[partition].push(Vec::new());
                } else {
                    self.count(partition, self.chunk_len, false)?;
                }
            }
            self.on_disk[partition] = true;
            self.ran[usize::from(!demote)] += 1;
            let freed = self.memory < before || demote;
            action = self.step(Event::Sent {
                freed,
                memory: self.memory,
            })?;
        }
    }

    /// Past a check, the memory fits, or what is left is the tails of
    /// partitions on disk, which the rule weighs as nothing.
    fn check_memory(&self) {
        let on_disk = self.on_disk.iter().filter(|&&on| on).count() as u64;
        let held = self.memory as f64 + self.join.reserve as f64 * on_disk as f64;
        let resident: u64 = (0..self.on_disk.len())
            .filter(|&partition| !self.on_disk[partition])
            .map(|partition| self.inner.chunks[partition].len() as u64)
            .sum();
        let params = self.join.params;
        assert!(
            held <= params.start.max(params.target) * params.limit as f64 || resident == 0,
            "{held} bytes past the limit with {resident} chunks in memory"
        );
    }

    /// Move the table into the level the machine started.
    fn start(&mut self, start: Start) -> Result<()> {
        let partitions = start.plan.partitions as usize;
        self.regions = Regions::of(&start);
        self.chunk_len = start.plan.chunk_len as u64;
        self.outer_len = start.outer_len as u64;
        self.shift = start.plan.shift;
        self.inner = Held {
            chunks: vec![Vec::new(); partitions],
            files: vec![Vec::new(); partitions],
        };
        self.outer = Held {
            chunks: vec![Vec::new(); partitions],
            files: vec![Vec::new(); partitions],
        };
        self.on_disk = vec![false; partitions];
        assert_eq!(self.step(Event::Made)?, Action::Done);
        self.memory = 0;
        for row in core::mem::take(&mut self.table) {
            self.append(Side::Inner, row)?;
        }
        self.send(
            Event::Grown {
                memory: self.memory,
            },
            true,
        )
    }

    /// Join `inner` and `outer` through the machine; the partitions on
    /// disk are joined whole, as the host decides for now.
    fn run(params: &Params, inner: &[Row], outer: &[Row], batch: usize) -> Result<(Pairs, Ran)> {
        let mut model = Self {
            join: Join::new(params)?,
            record: params.shape.record as u64,
            chunk_len: 0,
            outer_len: 0,
            shift: 0,
            table: Vec::new(),
            regions: Regions::none(),
            inner: Held::default(),
            outer: Held::default(),
            memory: 0,
            on_disk: Vec::new(),
            pairs: Vec::new(),
            ran: [0; 6],
        };
        let mut spilled = false;
        for rows in inner.chunks(batch) {
            if !spilled {
                let held = model.table.len() as u64 * model.record;
                let event = Event::Inner {
                    held,
                    rows: model.table.len() as u64,
                    estimate: inner.len() as f64,
                    needed: held + 2 * rows.len() as u64 * model.record,
                };
                match model.step(event)? {
                    Action::Done => {
                        model.table.extend_from_slice(rows);
                        continue;
                    }
                    Action::Start(start) => {
                        model.start(start)?;
                        spilled = true;
                    }
                    action => panic!("{action:?} before a batch"),
                }
            }
            for &row in rows {
                model.append(Side::Inner, row)?;
            }
            model.send(
                Event::Grown {
                    memory: model.memory,
                },
                true,
            )?;
        }
        if !spilled {
            pairs(&model.table, outer, &mut model.pairs);
            return Ok((model.pairs, model.ran));
        }
        model.send(Event::BuildEnd, false)?;
        // Every inner row is in memory or on disk, once.
        let mut kept: Vec<u32> = (model.inner.chunks.iter().flatten())
            .chain(model.inner.files.iter().flatten())
            .flatten()
            .map(|row| row.id)
            .collect();
        kept.sort_unstable();
        assert!(kept.iter().copied().eq(0..inner.len() as u32));
        // The routes were set, every one.
        let routes = model.regions.routes.clone();
        for (partition, &route) in routes.iter().enumerate() {
            let expected = if !model.on_disk[partition] {
                Route::Probe
            } else if model.regions.rows[partition] == 0 {
                Route::Answer
            } else {
                Route::Write
            };
            assert_eq!(route, expected as u8);
        }
        for &row in outer {
            let partition = model.partition(row.key);
            model.ran[3 + usize::from(routes[partition])] += 1;
            if routes[partition] == Route::Probe as u8 {
                let table: Vec<Row> = model.inner.chunks[partition].concat();
                pairs(&table, &[row], &mut model.pairs);
            } else if routes[partition] == Route::Write as u8 {
                model.append(Side::Outer, row)?;
            }
        }
        assert_eq!(model.step(Event::OuterEnd)?, Action::Done);
        for (partition, &route) in routes.iter().enumerate() {
            if route != Route::Write as u8 {
                continue;
            }
            let inner: Vec<Row> = model.inner.files[partition]
                .concat()
                .into_iter()
                .chain(model.inner.chunks[partition].concat())
                .collect();
            let outer: Vec<Row> = model.outer.files[partition]
                .concat()
                .into_iter()
                .chain(model.outer.chunks[partition].concat())
                .collect();
            pairs(&inner, &outer, &mut model.pairs);
        }
        Ok((model.pairs, model.ran))
    }
}

/// The pairs of a join, the ids of an inner row and an outer row.
type Pairs = Vec<(u32, u32)>;

/// What a model ran: demotions, evictions, flushes, and outer rows
/// probed, written and answered.
type Ran = [usize; 6];

/// The pairs of a join in memory.
fn pairs(inner: &[Row], outer: &[Row], out: &mut Pairs) {
    for o in outer {
        for i in inner {
            if i.key == o.key {
                out.push((i.id, o.id));
            }
        }
    }
}

fn rows(keys: &[u32]) -> Vec<Row> {
    keys.iter()
        .enumerate()
        .map(|(id, &key)| Row { key, id: id as u32 })
        .collect()
}

#[test]
fn a_join_through_the_machine_gives_the_pairs_of_one_in_memory() {
    let strategy = (
        prop::collection::vec(0_u32..400, 0..1500),
        prop::collection::vec(0_u32..500, 0..300),
        8_u64..200,
        1_usize..64,
        (0.5_f64..=1.0, 0.5_f64..=1.0, 0.0_f64..=1.0, 0.0_f64..0.5),
    );
    property(
        strategy,
        |(inner, outer, kib, batch, (start, target, spilled, resident))| {
            let params = Params {
                start,
                target,
                spilled,
                resident,
                ..params(kib * 1024)
            };
            let (inner, outer) = (rows(&inner), rows(&outer));
            let (mut got, _) = Model::run(&params, &inner, &outer, batch)?;
            let mut expected = Vec::new();
            pairs(&inner, &outer, &mut expected);
            got.sort_unstable();
            expected.sort_unstable();
            assert_eq!(got, expected);
            Ok(())
        },
    );
}

/// `count` rows of `keys` keys whose hashes fall into `partition` of
/// any level of up to 64 partitions.
fn keys_of(partition: u32, keys: usize, count: usize) -> Vec<u32> {
    let of: Vec<u32> = (0..)
        .filter(|&key| hash(key) & 63 == partition)
        .take(keys)
        .collect();
    (0..count).map(|row| of[row % keys]).collect()
}

/// Join through the machine, check the pairs, and give what ran.
fn ran(params: &Params, inner: &[u32], outer: &[u32]) -> Result<Ran> {
    let (inner, outer) = (rows(inner), rows(outer));
    let (mut got, ran) = Model::run(params, &inner, &outer, 50)?;
    let mut expected = Vec::new();
    pairs(&inner, &outer, &mut expected);
    got.sort_unstable();
    expected.sort_unstable();
    assert_eq!(got, expected);
    Ok(ran)
}

#[test]
fn a_join_that_spills_runs_every_path() -> Result<()> {
    let outer: Vec<u32> = (0..2000).collect();
    // Little memory: every partition on disk, their tails sent again.
    let spread: Vec<u32> = (0..6000).map(|row| row * 7 % 300).collect();
    let little = Params {
        spilled: 1.0,
        ..params(8 * 1024)
    };
    let [demoted, evicted, flushed, _, written, _] = ran(&little, &spread, &outer)?;
    assert!(demoted > 0 && evicted > 0 && flushed > 0 && written > 0);
    // One partition large, one in memory: probed and written.
    let two = [keys_of(0, 100, 3000), keys_of(1, 50, 1500)].concat();
    let [demoted, _, _, probed, written, _] = ran(&params(100 * 1024), &two, &outer)?;
    assert!(demoted > 0 && probed > 0 && written > 0);
    // Few rows left in memory: the empty partitions go too, answered.
    let few = [keys_of(0, 100, 3000), keys_of(3, 20, 500)].concat();
    let [_, _, _, _, written, answered] = ran(&params(100 * 1024), &few, &outer)?;
    assert!(written > 0 && answered > 0);
    Ok(())
}

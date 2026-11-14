use core::cell::Cell;

use proptest::prelude::*;

use super::model::{Counts, Kind as JoinKind, Model, Ran, Regions, Row, hash, joined};
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
        preserve_inner: 0,
        unmatched_outer: 0,
        spare: 0,
        page: 512,
        start: 1.0,
        target: 1.0,
        spilled: 0.0,
        resident: 0.25,
        tail: 1.0,
        split_room: 2.0 / 3.0,
        split_key: 0.9,
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
        routes: vec![0; 2],
        ..Regions::of(&start)
    };
    short.total = regions.total;
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
    // The outer rows end after the table they probed.
    assert_eq!(
        wrong(join.step(Event::OuterEnd, &mut regions.host())),
        Rule::JoinEvent
    );
    let end = Event::TableEnd { held: false };
    assert_eq!(join.step(end, &mut regions.host()), Ok(Action::Done));
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
    let below = Below {
        partition: 3,
        level: 1,
        expected: 4 * limit,
        limit: limit / 2,
        shift: 2,
    };
    let (join, start) = Join::below(&params(limit), &below).expect("a level below");
    assert_eq!((join.level(), start.plan.shift), (1, 2));
    assert_eq!(join.phase, PLANNED);
    let planned = plan::plan(
        &params(limit).shape,
        &Level {
            expected: (4 * limit) as f64,
            shift: 2,
            ..Level::first(limit as usize / 2)
        },
    );
    assert_eq!(Ok(start.plan), planned);
    let first = Below { level: 0, ..below };
    assert_eq!(wrong(Join::below(&params(limit), &first)), Rule::JoinLevel);
    let past = Below { shift: 40, ..below };
    assert_eq!(wrong(Join::below(&params(limit), &past)), Rule::PlanBits);
}

#[test]
fn a_shared_level_takes_the_partitions_given() {
    let limit = 1 << 20;
    let (mut join, start) = Join::given(&params(limit), 16).expect("a level");
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
    // A partition a participant takes alone splits, or is joined whole.
    let mut regions = Regions::of(&start);
    regions.rows[5] = 1000;
    regions.rows[6] = 1000;
    regions.total = 100_000;
    regions.disk[5] = 900_000;
    regions.disk[6] = 1000;
    let take = |partition| Event::Take {
        partition,
        memory: 0,
    };
    let Ok(Action::Split(below)) = join.step(take(5), &mut regions.host()) else {
        panic!("a split");
    };
    assert_eq!(
        (below.partition, below.level, below.expected),
        (5, 1, 900_000)
    );
    assert_eq!(below.shift, start.plan.below);
    assert_eq!(
        join.step(take(6), &mut regions.host()),
        Ok(Action::Open {
            partition: 6,
            pieces: false
        })
    );
    // No partition comes before the last one's table ends.
    assert_eq!(
        wrong(join.step(take(7), &mut regions.host())),
        Rule::JoinEvent
    );
    let end = Event::TableEnd { held: false };
    assert_eq!(join.step(end, &mut regions.host()), Ok(Action::Done));
    assert_eq!(
        wrong(join.step(take(16), &mut regions.host())),
        Rule::Partition
    );
    // The shared build decides the rest.
    let next = Event::Next { memory: 0 };
    assert_eq!(wrong(join.step(next, &mut regions.host())), Rule::JoinEvent);
}

#[test]
fn parameters_are_checked() {
    let good = params(1 << 20);
    assert!(Join::new(&good).is_ok());
    for bad in [
        Params { limit: 0, ..good },
        Params {
            preserve_inner: 2,
            ..good
        },
        Params {
            split_key: -0.5,
            ..good
        },
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

/// A machine whose level joins: its build ended with partitions 1, 2 and
/// 3 on disk, of 2000, 0 and 300 inner rows, and its outer rows ended.
fn joining(params: &Params) -> (Join, Regions) {
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
    assert_eq!(start.plan.partitions, 4);
    let mut regions = Regions::of(&start);
    assert_eq!(
        join.step(Event::Made, &mut regions.host()),
        Ok(Action::Done)
    );
    regions.rows = vec![5000, 2000, 0, 300];
    regions.total = 7300;
    for partition in [1, 2, 3] {
        regions
            .spill()
            .add_bytes(4 << 20, Some(partition))
            .expect("bytes");
        let grown = Event::Grown { memory: 4 << 20 };
        assert_eq!(
            join.step(grown, &mut regions.host()),
            Ok(Action::Demote(partition))
        );
        regions
            .spill()
            .add_bytes(-(4 << 20), Some(partition))
            .expect("bytes");
        let sent = Event::Sent {
            freed: true,
            memory: 0,
        };
        assert_eq!(join.step(sent, &mut regions.host()), Ok(Action::Done));
    }
    assert_eq!(
        join.step(Event::BuildEnd, &mut regions.host()),
        Ok(Action::Done)
    );
    // The table in memory ends, after its tail when the join keeps its
    // inner rows.
    let end = Event::TableEnd { held: false };
    if params.preserve_inner != 0 {
        assert_eq!(join.step(end, &mut regions.host()), Ok(Action::Tail));
        assert_eq!(
            join.step(Event::TailEnd, &mut regions.host()),
            Ok(Action::Done)
        );
    }
    for event in [end, Event::OuterEnd] {
        assert_eq!(join.step(event, &mut regions.host()), Ok(Action::Done));
    }
    (join, regions)
}

/// The machine of a level below, made, its partitions 1, 2 and 3 sent to
/// disk, its build and its outer rows ended.
fn joining_below(params: &Params, below: &Below) -> (Join, Regions) {
    let (mut join, start) = Join::below(params, below).expect("a level below");
    let mut regions = Regions::of(&start);
    assert_eq!(
        join.step(Event::Made, &mut regions.host()),
        Ok(Action::Done)
    );
    regions.rows[0] = 5000;
    regions.rows[1] = 2000;
    regions.rows[3] = 300;
    regions.total = 7300;
    for partition in [1, 2, 3] {
        regions
            .spill()
            .add_bytes(4 << 20, Some(partition))
            .expect("bytes");
        let grown = Event::Grown { memory: 4 << 20 };
        assert_eq!(
            join.step(grown, &mut regions.host()),
            Ok(Action::Demote(partition))
        );
        regions
            .spill()
            .add_bytes(-(4 << 20), Some(partition))
            .expect("bytes");
        let sent = Event::Sent {
            freed: true,
            memory: 0,
        };
        assert_eq!(join.step(sent, &mut regions.host()), Ok(Action::Done));
    }
    for event in [
        Event::BuildEnd,
        Event::TableEnd { held: false },
        Event::OuterEnd,
    ] {
        assert_eq!(join.step(event, &mut regions.host()), Ok(Action::Done));
    }
    (join, regions)
}

/// The next partition of a level that joins, its partition 1 of `size`
/// bytes on disk with outer rows, past `memory`: the level below it splits
/// into, or none.
fn split_of(join: &mut Join, regions: &mut Regions, size: u64, memory: u64) -> Option<Below> {
    regions.outer = vec![0; regions.rows.len()];
    regions.outer[1] = 100;
    regions.disk = vec![0; regions.rows.len()];
    regions.disk[1] = size;
    match join.step(Event::Next { memory }, &mut regions.host()) {
        Ok(Action::Split(below)) => Some(below),
        Ok(Action::Release(_) | Action::LevelDone) | Err(_) => panic!("no partition 1"),
        Ok(_) => None,
    }
}

#[test]
fn a_split_weighs_a_chunk_of_outer_rows_and_the_join_s_limit() {
    let limit = 1 << 20;
    let params = params(limit);
    // Between two thirds of the limit with and without a chunk of outer
    // rows and its values: it splits, the chunk counted.
    let (mut join, mut regions) = joining(&params);
    let size = 2 * (limit - join.outer_len) / 3;
    assert!(split_of(&mut join, &mut regions, size, 0).is_some());
    // A level below of a quarter of the limit weighs the join's limit:
    // the same partition, past two thirds of the quarter, does not split.
    let below = Below {
        partition: 1,
        level: 1,
        expected: 4 << 20,
        limit: limit / 4,
        shift: 2,
    };
    let (mut level, mut regions) = joining_below(&params, &below);
    let size = limit / 2;
    assert!(size > 2 * (limit / 4) / 3);
    assert_eq!(split_of(&mut level, &mut regions, size, 0), None);
    // Larger, it splits into the level below that one.
    let (mut level, mut regions) = joining_below(&params, &below);
    let deeper = split_of(&mut level, &mut regions, 2 * limit, 0).expect("a split");
    assert_eq!((deeper.level, deeper.shift), (2, level.below));
}

#[test]
fn a_level_below_has_a_quarter_from_three_quarters_of_the_limit_in_use() {
    // A limit that four does not divide, so that a quarter of it is not
    // what three quarters leave.
    let limit = (1 << 20) + 3;
    let level = Below {
        partition: 1,
        level: 1,
        expected: 1000,
        limit,
        shift: 2,
    };
    let memory = limit / 4 * 3;
    let (mut join, mut regions) = joining_below(&params(limit), &level);
    let below = split_of(&mut join, &mut regions, 600_000, memory).expect("a split");
    assert_eq!(below.limit, limit / 4);
    let (mut join, mut regions) = joining_below(&params(limit), &level);
    let below = split_of(&mut join, &mut regions, 600_000, memory - 1).expect("a split");
    assert_eq!(below.limit, limit - (memory - 1));
}

#[test]
fn a_partition_taken_alone_weighs_the_memory_in_use() {
    let limit = 1 << 20;
    let (mut join, start) = Join::given(&params(limit), 4).expect("a level");
    let mut regions = Regions::of(&start);
    regions.rows = vec![1000; 4];
    regions.total = 100_000;
    regions.disk = vec![400_000; 4];
    let take = |partition, memory| Event::Take { partition, memory };
    // Past what 600 000 bytes in use leave, not what nothing leaves.
    let Ok(Action::Split(_)) = join.step(take(1, 600_000), &mut regions.host()) else {
        panic!("a split");
    };
    let whole = join.step(take(2, 0), &mut regions.host());
    assert!(!matches!(whole, Ok(Action::Split(_))), "{whole:?}");
}

#[test]
fn partitions_on_disk_are_released_joined_or_split() {
    let limit = 1 << 20;
    let (mut join, mut regions) = joining(&params(limit));
    // Partition 1 larger than two thirds of what is left, 3 smaller.
    regions.outer = vec![0, 100, 0, 0];
    regions.disk = vec![0, 800_000, 0, 1000];
    let next = |memory| Event::Next { memory };
    let memory = 10_000;
    let used = memory + 2 * join.outer_len;
    assert!(800_000.0 > 2.0 / 3.0 * (limit - used) as f64);
    let split = Below {
        partition: 1,
        level: 1,
        expected: 800_000,
        limit: limit - memory,
        shift: join.below,
    };
    assert_eq!(
        join.step(next(memory), &mut regions.host()),
        Ok(Action::Split(split))
    );
    // No outer row, and no inner row to give out alone: nothing to join.
    assert_eq!(
        join.step(next(memory), &mut regions.host()),
        Ok(Action::Release(2))
    );
    assert_eq!(
        join.step(next(memory), &mut regions.host()),
        Ok(Action::Release(3))
    );
    assert_eq!(
        join.step(next(memory), &mut regions.host()),
        Ok(Action::LevelDone)
    );
    assert_eq!(
        wrong(join.step(next(memory), &mut regions.host())),
        Rule::JoinEvent
    );
}

#[test]
fn a_partition_too_large_to_split_is_joined_in_pieces() {
    let limit = 1 << 20;
    // Preserved inner rows keep partition 3 though it has no outer row.
    let params = Params {
        preserve_inner: 1,
        ..params(limit)
    };
    let (mut join, mut regions) = joining(&params);
    regions.outer = vec![0, 100, 0, 0];
    regions.disk = vec![0, 800_000, 0, 1000];
    // One key: partition 1 holds more than nine tenths of its level's rows.
    regions.rows = vec![0, 7000, 0, 300];
    regions.total = 7300;
    let before = join;
    let mut short = Regions {
        words: regions.words.clone(),
        disk: vec![0; 3],
        ..Regions::of(&Start {
            plan: plan::plan(&params.shape, &Level::first(limit as usize)).expect("plan"),
            outer_len: 0,
        })
    };
    short.rows = regions.rows.clone();
    short.outer = regions.outer.clone();
    let next = Event::Next { memory: 0 };
    assert_eq!(wrong(join.step(next, &mut short.host())), Rule::JoinDisk);
    assert_eq!(join, before);
    // Larger than two thirds of what the limit leaves: in pieces.
    let room = (limit - 2 * join.outer_len) / 3 * 2;
    assert!(800_000 > room);
    let open = Action::Open {
        partition: 1,
        pieces: true,
    };
    assert_eq!(join.step(next, &mut regions.host()), Ok(open));
    assert_eq!(wrong(join.step(next, &mut regions.host())), Rule::JoinEvent);
    let mut step = |event| join.step(event, &mut regions.host());
    let records = |len| Event::Block { values: false, len };
    assert_eq!(step(Event::Piece { memory: 0 }), Ok(Action::Done));
    assert_eq!(step(records(room - 1)), Ok(Action::Load));
    // Below the room the piece goes on; past it, without values, it ends
    // at the next block of records.
    assert_eq!(step(records(1)), Ok(Action::Load));
    assert_eq!(step(records(1000)), Ok(Action::Carry));
    // The piece's table goes after its tail, the join's inner rows kept.
    let end = Event::TableEnd { held: false };
    assert_eq!(
        step(Event::PassEnd),
        Err(Error::wrong_call(Rule::JoinEvent).with(PASS).and(13))
    );
    assert_eq!(step(Event::TableEnd { held: true }), Ok(Action::Hold));
    assert_eq!(step(end), Ok(Action::Tail));
    assert_eq!(step(Event::TailEnd), Ok(Action::Done));
    assert_eq!(step(end), Ok(Action::Done));
    assert_eq!(step(Event::PassEnd), Ok(Action::NextPiece));
    // More memory in use leaves a smaller room.
    let memory = 2 * room / 3;
    assert_eq!(step(Event::Piece { memory }), Ok(Action::Done));
    assert_eq!(step(records(1000)), Ok(Action::Load));
    assert_eq!(step(Event::FileEnd), Ok(Action::Done));
    assert_eq!(step(end), Ok(Action::Tail));
    assert_eq!(step(Event::TailEnd), Ok(Action::Done));
    assert_eq!(step(end), Ok(Action::Done));
    // The file is done, and no outer row goes out without a pair.
    assert_eq!(step(Event::PassEnd), Ok(Action::Done));
    // Partition 2: no inner row to give out; 3 whole.
    assert_eq!(join.step(next, &mut regions.host()), Ok(Action::Release(2)));
    let whole = Action::Open {
        partition: 3,
        pieces: false,
    };
    assert_eq!(join.step(next, &mut regions.host()), Ok(whole));
    // With most of the limit in use, a level below has a quarter of it.
    let (mut again, mut regions) = joining(&params);
    regions.outer = vec![0, 100, 0, 0];
    regions.disk = vec![0, 300_000, 0, 0];
    let memory = limit / 4 * 3 + 1;
    let Ok(Action::Split(below)) = again.step(Event::Next { memory }, &mut regions.host()) else {
        panic!("a split");
    };
    assert_eq!(below.limit, limit / 4);
}

#[test]
fn a_piece_ends_where_a_group_of_values_starts() {
    // A left join: its rows without a pair go out in a last pass.
    let params = Params {
        unmatched_outer: 1,
        ..params(1 << 20)
    };
    let (mut join, mut regions) = joining(&params);
    regions.outer = vec![0, 100, 0, 0];
    regions.disk = vec![0, 4 << 20, 0, 0];
    regions.rows = vec![0, 7000, 0, 300];
    let next = Event::Next { memory: 0 };
    let Ok(Action::Open { pieces: true, .. }) = join.step(next, &mut regions.host()) else {
        panic!("pieces");
    };
    let mut step = |event| join.step(event, &mut regions.host());
    let block = |values, len| Event::Block { values, len };
    assert_eq!(step(Event::Piece { memory: 1 << 20 }), Ok(Action::Done));
    // No room at all: still one group, its values and its records.
    assert_eq!(step(block(true, 100)), Ok(Action::Load));
    assert_eq!(step(block(false, 100)), Ok(Action::Load));
    // Records after records are the rest of the group.
    assert_eq!(step(block(false, 100)), Ok(Action::Load));
    assert_eq!(step(block(true, 100)), Ok(Action::Carry));
    let end = Event::TableEnd { held: false };
    assert_eq!(step(end), Ok(Action::Done));
    assert_eq!(step(Event::PassEnd), Ok(Action::NextPiece));
    assert_eq!(step(Event::Piece { memory: 1 << 20 }), Ok(Action::Done));
    assert_eq!(step(block(true, 100)), Ok(Action::Load));
    // Values after values: one group still, records not yet come.
    assert_eq!(step(block(true, 100)), Ok(Action::Load));
    assert_eq!(step(block(false, 100)), Ok(Action::Load));
    assert_eq!(step(Event::FileEnd), Ok(Action::Done));
    assert_eq!(step(end), Ok(Action::Done));
    assert_eq!(step(Event::PassEnd), Ok(Action::FinalPass));
    // The last pass has no table: its end gives no tail.
    assert_eq!(step(end), Ok(Action::Done));
    assert_eq!(step(Event::PassEnd), Ok(Action::Done));
    // Partitions 2 and 3: no outer row.
    assert_eq!(step(next), Ok(Action::Release(2)));
    assert_eq!(step(next), Ok(Action::Release(3)));
    assert_eq!(step(next), Ok(Action::LevelDone));
}

#[test]
fn a_piece_ends_only_where_the_records_of_a_group_end() {
    let (mut join, mut regions) = joining(&params(1 << 20));
    regions.outer = vec![0, 100, 0, 0];
    regions.disk = vec![0, 4 << 20, 0, 0];
    regions.rows = vec![0, 7000, 0, 300];
    let Ok(Action::Open { pieces: true, .. }) =
        join.step(Event::Next { memory: 0 }, &mut regions.host())
    else {
        panic!("pieces");
    };
    let mut step = |event| join.step(event, &mut regions.host());
    let block = |values, len| Event::Block { values, len };
    assert_eq!(step(Event::Piece { memory: 0 }), Ok(Action::Done));
    assert_eq!(step(block(true, 1)), Ok(Action::Load));
    assert_eq!(step(block(false, 1)), Ok(Action::Load));
    // The next group's values fill the room: a group holds several
    // blocks of values, and records have not come since.
    assert_eq!(step(block(true, u64::MAX / 2)), Ok(Action::Load));
    assert_eq!(step(block(true, 1)), Ok(Action::Load));
    assert_eq!(step(block(false, 1)), Ok(Action::Load));
    assert_eq!(step(block(true, 1)), Ok(Action::Carry));
}

#[test]
fn a_piece_of_a_level_below_has_the_room_of_the_join() {
    let limit = 1 << 20;
    let mut params = params(limit);
    params.shape.min_partitions = 8;
    // A level below with no bits for another: no split, and a quarter
    // of the join's limit.
    let level = Below {
        partition: 1,
        level: 1,
        expected: 1000,
        limit: limit / 4,
        shift: 27,
    };
    let (mut join, mut regions) = joining_below(&params, &level);
    assert_eq!(join.deeper, 0);
    regions.outer[1] = 100;
    regions.disk[1] = 4 << 20;
    let Ok(Action::Open { pieces: true, .. }) =
        join.step(Event::Next { memory: 0 }, &mut regions.host())
    else {
        panic!("pieces");
    };
    // Past two thirds of the level's limit, short of the join's.
    let size = limit / 2;
    assert!(size + 1 < (limit - 2 * join.outer_len) / 3 * 2);
    assert!(size > level.limit / 3 * 2);
    let mut step = |event| join.step(event, &mut regions.host());
    let block = |values, len| Event::Block { values, len };
    assert_eq!(step(Event::Piece { memory: 0 }), Ok(Action::Done));
    assert_eq!(step(block(true, 1)), Ok(Action::Load));
    assert_eq!(step(block(false, size)), Ok(Action::Load));
    assert_eq!(step(block(true, 1)), Ok(Action::Load));
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

fn rows(keys: &[u32]) -> Vec<Row> {
    keys.iter()
        .enumerate()
        .map(|(id, &key)| Row { key, id: id as u32 })
        .collect()
}

/// Join through the machine as a join of `kind`, `batch` inner rows at a
/// time, whose inner rows keep `values` or not, check what it gives
/// against a join in memory, and give what ran.
fn ran_as(
    params: &Params,
    kind: JoinKind,
    values: bool,
    inner: &[u32],
    outer: &[u32],
    batch: usize,
) -> Result<Counts> {
    let params = Params {
        unmatched_outer: u32::from(kind.unmatched()),
        preserve_inner: u32::from(kind.preserved()),
        ..*params
    };
    let (inner, outer) = (rows(inner), rows(outer));
    let (mut got, ran) = Model::run(&params, kind, values, &inner, &outer, batch)?;
    let mut expected = Vec::new();
    joined(kind, &inner, &outer, &mut expected);
    got.sort_unstable();
    expected.sort_unstable();
    assert_eq!(got, expected, "{kind:?}");
    Ok(ran)
}

/// [`ran_as`] an inner join of rows without values.
fn ran(params: &Params, inner: &[u32], outer: &[u32], batch: usize) -> Result<Counts> {
    ran_as(params, JoinKind::Inner, false, inner, outer, batch)
}

const KINDS: [JoinKind; 6] = [
    JoinKind::Inner,
    JoinKind::Semi,
    JoinKind::Anti,
    JoinKind::Left,
    JoinKind::Right,
    JoinKind::Full,
];

#[test]
fn a_join_through_the_machine_gives_the_pairs_of_one_in_memory() {
    let strategy = (
        prop::collection::vec(0_u32..400, 0..1500),
        prop::collection::vec(0_u32..500, 0..300),
        8_u64..200,
        1_usize..64,
        (0.5_f64..=1.0, 0.5_f64..=1.0, 0.0_f64..=1.0, 0.0_f64..0.5),
        (0.0_f64..2.0, 0.0_f64..=1.0),
        (0_usize..6, any::<bool>()),
    );
    property(
        strategy,
        |(
            inner,
            outer,
            kib,
            batch,
            (start, target, spilled, resident),
            (room, key),
            (kind, values),
        )| {
            let params = Params {
                start,
                target,
                spilled,
                resident,
                split_room: room,
                split_key: key,
                ..params(kib * 1024)
            };
            ran_as(&params, KINDS[kind], values, &inner, &outer, batch)?;
            Ok(())
        },
    );
}

#[test]
fn a_join_that_spills_runs_every_path() -> Result<()> {
    let outer: Vec<u32> = (0..2000).collect();
    // Little memory: every partition on disk, their tails sent again, and
    // the partitions split into levels below, some of them released.
    let spread: Vec<u32> = (0..6000).map(|row| row * 7 % 300).collect();
    let little = Params {
        spilled: 1.0,
        ..params(8 * 1024)
    };
    let ran_little = ran(&little, &spread, &outer, 50)?;
    // Levels below levels below.
    assert!(ran_little[Ran::DEEPEST] > 2, "{ran_little:?}");
    for what in [
        Ran::DEMOTED,
        Ran::EVICTED,
        Ran::FLUSHED,
        Ran::WRITTEN,
        Ran::SPLIT,
        Ran::OPENED,
    ] {
        assert!(ran_little[what] > 0, "{what} in {ran_little:?}");
    }
    // One partition large, one in memory: probed and written.
    let two = [keys_of(0, 100, 3000), keys_of(1, 50, 1500)].concat();
    let ran_two = ran(&params(100 * 1024), &two, &outer, 50)?;
    for what in [Ran::DEMOTED, Ran::PROBED, Ran::WRITTEN, Ran::OPENED] {
        assert!(ran_two[what] > 0, "{what} in {ran_two:?}");
    }
    // Few rows left in memory: the empty partitions go too, answered and
    // released.
    let few = [keys_of(0, 100, 3000), keys_of(3, 20, 500)].concat();
    let ran_few = ran(&params(100 * 1024), &few, &outer, 50)?;
    for what in [Ran::WRITTEN, Ran::ANSWERED, Ran::RELEASED] {
        assert!(ran_few[what] > 0, "{what} in {ran_few:?}");
    }
    Ok(())
}

#[test]
fn one_key_too_large_is_joined_in_pieces() -> Result<()> {
    // One key no level below splits, with values or not: in pieces, the
    // outer rows read once a piece and, for the rows without a pair, once
    // more, and the inner rows without a pair given out after each piece;
    // each join kind gives what it gives in memory.
    let one = vec![7; 3000];
    let outer = [7, 7, 8, 9, 7];
    for kind in KINDS {
        for values in [false, true] {
            let ran = ran_as(&params(16 * 1024), kind, values, &one, &outer, 50)?;
            assert!(ran[Ran::SPLIT] == 0 && ran[Ran::PIECED] > 0, "{ran:?}");
            assert!(ran[Ran::PIECES] > 2, "{ran:?}");
            // A tail for each piece and for the table in memory.
            if kind.preserved() {
                assert!(ran[Ran::TAILS] > ran[Ran::PIECES], "{ran:?}");
            }
        }
    }
    Ok(())
}

#[test]
fn the_end_of_the_build_goes_from_start_to_target_too() {
    // A start of 0 sends a partition whatever the memory; the target of 1
    // stops the next.
    let params = Params {
        start: 0.0,
        target: 1.0,
        resident: 0.0,
        ..params(1 << 20)
    };
    let (mut join, _, mut regions) = spilling_with(&params);
    for (partition, bytes) in [(0, 300_000), (1, 200_000), (2, 100_000)] {
        regions
            .spill()
            .add_bytes(bytes, Some(partition))
            .expect("bytes");
        regions.rows[partition as usize] = 1000;
    }
    regions.total = 3000;
    let grown = Event::Grown { memory: 600_000 };
    assert_eq!(join.step(grown, &mut regions.host()), Ok(Action::Demote(0)));
    assert_eq!(sent(&mut join, &mut regions, 0, 300_000, 0), Action::Done);
    assert_eq!(
        join.step(Event::BuildEnd, &mut regions.host()),
        Ok(Action::Demote(1))
    );
    assert_eq!(sent(&mut join, &mut regions, 1, 200_000, 0), Action::Done);
    assert_eq!(regions.routes, [1, 1, 0, 0]);
}

#[test]
fn no_partition_splits_where_no_level_of_the_shape_fits() {
    // Eight partitions at the least: three bits, of which two are left.
    let limit = 1 << 20;
    let mut params = params(limit);
    params.shape.min_partitions = 8;
    let level = Below {
        partition: 1,
        level: 1,
        expected: 1000,
        limit,
        shift: 27,
    };
    let (mut join, mut regions) = joining_below(&params, &level);
    assert_eq!((join.below, join.deeper), (30, 0));
    let next = split_of(&mut join, &mut regions, 2 * limit, 0);
    assert_eq!(next, None);
}

#[test]
fn a_partition_is_read_in_pieces_past_what_the_memory_in_use_leaves() {
    let limit = 1 << 20;
    let memory = 300_000;
    let opened = |size, shared| {
        let params = params(limit);
        let (mut join, mut regions) = if shared {
            let (join, start) = Join::given(&params, 4).expect("a level");
            (join, Regions::of(&start))
        } else {
            joining(&params)
        };
        // One key: no split.
        regions.rows = vec![0, 7000, 0, 300];
        regions.total = 7300;
        regions.outer = vec![0, 10, 0, 0];
        regions.disk = vec![0, size, 0, 0];
        let event = if shared {
            Event::Take {
                partition: 1,
                memory,
            }
        } else {
            Event::Next { memory }
        };
        let room = (limit - memory - 2 * join.outer_len) / 3 * 2;
        (join.step(event, &mut regions.host()), room)
    };
    for shared in [false, true] {
        let (_, room) = opened(0, shared);
        let open = |pieces| {
            Ok(Action::Open {
                partition: 1,
                pieces,
            })
        };
        assert_eq!(opened(room, shared).0, open(false), "{shared}");
        assert_eq!(opened(room + 1, shared).0, open(true), "{shared}");
    }
}

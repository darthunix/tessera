use super::*;
use crate::Kind;
use crate::phases::{BUILD as BUILD_PHASE, FREE, LINK as LINK_PHASE, OUTER as OUTER_PHASE};
use crate::shared_spill::words_for;

/// The rule of a wrong call's error, which a test expects.
#[track_caller]
fn wrong<T: core::fmt::Debug>(result: Result<T>) -> Rule {
    let error = result.expect_err("a wrong call");
    assert_eq!(error.kind(), Kind::WrongCall, "{error:?}");
    error.rule()
}

fn params() -> Params {
    Params {
        start: 1.0,
        target: 0.5,
        spilled: 1.0,
        limit: 1000,
        max_chunks: 4,
        per_check: 0,
        preserve: 0,
    }
}

/// Weights that send any partition with bytes.
fn any_bytes() -> Weights {
    Weights {
        start: 0.0,
        target: 0.0,
        spilled: 1.0,
        reserve: 0,
        resident: 0.0,
        per_check: 0,
    }
}

/// The words a build shares: its counters, `records` appended so far,
/// and the spill words of a table of up to 16 partitions, of `budget`
/// bytes.
struct Shared {
    counters: [AtomicU64; COUNTER_WORDS],
    spill: Vec<AtomicU64>,
}

impl Shared {
    fn new(records: u64, budget: u64) -> Self {
        let shared = Self {
            counters: [records, 0, 0, 0].map(AtomicU64::new),
            spill: (0..words_for(16).expect("words"))
                .map(|_| AtomicU64::new(0))
                .collect(),
        };
        shared.spill().init(budget);
        shared
    }

    fn spill(&self) -> SharedSpill<'_> {
        SharedSpill::new(&self.spill).expect("spill words")
    }

    fn host<'a>(&'a self, resident: &'a [u8]) -> Host<'a> {
        Host {
            counters: &self.counters,
            spill: &self.spill,
            resident,
        }
    }

    /// Another participant sends `partition` to disk.
    fn send(&self, partition: u32) {
        let spill = self.spill();
        spill.add_bytes(1, Some(partition)).expect("bytes");
        assert_eq!(
            spill.evict(&any_bytes(), Memory::Counted, None, 0),
            Ok(Some(partition))
        );
        spill.add_bytes(-1, Some(partition)).expect("bytes");
    }
}

/// The actions of a participant alone at the barrier, attached at
/// `phase`, until nothing is left: the barrier elects it every time.
fn alone(shared: &Shared, resident: &[u8], phase: u32) -> Vec<Action> {
    let mut build = Build::new(&params()).expect("parameters");
    let mut actions = Vec::new();
    let mut phase = phase;
    let mut event = Event::Done;
    loop {
        let action = build.step(event, &shared.host(resident)).expect("a step");
        actions.push(action);
        event = match action {
            Action::Attach => Event::Attached { phase },
            Action::ArriveAndWait => {
                phase += 1;
                Event::Arrived { elected: true }
            }
            Action::ArriveAndDetach => Event::Left { last: true },
            Action::Send(_) => Event::Sent { freed: true },
            // Each partition on disk fits a round.
            Action::Weigh(_) => Event::Weighed {
                bytes: 100,
                blocks: 1,
                index: 100,
            },
            Action::Free | Action::Finished => break,
            _ => Event::Done,
        };
    }
    actions
}

#[test]
fn a_table_that_never_split_writes_nothing() {
    use Action::*;
    let shared = Shared::new(3, 1 << 20);
    assert_eq!(
        alone(&shared, &[], BUILD_PHASE),
        [
            Attach,
            Build,
            ArriveAndWait,
            // The flush and the outer side have nothing to write.
            ArriveAndWait,
            Size,
            ArriveAndWait,
            Link,
            ArriveAndWait,
            ArriveAndWait,
            Probe { split: false },
            ArriveAndDetach,
            Free
        ]
    );
}

#[test]
fn a_table_that_split_writes_its_partitions_on_disk_and_keeps_the_others() {
    use Action::*;
    let shared = Shared::new(3, 1 << 20);
    assert_eq!(shared.spill().split(4), Ok(4));
    shared.send(1);
    assert_eq!(
        alone(&shared, &[1, 0, 1, 1], BUILD_PHASE),
        [
            Attach,
            Build,
            ArriveAndWait,
            // It catches up: its level, then nothing to write, since it
            // keeps nothing of partition 1.
            Switch { partitions: 4 },
            Keep(0),
            Write(1),
            Keep(2),
            Keep(3),
            FlushEnd,
            ArriveAndWait,
            Size,
            // The one partition on disk, weighed for its round.
            Weigh(1),
            ArriveAndWait,
            Link,
            ArriveAndWait,
            Outer,
            ArriveAndWait,
            Probe { split: true },
            ArriveAndDetach,
            Free
        ]
    );
    assert!(shared.spill().together(1).expect("a partition"));
}

#[test]
fn a_late_participant_skips_the_writes_of_a_table_that_never_split() {
    use Action::*;
    let shared = Shared::new(3, 1 << 20);
    assert_eq!(
        alone(&shared, &[], LINK_PHASE),
        [
            Attach,
            Link,
            ArriveAndWait,
            ArriveAndWait,
            Probe { split: false },
            ArriveAndDetach,
            Free
        ]
    );
    assert_eq!(
        alone(&shared, &[], OUTER_PHASE),
        [
            Attach,
            ArriveAndWait,
            Probe { split: false },
            ArriveAndDetach,
            Free
        ]
    );
    assert_eq!(alone(&shared, &[], FREE), [Attach, Detach, Finished]);
}

/// A machine of `params` attached to the build.
fn building(shared: &Shared, params: &Params) -> Build {
    let mut build = Build::new(params).expect("parameters");
    let host = shared.host(&[]);
    assert_eq!(build.step(Event::Done, &host), Ok(Action::Attach));
    let attached = Event::Attached { phase: BUILD_PHASE };
    assert_eq!(build.step(attached, &host), Ok(Action::Build));
    build
}

#[test]
fn a_batch_writes_what_others_sent_then_sends_the_largest() {
    let shared = Shared::new(0, 1000);
    let mut build = building(&shared, &params());
    let spill = shared.spill();
    assert_eq!(spill.split(4), Ok(4));
    shared.send(2);
    // This one keeps 0, 2 and 3 in memory; 3 and 0 pass the budget.
    spill.add_bytes(800, Some(3)).expect("bytes");
    spill.add_bytes(300, Some(0)).expect("bytes");
    let resident = [1, 0, 1, 1];
    let host = shared.host(&resident);
    let batch = Event::Batch { over: false };
    // The table split since its last batch: its level first.
    assert_eq!(
        build.step(batch, &host),
        Ok(Action::Switch { partitions: 4 })
    );
    assert_eq!(build.step(Event::Done, &host), Ok(Action::Demote(2)));
    assert_eq!(build.step(Event::Done, &host), Ok(Action::Send(3)));
    spill.add_bytes(-800, Some(3)).expect("bytes");
    // 300 of 1000, under the target of half: the check is over.
    assert_eq!(
        build.step(Event::Sent { freed: true }, &host),
        Ok(Action::Done)
    );
    // A batch with nothing new sends nothing.
    let resident = [1, 0, 0, 0];
    let host = shared.host(&resident);
    assert_eq!(build.step(batch, &host), Ok(Action::Done));
    // A send that freed nothing ends the check.
    spill.add_bytes(2000, Some(0)).expect("bytes");
    assert_eq!(build.step(batch, &host), Ok(Action::Send(0)));
    assert_eq!(
        build.step(Event::Sent { freed: false }, &host),
        Ok(Action::Done)
    );
    // A partition sent since the last batch is found by the next one's
    // scan from the first partition again.
    spill.add_bytes(-2300, Some(0)).expect("bytes");
    shared.send(3);
    let host = shared.host(&[0, 0, 0, 1]);
    assert_eq!(build.step(batch, &host), Ok(Action::Demote(3)));
    assert_eq!(build.step(Event::Done, &host), Ok(Action::Done));
    // The build ends: the flush comes next.
    assert_eq!(build.step(Event::Done, &host), Ok(Action::ArriveAndWait));
}

#[test]
fn a_check_sends_as_many_as_its_weights_allow() {
    let shared = Shared::new(0, 1000);
    let mut build = building(
        &shared,
        &Params {
            per_check: 1,
            ..params()
        },
    );
    let spill = shared.spill();
    assert_eq!(spill.split(2), Ok(2));
    spill.add_bytes(1500, Some(0)).expect("bytes");
    spill.add_bytes(1400, Some(1)).expect("bytes");
    let host = shared.host(&[1, 1]);
    let batch = Event::Batch { over: true };
    assert_eq!(
        build.step(batch, &host),
        Ok(Action::Switch { partitions: 2 })
    );
    assert_eq!(build.step(Event::Done, &host), Ok(Action::Send(0)));
    spill.add_bytes(-1500, Some(0)).expect("bytes");
    // One a check: the other waits for the next batch.
    assert_eq!(
        build.step(Event::Sent { freed: true }, &host),
        Ok(Action::Done)
    );
    let host = shared.host(&[0, 1]);
    assert_eq!(build.step(batch, &host), Ok(Action::Send(1)));
}

#[test]
fn a_late_participant_catches_up_at_the_flush() {
    let shared = Shared::new(3, 1000);
    assert_eq!(shared.spill().split(2), Ok(2));
    shared.send(1);
    let mut build = Build::new(&params()).expect("parameters");
    let resident = [1, 1];
    let host = shared.host(&resident);
    assert_eq!(build.step(Event::Done, &host), Ok(Action::Attach));
    // Its catch-up makes its level and writes what went to disk, and
    // sends nothing, past the budget though the chunks are.
    shared.spill().add_bytes(1500, Some(0)).expect("bytes");
    let attached = Event::Attached {
        phase: crate::phases::FLUSH,
    };
    assert_eq!(
        build.step(attached, &host),
        Ok(Action::Switch { partitions: 2 })
    );
    assert_eq!(build.step(Event::Done, &host), Ok(Action::Demote(1)));
    let host = shared.host(&[1, 0]);
    assert_eq!(build.step(Event::Done, &host), Ok(Action::Keep(0)));
    assert_eq!(build.step(Event::Done, &host), Ok(Action::Write(1)));
    assert_eq!(build.step(Event::Done, &host), Ok(Action::FlushEnd));
    assert_eq!(build.step(Event::Done, &host), Ok(Action::ArriveAndWait));
}

#[test]
fn a_late_participant_catches_up_at_the_outer_side_and_the_probe() {
    use Action::*;
    let shared = Shared::new(3, 1 << 20);
    assert_eq!(shared.spill().split(4), Ok(4));
    shared.send(1);
    // Its level and what went to disk before the work of either phase.
    assert_eq!(
        alone(&shared, &[1, 1, 1, 1], OUTER_PHASE),
        [
            Attach,
            Switch { partitions: 4 },
            Demote(1),
            Outer,
            ArriveAndWait,
            Probe { split: true },
            ArriveAndDetach,
            Free
        ]
    );
    assert_eq!(
        alone(&shared, &[1, 1, 1, 1], crate::phases::PROBE),
        [
            Attach,
            Switch { partitions: 4 },
            Demote(1),
            Probe { split: true },
            ArriveAndDetach,
            Free
        ]
    );
}

/// A machine that probed a table split into 4, partitions 1, 2 and 3 on
/// disk, and left it: ready to go round them.
fn walking(shared: &Shared, params: &Params) -> Build {
    let mut build = Build::new(params).expect("parameters");
    let resident = [1, 0, 0, 0];
    let host = shared.host(&resident);
    let mut event = Event::Done;
    let mut phase = crate::phases::PROBE;
    loop {
        let action = build.step(event, &host).expect("a step");
        event = match action {
            Action::Attach => Event::Attached { phase },
            Action::ArriveAndDetach => Event::Left { last: false },
            Action::Finished => return build,
            Action::ArriveAndWait => {
                phase += 1;
                Event::Arrived { elected: false }
            }
            _ => Event::Done,
        };
    }
}

#[test]
fn the_partitions_on_disk_are_joined_together_alone_or_released() {
    let shared = Shared::new(3, 1 << 20);
    let spill = shared.spill();
    assert_eq!(spill.split(4), Ok(4));
    for partition in [1, 2, 3] {
        shared.send(partition);
    }
    spill.add_records(2, 5).expect("records");
    spill.add_records(3, 1).expect("records");
    spill.set_together(1).expect("a partition");
    let none = shared.host(&[1, 0, 0, 0]);
    // A join that keeps no inner row: one with no outer rows goes.
    let mut first = walking(&shared, &params());
    let mut steps = Vec::new();
    let mut event = Event::Next;
    loop {
        let action = first.step(event, &none).expect("a step");
        steps.push(action);
        event = match action {
            // Partition 3 has no outer rows.
            Action::Visit(partition) => Event::Visited {
                outer: partition != 3,
            },
            Action::Walked => break,
            _ => Event::Next,
        };
    }
    // From its start, 0: partition 1 in a round, 2 alone, 3 released.
    assert_eq!(
        steps,
        [
            Action::Visit(1),
            Action::Round(1),
            Action::Visit(2),
            Action::Alone(2),
            Action::Visit(3),
            Action::Walked
        ]
    );
    // Another, which keeps its inner rows: partition 1, with no outer
    // rows and no records, goes; 2 another took; 3, with records, is
    // taken with no visit of its outer files.
    let mut second = walking(
        &shared,
        &Params {
            preserve: 1,
            ..params()
        },
    );
    let mut steps = Vec::new();
    let mut event = Event::Next;
    loop {
        let action = second.step(event, &none).expect("a step");
        steps.push(action);
        event = match action {
            Action::Visit(_) => Event::Visited { outer: false },
            Action::Walked => break,
            _ => Event::Next,
        };
    }
    // Its start is 1.
    assert_eq!(steps, [Action::Visit(1), Action::Alone(3), Action::Walked]);
    assert_eq!(wrong(second.step(Event::Next, &none)), Rule::JoinEvent);
}

#[test]
fn the_size_marks_the_partitions_a_round_joins_together() {
    let shared = Shared::new(3, 1 << 20);
    let spill = shared.spill();
    assert_eq!(spill.split(4), Ok(4));
    for partition in [0, 1, 2, 3] {
        shared.send(partition);
    }
    let mut build = Build::new(&params()).expect("parameters");
    let host = shared.host(&[0, 0, 0, 0]);
    assert_eq!(build.step(Event::Done, &host), Ok(Action::Attach));
    let attached = Event::Attached {
        phase: crate::phases::SIZE - 1,
    };
    // Attached at the flush, then elected for the size.
    assert_eq!(
        build.step(attached, &host),
        Ok(Action::Switch { partitions: 4 })
    );
    let weighed = |bytes, blocks| Event::Weighed {
        bytes,
        blocks,
        index: 200,
    };
    let mut event = Event::Done;
    let mut seen = Vec::new();
    loop {
        let action = build.step(event, &host).expect("a step");
        event = match action {
            Action::ArriveAndWait if seen.is_empty() => Event::Arrived { elected: true },
            Action::ArriveAndWait => break,
            // Partition 0 fits; 1 has too many blocks; 2 passes the limit;
            // 3 has no blocks.
            Action::Weigh(partition) => {
                seen.push(partition);
                match partition {
                    0 => weighed(800, 4),
                    1 => weighed(100, 5),
                    2 => weighed(801, 1),
                    _ => weighed(0, 0),
                }
            }
            _ => Event::Done,
        };
    }
    assert_eq!(seen, [0, 1, 2, 3]);
    let together: Vec<bool> = (0..4).map(|p| spill.together(p).expect("p")).collect();
    assert_eq!(together, [true, false, false, false]);
}

#[test]
fn a_level_of_one_partition_is_a_level() {
    let shared = Shared::new(0, 1000);
    let mut build = building(&shared, &params());
    assert_eq!(shared.spill().split(1), Ok(1));
    assert_eq!(
        build.step(Event::Batch { over: false }, &shared.host(&[])),
        Ok(Action::Switch { partitions: 1 })
    );
}

#[test]
fn a_table_past_its_budget_splits_once_its_plan_is_published() {
    let shared = Shared::new(0, 1000);
    let mut first = building(&shared, &params());
    let mut second = building(&shared, &params());
    let none = shared.host(&[]);
    // Under the budget, nothing; past it, both would split.
    assert_eq!(
        first.step(Event::Batch { over: false }, &none),
        Ok(Action::Done)
    );
    let over = Event::Batch { over: true };
    assert_eq!(first.step(over, &none), Ok(Action::Split));
    assert_eq!(second.step(over, &none), Ok(Action::Split));
    // The table splits only once a plan is published: the first holds.
    assert_eq!(shared.spill().partitions(), 0);
    assert_eq!(
        first.step(Event::Planned { partitions: 4 }, &none),
        Ok(Action::Switch { partitions: 4 })
    );
    assert_eq!(
        second.step(Event::Planned { partitions: 8 }, &none),
        Ok(Action::Switch { partitions: 4 })
    );
    assert_eq!(shared.spill().partitions(), 4);
    // Each makes its level, then checks the spill: nothing past the budget.
    let host = shared.host(&[1, 1, 1, 1]);
    assert_eq!(first.step(Event::Done, &host), Ok(Action::Done));
    // A split into a count the words do not take is refused.
    let shared = Shared::new(0, 1000);
    let none = shared.host(&[]);
    let mut build = building(&shared, &params());
    assert_eq!(build.step(over, &none), Ok(Action::Split));
    let splitting = build;
    assert_eq!(
        wrong(build.step(Event::Planned { partitions: 3 }, &none)),
        Rule::Partitions
    );
    assert_eq!(build, splitting);
    assert_eq!(shared.spill().partitions(), 0);
}

#[test]
fn the_machine_refuses_what_is_no_step() -> Result<()> {
    let bad = [
        Params {
            start: f64::NAN,
            ..params()
        },
        Params {
            spilled: -1.0,
            ..params()
        },
        Params {
            preserve: 2,
            ..params()
        },
    ];
    for params in bad {
        assert_eq!(wrong(Build::new(&params)), Rule::JoinParams, "{params:?}");
    }
    let shared = Shared::new(0, 1000);
    let host = shared.host(&[]);
    let mut build = Build::new(&params())?;
    let made = build;
    // Before its first step, only the start.
    for event in [
        Event::Attached { phase: 0 },
        Event::Arrived { elected: true },
        Event::Left { last: true },
        Event::Sent { freed: true },
    ] {
        assert_eq!(
            wrong(build.step(event, &host)),
            Rule::JoinEvent,
            "{event:?}"
        );
        assert_eq!(build, made);
    }
    let mut build = building(&shared, &params());
    let built = build;
    // A plan no split asked for, and an attach past the last phase.
    assert_eq!(
        wrong(build.step(Event::Planned { partitions: 4 }, &host)),
        Rule::JoinEvent
    );
    assert_eq!(build, built);
    let mut late = Build::new(&params())?;
    assert_eq!(late.step(Event::Done, &host)?, Action::Attach);
    let before = late;
    let past = Event::Attached { phase: FREE + 1 };
    assert_eq!(wrong(late.step(past, &host)), Rule::AttachPhase);
    assert_eq!(late, before);
    // Flags of another number of partitions than the table's.
    assert_eq!(shared.spill().split(4), Ok(4));
    let short = shared.host(&[1, 1]);
    let batch = Event::Batch { over: false };
    assert_eq!(
        build.step(batch, &short),
        Ok(Action::Switch { partitions: 4 })
    );
    let switching = build;
    assert_eq!(wrong(build.step(Event::Done, &short)), Rule::ResidentFlags);
    assert_eq!(build, switching);
    // Words that are no machine.
    for mut broken in [
        Build { given: 99, ..built },
        Build { stage: 7, ..built },
        Build { evict: 2, ..built },
        Build {
            switched: 2,
            ..built
        },
        Build { walk: 2, ..built },
        Build { spare: 1, ..built },
        Build {
            params: Params {
                target: f64::INFINITY,
                ..params()
            },
            ..built
        },
    ] {
        let before = broken;
        assert_eq!(
            wrong(broken.step(Event::Done, &host)),
            Rule::JoinState,
            "{before:?}"
        );
        assert_eq!(broken, before);
    }
    Ok(())
}

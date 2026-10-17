//! The phases of a shared build, the rounds over a partition on disk and
//! the words of a spill under loom. Participants run the steps of
//! [`crate::phases`] over a model of PostgreSQL's `Barrier`, and the table
//! they share is the model table of [`crate::table::loom`], which runs the
//! table's own code over loom's atomics; the words of a spill are loom
//! atomics with the orderings of [`crate::shared_spill`]. Run with `make
//! rust-loom`.
#![allow(unsafe_code)]

use ::loom::sync::Arc;
use ::loom::sync::atomic::AtomicU64;
use ::loom::thread;
use anyhow::Result;
use core::sync::atomic::Ordering;

use crate::phases::{self, Action, COUNTER_WORDS, CounterWords, Counters, Participant, Stop};
use crate::shared_spill::{self, Memory, Spill, Weights, Words};
use crate::table::loom::ModelTable;

/// PostgreSQL's `Barrier` (`storage/ipc/barrier.c`) over loom: the same
/// counts, phase and election, a mutex for its spinlock and a condition
/// variable for its own. `skip` makes arrivals in one phase return at
/// once, for the test that the model notices a missing wait.
struct LoomBarrier {
    state: ::loom::sync::Mutex<BarrierState>,
    released: ::loom::sync::Condvar,
    skip: Option<u32>,
}

struct BarrierState {
    participants: u32,
    arrived: u32,
    phase: u32,
    elected: u32,
}

impl LoomBarrier {
    fn new(skip: Option<u32>) -> Self {
        Self {
            state: ::loom::sync::Mutex::new(BarrierState {
                participants: 0,
                arrived: 0,
                phase: 0,
                elected: 0,
            }),
            released: ::loom::sync::Condvar::new(),
            skip,
        }
    }

    fn attach(&self) -> u32 {
        let mut state = self.state.lock().unwrap();
        state.participants += 1;
        state.phase
    }

    fn arrive_and_wait(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        let next = state.phase + 1;
        let skipping = self.skip == Some(state.phase);
        state.arrived += 1;
        if state.arrived == state.participants {
            state.arrived = 0;
            state.phase = next;
            state.elected = next;
            drop(state);
            self.released.notify_all();
            return true;
        }
        if skipping {
            return false;
        }
        loop {
            if state.phase == next {
                if state.elected != next {
                    state.elected = next;
                    return true;
                }
                return false;
            }
            state = self.released.wait(state).unwrap();
        }
    }

    fn detach(&self, arrive: bool) -> bool {
        let mut state = self.state.lock().unwrap();
        state.participants -= 1;
        let release = (arrive || state.participants > 0) && state.arrived == state.participants;
        if release {
            state.arrived = 0;
            state.phase += 1;
        }
        let last = state.participants == 0;
        drop(state);
        if release {
            self.released.notify_all();
        }
        last
    }
}

/// The words of a build's counters as loom atomics, with the orderings
/// of the real words, so that the model runs the real counters.
struct LoomCounterWords([AtomicU64; COUNTER_WORDS]);

impl CounterWords for LoomCounterWords {
    fn load(&self, index: usize) -> u64 {
        self.0[index].load(phases::order::RELAXED)
    }
    fn store(&self, index: usize, value: u64) {
        self.0[index].store(value, phases::order::RELAXED);
    }
    fn fetch_or(&self, index: usize, bits: u64) -> u64 {
        self.0[index].fetch_or(bits, phases::order::RELAXED)
    }
    fn compare_exchange(&self, index: usize, current: u64, new: u64) -> Result<u64, u64> {
        self.0[index].compare_exchange(current, new, phases::order::RELAXED, phases::order::RELAXED)
    }
}

/// The stop word of a table over a loom atomic, relaxed as the real one.
struct LoomStop(AtomicU64);

impl Stop for LoomStop {
    fn stop(&self) {
        self.0.store(1, phases::order::RELAXED);
    }

    fn stopped(&self) -> bool {
        self.0.load(phases::order::RELAXED) != 0
    }
}

/// A participant that stops while it probes: its number, and whether the
/// model marks the stop word only after it left, the mistake the model
/// must notice.
#[derive(Clone, Copy)]
struct Stopper {
    chunk: usize,
    late: bool,
}

/// A shared build: the inner side's keys, which the participants take
/// one at a time as a parallel scan hands out pages, the model table with
/// a chunk per participant, the counters and the chunk numbers they gave,
/// the barrier and the frees; the stop word, whether a participant marked
/// it, and what the last one to leave read of it.
struct Build {
    keys: &'static [i32],
    next_key: ::loom::sync::atomic::AtomicUsize,
    table: ModelTable,
    counters: Counters<LoomCounterWords>,
    numbers: AtomicU64,
    barrier: LoomBarrier,
    frees: ::loom::sync::atomic::AtomicUsize,
    stopper: Option<Stopper>,
    stop: LoomStop,
    marked: ::loom::sync::atomic::AtomicBool,
    last_saw_stop: ::loom::sync::atomic::AtomicBool,
}

impl Build {
    fn new(keys: &'static [i32], participants: usize, skip: Option<u32>) -> Self {
        Self {
            keys,
            next_key: ::loom::sync::atomic::AtomicUsize::new(0),
            table: ModelTable::new(keys.len(), participants).unwrap(),
            counters: Counters::over(LoomCounterWords(core::array::from_fn(|_| {
                AtomicU64::new(0)
            }))),
            numbers: AtomicU64::new(0),
            barrier: LoomBarrier::new(skip),
            frees: ::loom::sync::atomic::AtomicUsize::new(0),
            stopper: None,
            stop: LoomStop(AtomicU64::new(0)),
            marked: ::loom::sync::atomic::AtomicBool::new(false),
            last_saw_stop: ::loom::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Take a chunk number, which no other participant may get; take keys
    /// of the inner side until none is left and append each to this
    /// participant's chunk, then report how many.
    fn build(&self, chunk: usize) {
        let number = self.counters.take_chunk().unwrap();
        let bit = 1 << number;
        let given = self.numbers.fetch_or(bit, Ordering::Relaxed);
        assert_eq!(given & bit, 0, "chunk number {number} was given twice");
        let mut appended = 0;
        loop {
            let index = self.next_key.fetch_add(1, Ordering::Relaxed);
            let Some(&key) = self.keys.get(index) else {
                break;
            };
            appended += self.table.append(chunk, &[key]).unwrap();
        }
        self.counters.report(appended as u64, 1 << chunk).unwrap();
    }

    /// Run participant `chunk`, which appends to the chunk of its number,
    /// checking that its probes find every key.
    fn participate(&self, chunk: usize) {
        let mut participant = Participant::new();
        let mut reply = 0;
        let stopper = self.stopper.filter(|stopper| stopper.chunk == chunk);
        let mut mark_after_leaving = false;
        loop {
            let action = participant.next(&self.counters, reply).unwrap();
            reply = 0;
            match action {
                Action::Attach => reply = self.barrier.attach(),
                Action::ArriveAndWait => reply = u32::from(self.barrier.arrive_and_wait()),
                Action::Build => self.build(chunk),
                // No spill in the model: nothing to write.
                Action::Flush | Action::Outer => {}
                Action::Allocate | Action::Load => panic!("a build got {action:?}"),
                Action::Size => {
                    // SAFETY: the elected one alone uses the index now.
                    unsafe { self.table.size(self.counters.total_records()) }.unwrap();
                }
                Action::Link => {
                    self.table.link(chunk).unwrap();
                }
                // It stops before it probes: marked first, or late after it
                // left, then it leaves as the next step says.
                Action::Probe if stopper.is_some_and(|stopper| stopper.late) => {
                    mark_after_leaving = true;
                }
                Action::Probe if stopper.is_some() => {
                    let marked = participant.stop_in(&self.stop);
                    self.marked.store(marked, Ordering::Relaxed);
                }
                Action::Probe => {
                    let found = self.table.probe(self.keys).unwrap();
                    for (key, found) in self.keys.iter().zip(found) {
                        assert!(found, "key {key} was lost");
                    }
                }
                Action::ArriveAndDetach => {
                    reply = u32::from(self.barrier.detach(true));
                    if mark_after_leaving {
                        self.stop.stop();
                        self.marked.store(true, Ordering::Relaxed);
                    }
                }
                Action::Detach => {
                    self.barrier.detach(false);
                }
                Action::Free => {
                    self.frees.fetch_add(1, Ordering::Relaxed);
                    self.last_saw_stop
                        .store(self.stop.stopped(), Ordering::Relaxed);
                    return;
                }
                Action::Done => return,
            }
        }
    }
}

/// `participants` build one table of `keys` and probe it; exactly one
/// frees it, the chunks are numbered from 0 once each, and every record
/// is counted.
fn shared_build(participants: usize, keys: &'static [i32], skip: Option<u32>, preemptions: usize) {
    let mut model = ::loom::model::Builder::new();
    model.preemption_bound = Some(preemptions);
    model.max_branches = 100_000;
    model.check(move || {
        let build = Arc::new(Build::new(keys, participants, skip));
        let threads: Vec<_> = (0..participants)
            .map(|chunk| {
                let build = build.clone();
                thread::spawn(move || build.participate(chunk))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            build.frees.load(Ordering::Relaxed),
            1,
            "the table is freed once"
        );
        assert_eq!(
            build.numbers.load(Ordering::Relaxed),
            (1 << build.counters.total_chunks()) - 1,
            "the chunks numbered from 0, once each"
        );
        assert_eq!(build.counters.total_records(), keys.len() as u64);
    });
}

/// `participants` build one table, and the first stops while it probes:
/// in every order of their steps, the last one to leave the table sees
/// its mark whenever it made one, and returns no record without a mark.
fn a_participant_stops(participants: usize, late: bool) {
    let mut model = ::loom::model::Builder::new();
    model.preemption_bound = Some(2);
    model.max_branches = 100_000;
    model.check(move || {
        let mut build = Build::new(&[1, 2], participants, None);
        build.stopper = Some(Stopper { chunk: 0, late });
        let build = Arc::new(build);
        let threads: Vec<_> = (0..participants)
            .map(|chunk| {
                let build = build.clone();
                thread::spawn(move || build.participate(chunk))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            build.frees.load(Ordering::Relaxed),
            1,
            "the table is freed once"
        );
        if build.marked.load(Ordering::Relaxed) {
            assert!(
                build.last_saw_stop.load(Ordering::Relaxed),
                "the last participant missed one that stopped"
            );
        }
    });
}

#[test]
fn the_last_participant_sees_one_that_stopped_while_it_probed() {
    a_participant_stops(2, false);
    a_participant_stops(3, false);
}

#[test]
#[should_panic(expected = "missed one that stopped")]
fn a_stop_marked_after_leaving_goes_unseen() {
    a_participant_stops(2, true);
}

#[test]
fn two_participants_append_size_link_and_probe() {
    shared_build(2, &[1, 2, 3], None, 3);
}

#[test]
fn three_participants_attach_at_any_phase() {
    shared_build(3, &[1, 2, 3], None, 2);
}

#[test]
#[should_panic(expected = "does not hold a table")]
fn linking_before_the_index_is_made_breaks_the_table() {
    shared_build(2, &[1, 2, 3], Some(phases::SIZE), 2);
}

/// The words of a shared spill as loom atomics, for `capacity`
/// partitions and the resident ones.
#[derive(Debug)]
struct LoomWords {
    words: Vec<AtomicU64>,
    capacity: usize,
}

/// The shared state of a spill over loom atomics.
type LoomSpill = Spill<LoomWords>;

/// A cleared spill state for `capacity` partitions and a budget.
fn loom_spill(capacity: usize, budget: u64) -> LoomSpill {
    let words = (0..shared_spill::words_for(capacity).unwrap())
        .map(|_| AtomicU64::new(0))
        .collect();
    let spill = Spill::over(LoomWords { words, capacity });
    spill.init(budget);
    spill
}

impl Words for LoomWords {
    fn load(&self, index: usize) -> u64 {
        self.words[index].load(shared_spill::order::LOAD)
    }
    fn store(&self, index: usize, value: u64) {
        self.words[index].store(value, shared_spill::order::STORE);
    }
    fn fetch_add(&self, index: usize, delta: u64) -> u64 {
        self.words[index].fetch_add(delta, shared_spill::order::ADD)
    }
    fn fetch_sub(&self, index: usize, delta: u64) -> u64 {
        self.words[index].fetch_sub(delta, shared_spill::order::ADD)
    }
    fn fetch_or(&self, index: usize, bits: u64) -> u64 {
        self.words[index].fetch_or(bits, shared_spill::order::CAS)
    }
    fn compare_exchange(&self, index: usize, current: u64, new: u64) -> Result<u64, u64> {
        self.words[index].compare_exchange(
            current,
            new,
            shared_spill::order::CAS,
            shared_spill::order::CAS_FAILED,
        )
    }
    fn capacity(&self) -> usize {
        self.capacity
    }
}

#[test]
fn participants_past_the_budget_agree_on_one_split() {
    ::loom::model(|| {
        let spill = Arc::new(loom_spill(8, 100));
        let threads: Vec<_> = [4_u32, 8]
            .into_iter()
            .map(|wanted| {
                let spill = spill.clone();
                thread::spawn(move || {
                    let over = spill.add_bytes(60, None).unwrap();
                    // Whoever sees the budget passed splits; the first holds.
                    if over || spill.partitions() == 0 {
                        spill.split(wanted).unwrap()
                    } else {
                        spill.partitions()
                    }
                })
            })
            .collect();
        let seen: Vec<u32> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let partitions = spill.partitions();
        assert!(partitions == 4 || partitions == 8);
        assert!(
            seen.iter().all(|&p| p == partitions),
            "{seen:?} split apart"
        );
    });
}

/// A shared table's weights: past its budget, one partition a check.
const SHARED: Weights = Weights {
    start: 1.0,
    target: 1.0,
    spilled: 0.0,
    reserve: 0,
    resident: 0.0,
    per_check: 1,
};

#[test]
fn a_partition_goes_to_disk_once() {
    ::loom::model(|| {
        let spill = Arc::new(loom_spill(4, 10));
        spill.split(4).unwrap();
        spill.add_bytes(30, Some(1)).unwrap();
        spill.add_bytes(20, Some(2)).unwrap();
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let spill = spill.clone();
                thread::spawn(move || spill.evict(&SHARED, Memory::Counted, None, 0).unwrap())
            })
            .collect();
        let marked: Vec<u32> = threads
            .into_iter()
            .filter_map(|t| t.join().unwrap())
            .collect();
        assert!(marked.contains(&1), "the largest went to disk");
        assert!(
            marked.len() < 2 || marked[0] != marked[1],
            "one partition marked twice"
        );
        assert!(spill.on_disk(1).unwrap());
    });
}

/// A round over one partition: its inner rows in files, which the
/// participants take one at a time and load into chunks of their own of
/// the model table, the barrier and the frees.
struct Round {
    files: &'static [&'static [i32]],
    keys: usize,
    spill: LoomSpill,
    table: ModelTable,
    barrier: LoomBarrier,
    frees: ::loom::sync::atomic::AtomicUsize,
    probes: ::loom::sync::atomic::AtomicUsize,
    loads: ::loom::sync::atomic::AtomicUsize,
}

impl Round {
    fn new(files: &'static [&'static [i32]], participants: usize, skip: Option<u32>) -> Self {
        let keys = files.iter().map(|file| file.len()).sum();
        let spill = loom_spill(1, 0);
        spill.split(1).unwrap();
        Self {
            files,
            keys,
            spill,
            table: ModelTable::new(keys, participants).unwrap(),
            barrier: LoomBarrier::new(skip),
            frees: ::loom::sync::atomic::AtomicUsize::new(0),
            probes: ::loom::sync::atomic::AtomicUsize::new(0),
            loads: ::loom::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Participant `chunk` of the round, loading into the chunk of its
    /// number; its probe finds every key of every file.
    fn participate(&self, chunk: usize) {
        let mut participant = Participant::new();
        let mut reply = 0;
        loop {
            let action = participant.round_step(reply).unwrap();
            reply = 0;
            match action {
                Action::Attach => reply = self.barrier.attach(),
                Action::ArriveAndWait => reply = u32::from(self.barrier.arrive_and_wait()),
                Action::Allocate => {
                    // SAFETY: the elected one alone uses the index now.
                    unsafe { self.table.size(self.keys as u64) }.unwrap();
                }
                Action::Load => {
                    let mut loaded = false;
                    loop {
                        let file = self.spill.take_file(0, false).unwrap() as usize;
                        let Some(keys) = self.files.get(file) else {
                            break;
                        };
                        self.table.append(chunk, keys).unwrap();
                        self.loads.fetch_add(1, Ordering::Relaxed);
                        loaded = true;
                    }
                    if loaded {
                        self.table.link(chunk).unwrap();
                    }
                }
                Action::Probe => {
                    for file in self.files {
                        let found = self.table.probe(file).unwrap();
                        for (key, found) in file.iter().zip(found) {
                            assert!(found, "key {key} was not loaded");
                        }
                    }
                    self.probes.fetch_add(1, Ordering::Relaxed);
                }
                Action::ArriveAndDetach => reply = u32::from(self.barrier.detach(true)),
                Action::Detach => {
                    self.barrier.detach(false);
                }
                Action::Free => {
                    self.frees.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Action::Done => return,
                other => panic!("a round got {other:?}"),
            }
        }
    }
}

/// `participants` join a round over a partition of `files` at any time;
/// every file is loaded once, the probes see every key, one frees it.
fn round(
    participants: usize,
    files: &'static [&'static [i32]],
    skip: Option<u32>,
    preemptions: usize,
) {
    let mut model = ::loom::model::Builder::new();
    model.preemption_bound = Some(preemptions);
    model.max_branches = 100_000;
    model.check(move || {
        let round = Arc::new(Round::new(files, participants, skip));
        let threads: Vec<_> = (0..participants)
            .map(|chunk| {
                let round = round.clone();
                thread::spawn(move || round.participate(chunk))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            round.frees.load(Ordering::Relaxed),
            1,
            "the round is freed once"
        );
        assert!(round.probes.load(Ordering::Relaxed) >= 1, "someone probed");
        assert_eq!(
            round.loads.load(Ordering::Relaxed),
            files.len(),
            "every file loaded once"
        );
    });
}

/// Two participants take the files of a partition, and the partition
/// whole, at once: each file goes to one of them, and the partition to
/// one.
#[test]
fn files_and_a_whole_partition_go_to_one_participant_each() {
    ::loom::model(|| {
        let spill = Arc::new(loom_spill(2, 0));
        spill.split(2).unwrap();
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let spill = spill.clone();
                thread::spawn(move || {
                    let file = spill.take_file(1, false).unwrap();
                    (file, spill.take_alone(1).unwrap())
                })
            })
            .collect();
        let taken: Vec<(u32, bool)> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let mut files: Vec<u32> = taken.iter().map(|&(file, _)| file).collect();
        files.sort_unstable();
        assert_eq!(files, [0, 1], "each file to one participant");
        assert_eq!(
            taken.iter().filter(|&&(_, alone)| alone).count(),
            1,
            "the partition to one"
        );
        assert!(spill.alone(1).unwrap());
    });
}

#[test]
fn two_participants_load_a_partition_and_probe_it() {
    round(2, &[&[1, 2], &[3]], None, 3);
}

#[test]
fn three_participants_join_a_round_at_any_phase() {
    round(3, &[&[1], &[2], &[3]], None, 2);
}

#[test]
#[should_panic(expected = "was not loaded")]
fn probing_before_every_file_is_loaded_misses_keys() {
    round(2, &[&[1], &[2]], Some(phases::ROUND_LOAD), 2);
}

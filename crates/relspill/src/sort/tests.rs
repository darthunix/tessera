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

const ROW: u64 = 16;
const ITEM: u64 = 12;
const CHUNK: u64 = 1024;

fn params(limit: u64) -> Params {
    Params {
        limit,
        row: ROW,
        item: ITEM,
        chunk: CHUNK,
        set_runs: 4,
        max_merge: 8,
        batch_rows: 64,
        backward: 0,
    }
}

/// What a sort through the machine did.
#[derive(Debug, Default)]
struct Ran {
    runs: usize,
    sets: usize,
    passes: usize,
    merges: usize,
    keeps: usize,
    read: u32,
}

/// The host's runs: each set a file of the runs it holds, a run read
/// and freed taken out of it.
#[derive(Default)]
struct Host {
    /// Each set's runs, and the pass that wrote it.
    sets: Vec<(Vec<Option<Vec<u32>>>, usize)>,
    /// Whether the last set is being written.
    open: bool,
    /// The runs, and those of the pass being read, as (set, partition).
    runs: Vec<(usize, usize)>,
    pass: Vec<(usize, usize)>,
    passes: usize,
    ran: Ran,
}

impl Host {
    /// Write `rows`, sorted, as a run at `place`, after the others.
    fn write(&mut self, params: &Params, place: Place, mut rows: Vec<u32>) {
        rows.sort_unstable();
        if place.create {
            self.sets.push((Vec::new(), self.passes));
            self.open = true;
            self.ran.sets += 1;
        }
        assert!(self.open, "a run in a set no longer written");
        let set = self.sets.len() - 1;
        let (runs, pass) = &mut self.sets[set];
        assert_eq!(*pass, self.passes, "a set of two passes");
        assert_eq!(place.partition as usize, runs.len(), "a partition skipped");
        assert!(place.partition < params.set_runs, "a set past its runs");
        runs.push(Some(rows));
        self.runs.push((set, place.partition as usize));
        self.ran.runs += 1;
    }

    /// Run `run` of the pass, read and freed.
    fn take(&mut self, run: u32) -> Vec<u32> {
        let (set, partition) = self.pass[run as usize];
        self.sets[set].0[partition]
            .take()
            .expect("a run read twice")
    }
}

/// Merge sorted runs.
fn merge(runs: Vec<Vec<u32>>) -> Vec<u32> {
    let mut rows: Vec<u32> = runs.into_iter().flatten().collect();
    rows.sort_unstable();
    rows
}

/// Sort `keys` through the machine, `batch` rows at a time, and give the
/// rows read and what ran.
fn sort_through(params: &Params, keys: &[u32], batch: usize) -> Result<(Vec<u32>, Ran)> {
    let (mut machine, blocks) = Runs::new(params)?;
    let mut host = Host::default();
    let mut memory: Vec<u32> = Vec::new();
    for rows in keys.chunks(batch) {
        memory.extend_from_slice(rows);
        let count = memory.len() as u64;
        let grown = Event::Grown {
            memory: count * params.row,
            rows: count,
            limit: params.limit,
        };
        match machine.step(grown)? {
            Action::Done => {
                let bytes = count * (params.row + params.item) + params.chunk;
                assert!(bytes <= params.limit, "{bytes} past {}", params.limit);
            }
            Action::Run(place) => host.write(params, place, core::mem::take(&mut memory)),
            action => panic!("{action:?} for a batch"),
        }
    }
    let end = Event::InputEnd {
        rows: memory.len() as u64,
    };
    let mut action = machine.step(end)?;
    if action == Action::Done {
        assert_eq!(host.ran.runs, 0, "runs left unread");
        memory.sort_unstable();
        return Ok((memory, host.ran));
    }
    loop {
        match action {
            Action::Run(place) => host.write(params, place, core::mem::take(&mut memory)),
            Action::Pass => {
                host.open = false;
                host.passes += 1;
                host.pass = core::mem::take(&mut host.runs);
            }
            Action::Merge {
                first,
                count,
                place,
            } => {
                assert!((2..=blocks.fan_in).contains(&count), "a merge of {count}");
                let runs = (first..first + count).map(|run| host.take(run)).collect();
                host.write(params, place, merge(runs));
                host.ran.merges += 1;
            }
            Action::Keep { run } => {
                let kept = host.pass[run as usize];
                assert!(host.sets[kept.0].0[kept.1].is_some(), "a run kept twice");
                host.runs.push(kept);
                host.ran.keeps += 1;
            }
            Action::PassEnd => {
                for &(set, partition) in &host.pass {
                    let kept = host.runs.contains(&(set, partition));
                    assert!(kept || host.sets[set].0[partition].is_none(), "a run left");
                }
                host.ran.passes += 1;
            }
            Action::Read { runs } => {
                host.open = false;
                assert_eq!(runs as usize, host.runs.len());
                let most = if params.backward != 0 {
                    1
                } else {
                    blocks.fan_in
                };
                assert!(runs <= most, "{runs} runs read at once");
                assert_eq!(machine.step(Event::Rescan)?, Action::Read { runs });
                host.ran.read = runs;
                let runs = core::mem::take(&mut host.runs);
                let rows = runs
                    .into_iter()
                    .map(|(set, partition)| host.sets[set].0[partition].take().expect("a run"))
                    .collect();
                return Ok((merge(rows), host.ran));
            }
            Action::Done => panic!("done before the runs are read"),
        }
        action = machine.step(Event::Next)?;
    }
}

#[test]
fn a_sort_through_its_runs_gives_the_rows_of_one_in_memory() {
    let strategy = (
        prop::collection::vec(any::<u32>(), 0..20_000),
        1_u64..64,
        1_usize..200,
        any::<bool>(),
    );
    property(strategy, |(keys, kib, batch, backward)| {
        let params = Params {
            backward: u32::from(backward),
            ..params(kib * 1024)
        };
        let (rows, _) = sort_through(&params, &keys, batch)?;
        let mut expected = keys;
        expected.sort_unstable();
        assert_eq!(rows, expected);
        Ok(())
    });
}

#[test]
fn a_sort_that_spills_runs_every_path() -> Result<()> {
    let keys: Vec<u32> = (0..28_900)
        .map(|row: u32| row.wrapping_mul(2_654_435_761))
        .collect();
    let mut expected = keys.clone();
    expected.sort_unstable();
    // Runs of 320 rows, 91 of them: a pass merges them six at a time and
    // keeps the last; the next merges the 16 into 3.
    let params = params(8 * 1024);
    let (rows, ran) = sort_through(&params, &keys, 64)?;
    assert_eq!(rows, expected);
    assert!(
        ran.passes >= 2 && ran.merges > 0 && ran.keeps > 0,
        "{ran:?}"
    );
    assert!(ran.sets > ran.passes + 1, "{ran:?}");
    assert!(ran.read > 1, "{ran:?}");
    // Read backward: into one run.
    let backward = Params {
        backward: 1,
        ..params
    };
    let (rows, ran) = sort_through(&backward, &keys, 64)?;
    assert_eq!(rows, expected);
    assert_eq!(ran.read, 1);
    // In memory: no run.
    let (rows, ran) = sort_through(&self::params(1 << 30), &keys, 64)?;
    assert_eq!(rows, expected);
    assert_eq!(ran.runs, 0);
    Ok(())
}

#[test]
fn a_run_goes_to_disk_past_the_limit_with_what_sorting_takes() -> Result<()> {
    let limit = 64 * 1024;
    let (mut machine, _) = Runs::new(&params(limit))?;
    let rows = 1000;
    let memory = limit - CHUNK - rows * ITEM;
    assert_eq!(
        machine.step(Event::Grown {
            memory,
            rows,
            limit,
        })?,
        Action::Done
    );
    // No rows: nothing to write, whatever the bytes.
    let empty = Event::Grown {
        memory: u64::MAX,
        rows: 0,
        limit,
    };
    assert_eq!(machine.step(empty)?, Action::Done);
    let past = Event::Grown {
        memory: memory + 1,
        rows,
        limit,
    };
    let first = Place {
        create: true,
        partition: 0,
    };
    assert_eq!(machine.step(past)?, Action::Run(first));
    // Counts that overflow saturate past the limit.
    let huge = Event::Grown {
        memory: 0,
        rows: u64::MAX,
        limit,
    };
    let second = Place {
        create: false,
        partition: 1,
    };
    assert_eq!(machine.step(huge)?, Action::Run(second));
    Ok(())
}

#[test]
fn the_blocks_follow_the_limit() -> Result<()> {
    let blocks = |limit, max_merge| {
        let params = Params {
            max_merge,
            ..params(limit)
        };
        Runs::new(&params).map(|(_, blocks)| blocks)
    };
    // A 128th of 4 MiB: 2048 rows of 16 bytes, and 64 runs a merge.
    let four = Blocks {
        rows: 2048,
        values: 32 * 1024,
        fan_in: 64,
    };
    assert_eq!(blocks(4 << 20, 256)?, four);
    // Rows of a batch at least, values of 4 KiB; six runs at least.
    let tiny = Blocks {
        rows: 64,
        values: 4096,
        fan_in: 6,
    };
    assert_eq!(blocks(1024, 256)?, tiny);
    let small = Blocks {
        rows: 64,
        values: 4096,
        fan_in: 32,
    };
    assert_eq!(blocks(64 * 1024, 256)?, small);
    // 256 KiB at most, and the most runs a merge reads.
    let large = Blocks {
        rows: 16 * 1024,
        values: 256 * 1024,
        fan_in: 256,
    };
    assert_eq!(blocks(1 << 30, 256)?, large);
    assert_eq!(blocks(1024, 2)?.fan_in, 2);
    Ok(())
}

#[test]
fn a_batch_weighs_its_rows_by_the_limit_it_brings() -> Result<()> {
    // Planned at 1 KiB, the sort may take 1 MiB by its first batch: its
    // rows weigh by that.
    let (mut machine, blocks) = Runs::new(&params(1024))?;
    let grown = |limit| Event::Grown {
        memory: 4096,
        rows: 10,
        limit,
    };
    assert_eq!(machine.step(grown(1 << 20))?, Action::Done);
    assert!(matches!(machine.step(grown(1024))?, Action::Run(_)));
    // Its blocks stay those of its start.
    assert_eq!(blocks.fan_in, 6);
    Ok(())
}

#[test]
fn as_many_runs_as_a_merge_reads_are_read_with_no_pass() -> Result<()> {
    let params = params(1024);
    let (mut machine, blocks) = Runs::new(&params)?;
    let grown = Event::Grown {
        memory: 1024,
        rows: 1,
        limit: params.limit,
    };
    for _ in 1..blocks.fan_in {
        assert!(matches!(machine.step(grown)?, Action::Run(_)));
    }
    let end = Event::InputEnd { rows: 1 };
    assert!(matches!(machine.step(end)?, Action::Run(_)));
    let runs = blocks.fan_in;
    assert_eq!(machine.step(Event::Next)?, Action::Read { runs });
    Ok(())
}

#[test]
fn runs_fill_a_set_then_another() -> Result<()> {
    let params = Params {
        set_runs: 2,
        ..params(1024)
    };
    let (mut machine, _) = Runs::new(&params)?;
    let grown = Event::Grown {
        memory: 1024,
        rows: 1,
        limit: params.limit,
    };
    let places: Vec<Action> = (0..3).map(|_| machine.step(grown)).collect::<Result<_>>()?;
    let place = |create, partition| Action::Run(Place { create, partition });
    assert_eq!(places, [place(true, 0), place(false, 1), place(true, 0)]);
    // The input's last run goes after them; a pass writes a set of its
    // own.
    let end = Event::InputEnd { rows: 1 };
    assert_eq!(machine.step(end)?, place(false, 1));
    assert_eq!(machine.step(Event::Next)?, Action::Read { runs: 4 });
    Ok(())
}

#[test]
fn passes_merge_until_one_merge_reads_every_run() -> Result<()> {
    let params = params(1024);
    let (mut machine, blocks) = Runs::new(&params)?;
    assert_eq!(blocks.fan_in, 6);
    let grown = Event::Grown {
        memory: 1024,
        rows: 1,
        limit: params.limit,
    };
    for _ in 0..12 {
        assert!(matches!(machine.step(grown)?, Action::Run(_)));
    }
    let end = Event::InputEnd { rows: 1 };
    assert!(matches!(machine.step(end)?, Action::Run(_)));
    let mut step = || machine.step(Event::Next);
    assert_eq!(step()?, Action::Pass);
    let merge = |first, partition| Action::Merge {
        first,
        count: 6,
        place: Place {
            create: partition == 0,
            partition,
        },
    };
    assert_eq!(step()?, merge(0, 0));
    assert_eq!(step()?, merge(6, 1));
    assert_eq!(step()?, Action::Keep { run: 12 });
    assert_eq!(step()?, Action::PassEnd);
    assert_eq!(step()?, Action::Read { runs: 3 });
    assert_eq!(machine.step(Event::Rescan)?, Action::Read { runs: 3 });
    assert_eq!(machine.step(Event::Rescan)?, Action::Read { runs: 3 });
    Ok(())
}

#[test]
fn a_sort_read_backward_merges_into_one_run() -> Result<()> {
    let params = Params {
        backward: 1,
        ..params(1024)
    };
    let (mut machine, _) = Runs::new(&params)?;
    let grown = Event::Grown {
        memory: 1024,
        rows: 1,
        limit: params.limit,
    };
    assert!(matches!(machine.step(grown)?, Action::Run(_)));
    let end = Event::InputEnd { rows: 1 };
    assert!(matches!(machine.step(end)?, Action::Run(_)));
    let mut step = || machine.step(Event::Next);
    assert_eq!(step()?, Action::Pass);
    let place = Place {
        create: true,
        partition: 0,
    };
    let merge = Action::Merge {
        first: 0,
        count: 2,
        place,
    };
    assert_eq!(step()?, merge);
    assert_eq!(step()?, Action::PassEnd);
    assert_eq!(step()?, Action::Read { runs: 1 });
    // One run from the input is read as it is.
    let (mut machine, _) = Runs::new(&params)?;
    assert!(matches!(machine.step(grown)?, Action::Run(_)));
    let end = Event::InputEnd { rows: 0 };
    assert_eq!(machine.step(end)?, Action::Read { runs: 1 });
    Ok(())
}

#[test]
fn the_runs_refuse_what_is_no_step() -> Result<()> {
    let good = params(1024);
    let bad = [
        Params { limit: 0, ..good },
        Params { row: 0, ..good },
        Params {
            set_runs: 0,
            ..good
        },
        Params {
            batch_rows: 0,
            ..good
        },
        Params {
            max_merge: 1,
            ..good
        },
        Params {
            backward: 2,
            ..good
        },
    ];
    for params in bad {
        assert_eq!(wrong(Runs::new(&params)), Rule::JoinParams, "{params:?}");
    }
    let (mut machine, _) = Runs::new(&good)?;
    let made = machine;
    for event in [Event::Next, Event::Rescan] {
        assert_eq!(wrong(machine.step(event)), Rule::JoinEvent, "{event:?}");
        assert_eq!(machine, made);
    }
    // A sort that wrote no run is done at its end.
    assert_eq!(machine.step(Event::InputEnd { rows: 5 })?, Action::Done);
    let done = machine;
    let grown = Event::Grown {
        memory: 1 << 20,
        rows: 1,
        limit: good.limit,
    };
    for event in [
        grown,
        Event::Next,
        Event::Rescan,
        Event::InputEnd { rows: 0 },
    ] {
        assert_eq!(wrong(machine.step(event)), Rule::JoinEvent, "{event:?}");
        assert_eq!(machine, done);
    }
    // Words that are no machine.
    let broken = [
        Runs { phase: 0, ..made },
        Runs { phase: 7, ..made },
        Runs { fan_in: 1, ..made },
        Runs { fan_in: 9, ..made },
        Runs { open: 2, ..made },
        Runs { used: 5, ..made },
        Runs {
            done: 1,
            pass_runs: 0,
            ..made
        },
        Runs {
            params: Params { limit: 0, ..good },
            ..made
        },
    ];
    for mut machine in broken {
        let before = machine;
        assert_eq!(wrong(machine.step(grown)), Rule::JoinState, "{before:?}");
        assert_eq!(machine, before);
    }
    // A run past what a u32 counts.
    let mut full = Runs {
        runs: u32::MAX,
        ..made
    };
    assert_eq!(wrong(full.step(grown)), Rule::JoinState);
    assert_eq!(full.runs, u32::MAX);
    // A run kept past what a u32 counts.
    let mut kept = Runs {
        phase: MERGING,
        runs: u32::MAX,
        pass_runs: 1,
        done: 0,
        ..made
    };
    let before = kept;
    assert_eq!(wrong(kept.step(Event::Next)), Rule::JoinState);
    assert_eq!(kept, before);
    // At the end of the input too, the phase as it was.
    let end = Event::InputEnd { rows: 1 };
    assert_eq!(wrong(full.step(end)), Rule::JoinState);
    assert_eq!(
        full,
        Runs {
            runs: u32::MAX,
            ..made
        }
    );
    Ok(())
}

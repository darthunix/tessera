//! Benchmark runner on PMU counters; no wall time anywhere.
//!
//! Every operation is calibrated by cycles, warmed up at the chosen call
//! count, and then measured in [`BLOCKS`] blocks of that many calls. A
//! calibration whose cycles read zero is repeated as a block is, and a run
//! stops when they stay zero. Raw
//! per-block readings go to a JSONL file together with the CPU each block
//! started and ended on; statistics and limits belong to tessera-bench. A
//! block whose counters read zero is a failed counter read, not a
//! measurement: it is repeated a bounded number of times and the repeats are
//! recorded. `--list` prints operation ids and no arguments (as under
//! `cargo test --benches` or `cargo bench`) only runs the correctness checks;
//! neither opens counters, so both work without root. `--output` needs root.

use anyhow::{Context, Result, bail, ensure};
use pmu::{Counters, Reading, cpu_number};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
};

/// Measured blocks per operation.
pub const BLOCKS: usize = 10;
/// Zero-reading blocks are re-measured at most this many times per operation.
const MAX_RETRIES: u32 = 3;
/// Calls per block are chosen to spend about this many cycles per block.
const TARGET_CYCLES: u64 = 20_000_000;
const CALIBRATION_CALLS: u64 = 1_000;
const MAX_CALLS: u64 = 10_000_000;

/// One line of the output file: raw readings of every block, the CPU each
/// block started and ended on, and how many zero readings were repeated.
#[derive(Serialize)]
pub struct Record<'a> {
    pub id: &'a str,
    pub iters: u64,
    pub instructions: Vec<u64>,
    pub cycles: Vec<u64>,
    pub branch_misses: Vec<u64>,
    pub branches: Vec<u64>,
    pub cpus: Vec<[usize; 2]>,
    pub retries: u32,
}

enum Mode {
    Check,
    List,
    Measure { counters: Counters, output: File },
}

pub struct Runner {
    mode: Mode,
    only: Option<BTreeSet<String>>,
    seen: BTreeSet<String>,
}

/// Operations of one case share the case id prefix.
pub struct Group<'a> {
    runner: &'a mut Runner,
    id: String,
}

const USAGE: &str =
    "usage: <bench> [--bench] | --list [--only ID]... | --output FILE [--only ID]...";

impl Runner {
    fn from_args() -> Result<Self> {
        let mut list = false;
        let mut output = None;
        let mut only = BTreeSet::new();
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                // Passed by `cargo bench`; checks only, like a plain run.
                "--bench" => {}
                "--list" => list = true,
                "--output" => output = Some(PathBuf::from(args.next().context(USAGE)?)),
                "--only" => {
                    only.insert(args.next().context(USAGE)?);
                }
                _ => bail!("unexpected argument {arg}\n{USAGE}"),
            }
        }
        let mode = match (list, output) {
            (false, None) => Mode::Check,
            (true, None) => Mode::List,
            (false, Some(path)) => Mode::Measure {
                counters: Counters::open()?,
                output: OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .with_context(|| {
                        format!(
                            "cannot create {} (existing files are never overwritten)",
                            path.display()
                        )
                    })?,
            },
            _ => bail!(USAGE),
        };
        Ok(Self {
            mode,
            only: (!only.is_empty()).then_some(only),
            seen: BTreeSet::new(),
        })
    }

    pub fn group(&mut self, id: impl Into<String>) -> Group<'_> {
        Group {
            runner: self,
            id: id.into(),
        }
    }

    /// Whether this run takes any operation of the case `name`, whose
    /// operation ids all hold `/{name}/` ([`takes_case`]). A bench skips the
    /// inputs of a case not taken, so a process that measures one case does
    /// not build and check the others; an id whose case it skipped is never
    /// seen, and [`main`] refuses it.
    pub fn takes(&self, name: &str) -> bool {
        takes_case(self.only.as_ref(), name)
    }

    fn run(
        &mut self,
        id: &str,
        period: u64,
        run: &mut dyn FnMut(&Counters, u64) -> Result<Reading>,
    ) -> Result<()> {
        ensure!(self.seen.insert(id.to_owned()), "duplicate operation {id}");
        if self.only.as_ref().is_some_and(|only| !only.contains(id)) {
            return Ok(());
        }
        match &mut self.mode {
            Mode::Check => Ok(()),
            Mode::List => {
                println!("{id}");
                Ok(())
            }
            Mode::Measure { counters, output } => {
                // A calibration that read zero is a failed read as well: taken
                // for a call of one cycle, it would choose the most calls.
                let calibration_calls = whole_turns(CALIBRATION_CALLS, period);
                let mut calibration = run(counters, calibration_calls)?;
                for _ in 0..MAX_RETRIES {
                    if calibration.cycles != 0 {
                        break;
                    }
                    calibration = run(counters, calibration_calls)?;
                }
                ensure!(
                    calibration.cycles != 0,
                    "{id}: the counters read zero cycles for {calibration_calls} calls"
                );
                let per_call = calibration.cycles.div_ceil(calibration_calls).max(1);
                let iters = whole_turns((TARGET_CYCLES / per_call).clamp(1, MAX_CALLS), period);
                run(counters, iters)?;
                let mut record = Record {
                    id,
                    iters,
                    instructions: Vec::with_capacity(BLOCKS),
                    cycles: Vec::with_capacity(BLOCKS),
                    branch_misses: Vec::with_capacity(BLOCKS),
                    branches: Vec::with_capacity(BLOCKS),
                    cpus: Vec::with_capacity(BLOCKS),
                    retries: 0,
                };
                for _ in 0..BLOCKS {
                    let (reading, cpus) = loop {
                        let cpu_start = cpu_number().unwrap_or(usize::MAX);
                        let reading = run(counters, iters)?;
                        let cpu_end = cpu_number().unwrap_or(usize::MAX);
                        let failed = reading.instructions == 0 || reading.cycles == 0;
                        if !failed || record.retries >= MAX_RETRIES {
                            break (reading, [cpu_start, cpu_end]);
                        }
                        record.retries += 1;
                    };
                    record.instructions.push(reading.instructions);
                    record.cycles.push(reading.cycles);
                    record.branch_misses.push(reading.branch_misses);
                    record.branches.push(reading.branches);
                    record.cpus.push(cpus);
                }
                serde_json::to_writer(&mut *output, &record)?;
                output.write_all(b"\n")?;
                Ok(())
            }
        }
    }
}

/// Whether the `--only` ids name an operation of the case `name`: every
/// case without them, as in a check or a plain list.
pub fn takes_case(only: Option<&BTreeSet<String>>, name: &str) -> bool {
    let segment = format!("/{name}/");
    only.is_none_or(|only| only.iter().any(|id| id.contains(&segment)))
}

/// `calls` rounded up to whole turns of a ring of `period` inputs, so that
/// every input is taken as often.
pub fn whole_turns(calls: u64, period: u64) -> u64 {
    calls.div_ceil(period.max(1)) * period.max(1)
}

impl Group<'_> {
    /// Measure `f` called repeatedly; its result is kept alive by `black_box`.
    pub fn op<R>(&mut self, name: &str, f: impl FnMut() -> R) -> Result<()> {
        self.op_ring(name, 1, f)
    }

    /// Measure `f` called repeatedly over a ring of `period` inputs, which
    /// `f` takes in turn: the calibration and every block are whole turns of
    /// the ring, so that every block takes every input as often and the
    /// counters divide by a true count of calls.
    pub fn op_ring<R>(
        &mut self,
        name: &str,
        period: usize,
        mut f: impl FnMut() -> R,
    ) -> Result<()> {
        let id = format!("{}/{name}", self.id);
        self.runner.run(&id, period as u64, &mut |counters, iters| {
            Ok(counters.measure(iters, &mut f)?)
        })
    }

    /// Measure `f` over `inputs` taken in turn, in whole turns of the ring
    /// ([`Self::op_ring`]); over one input, as [`Self::op`]. `f` gets each
    /// input through `black_box`.
    pub fn op_each<I, R>(
        &mut self,
        name: &str,
        inputs: &[I],
        mut f: impl FnMut(&I) -> R,
    ) -> Result<()> {
        use std::hint::black_box;
        if let [input] = inputs {
            return self.op(name, || f(black_box(input)));
        }
        self.op_turns(name, inputs.len(), |at| f(black_box(&inputs[at])))
    }

    /// Measure `f` over a ring of `period` inputs ([`Self::op_ring`]): `f`
    /// gets the index of the input to take, in turn.
    pub fn op_turns<R>(
        &mut self,
        name: &str,
        period: usize,
        mut f: impl FnMut(usize) -> R,
    ) -> Result<()> {
        let mut at = 0;
        self.op_ring(name, period, || {
            at = if at + 1 == period { 0 } else { at + 1 };
            f(at)
        })
    }

    /// Measure an operation that runs `iters` calls itself and returns the
    /// counters accumulated by exactly those calls.
    pub fn op_blocks(
        &mut self,
        name: &str,
        run: impl FnMut(&Counters, u64) -> Result<Reading>,
    ) -> Result<()> {
        self.op_blocks_ring(name, 1, run)
    }

    /// [`Self::op_blocks`] over a ring of `period` inputs, which the
    /// operation takes in turn: `iters` is whole turns of the ring.
    pub fn op_blocks_ring(
        &mut self,
        name: &str,
        period: usize,
        mut run: impl FnMut(&Counters, u64) -> Result<Reading>,
    ) -> Result<()> {
        let id = format!("{}/{name}", self.id);
        self.runner.run(&id, period as u64, &mut run)
    }
}

/// Parse arguments, register and measure every operation, and check that
/// every `--only` id exists.
pub fn main(bench: impl FnOnce(&mut Runner) -> Result<()>) -> Result<()> {
    let mut runner = Runner::from_args()?;
    bench(&mut runner)?;
    if let Some(only) = &runner.only {
        let unknown: Vec<_> = only.difference(&runner.seen).collect();
        ensure!(unknown.is_empty(), "unknown operations: {unknown:?}");
    }
    if let Mode::Measure { output, .. } = &mut runner.mode {
        output.flush()?;
    }
    Ok(())
}

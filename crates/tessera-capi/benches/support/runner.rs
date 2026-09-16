//! Benchmark runner on PMU counters; no wall time anywhere.
//!
//! Every operation is calibrated by cycles, warmed up at the chosen call
//! count, and then measured in [`BLOCKS`] blocks of that many calls. Raw
//! per-block readings go to a JSONL file together with the CPU each block
//! started and ended on; statistics and limits belong to tessera-bench. A
//! block whose counters read zero is a failed counter read, not a
//! measurement: it is repeated a bounded number of times and the repeats are
//! recorded. `--list` prints operation ids and no arguments (as under
//! `cargo test --benches` or `cargo bench`) only runs the correctness checks;
//! neither opens counters, so both work without root. `--output` needs root.

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
};
use tessera_pmu::{Counters, Reading, cpu_number};

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

    fn run(&mut self, id: &str, run: &mut dyn FnMut(&Counters, u64) -> Reading) -> Result<()> {
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
                let calibration = run(counters, CALIBRATION_CALLS);
                let per_call = calibration.cycles.div_ceil(CALIBRATION_CALLS).max(1);
                let iters = (TARGET_CYCLES / per_call).clamp(1, MAX_CALLS);
                run(counters, iters);
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
                        let cpu_start = cpu_number();
                        let reading = run(counters, iters);
                        let cpu_end = cpu_number();
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

impl Group<'_> {
    /// Measure `f` called repeatedly; its result is kept alive by `black_box`.
    pub fn op<R>(&mut self, name: &str, mut f: impl FnMut() -> R) -> Result<()> {
        let id = format!("{}/{name}", self.id);
        self.runner
            .run(&id, &mut |counters, iters| counters.measure(iters, &mut f))
    }

    /// Measure an operation that runs `iters` calls itself and returns the
    /// counters accumulated by exactly those calls.
    pub fn op_blocks(
        &mut self,
        name: &str,
        mut run: impl FnMut(&Counters, u64) -> Reading,
    ) -> Result<()> {
        let id = format!("{}/{name}", self.id);
        self.runner.run(&id, &mut run)
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

# Measuring

The rules of every measurement in this repository: what to measure with,
what must hold before a run, and how to read the result. The tools' own
READMEs say how each tool works; this page says when and how to use
them. Most rules here were paid for with a wrong finding.

## Decide by numbers

- **A hypothesis comes first.** A run answers a question written down
  before it: which cases should move, in which direction, and why. A
  tool or a threshold is not tuned until the run passes.
- **An optimization is proposed with evidence**: a profile, or a count
  of instructions per row in the disassembly of the built module, and
  the expected gain against the measured base. An item below a few
  percent is dropped unless it rides with a larger change of the same
  loop.
- **Sizing constants stay** (a batch is 64 rows) unless a profile shows
  what they cost.
- **A port of a per-row C loop to a kernel is measured**: the kernel can
  lose, to a check of every chunk per call or a bound check per row.

## Before a run

- **The release build is installed.** After a debug build or a run of
  the suites, `make RUST_PROFILE=release && make install
  RUST_PROFILE=release` with the same `PG_CONFIG`, then check which
  library the server loads. Debug kernels are five to ten times slower
  and have produced false regressions more than once.
- **The machine is idle, and whoever uses it says so.** An agent asks
  the person it works for before starting any load; record the power
  source. A laptop runs on mains power.
- **A run takes five minutes at most.** Time the cases the change
  touches, with fewer repetitions; a whole family with 31 repetitions is
  for a baseline the maintainer asks for. Every run has a timeout.
- **Nothing is retried, discarded or stopped early** because of its
  result. A bad run is kept and explained. UNSTABLE in a report means
  the precision was not enough, not success and not failure.

## The tools

Measure with the tools, not with scripts written for the occasion. When
a tool lacks a mode, add it to the tool in the same pull request, with a
test and a line in its README.

- **Is a query faster with Tessera than without?**
  `bench/pg/run.sh measure <family>`, described in
  [bench/pg/README.md](../bench/pg/README.md).
- **Did a change make queries slower?** `cargo ab measure --base REF`,
  described in [tools/tessera-ab](../tools/tessera-ab/README.md).
- **Does a cut of a file only move lines?** `cargo ab moved`, described
  in the same place.
- **Did the machine code of a loop change?**
  `tessera-bench --base REF --disasm [--module nodes]`, described in
  [crates/tessera-capi/benches](../crates/tessera-capi/benches/README.md).
- **How many instructions and cycles does a kernel take?**
  `tessera-bench --base REF`, described in the same place.
- **Are the answers of the TPC-H queries right, and the times?**
  `cargo tpch`, described in
  [bench/tpch/README.md](../bench/tpch/README.md).

The bench cluster is made once by `bench/pg/run.sh setup`. `run.sh stop`
deletes it: restart it with `pg_ctl restart` instead.

For now the counters work only on macOS with Apple silicon: they come
from kperf and need root, so an agent leaves such a run to the person it
works for. x86 and Linux are planned, and a run will go differently
there. CI checks correctness, not speed.

## Machine code before counters

On macOS a run with counters costs a person's time under `sudo`. Before
asking for one:

- Run `--disasm` on every benchmark program in scope, not only on the
  loop that motivated the change. No loop of the candidate may be longer
  than the base's without a reason written down.
- State prepared once a call stays out of the loop; a closure or an
  `Option` changed inside a shared loop costs every operation that
  shares it.
- A row loop stays in the function that checks the lengths, or the
  bound checks come back.
- A small function of another crate called from a generic kernel needs
  `#[inline]`.

## Reading a result

- **Counters.** Instructions decide: more than 1 % is a failure. Cycles
  warn at 3 % and fail at 10 % on operations of 500 cycles or more.
  Summarize a report as how many cases have fewer, the same and more
  instructions, with the median change, before judging it: a net gain
  still prints failures.
- **More cycles with fewer instructions** is first compared with a run
  of three repeats, then with the address of the loop's head. A function
  moved by 16 bytes against a 64-byte line has cost 5 %: build both
  sides with `COPT=-falign-functions=64` (`cargo ab measure --copt`) and
  measure again.
- **A ratio of times.** Before explaining it, open `plans.txt` of that
  very run and confirm that the batch node ran: a memory gate or a
  missing statistic may have left the query to the core. Profile under
  the settings of the run. An aggregate the outer query does not read is
  dropped by the planner, so refer to it outside.
- **A confirmed excess over a threshold stops the work** for profiling
  and a discussion. A run that was not agreed on is not a passed check.

## Where runs are kept

A run lives in `target/bench-runs/<kind>-<id>/`, outside git: the
sources or their hashes, the protocol fixed before the run, the raw
data, the plans, the report. Runs worth citing are copied to
`docs/benchmarks/<date>/`. The README gives rough ranges by kind of
query and links there; it keeps no exact timings.

## Mutation runs

- After a mutated source is restored, `touch` it: a restored file with
  an older time is not rebuilt, and every later mutant is "caught" by
  the stale one.
- Run the clean tests once after the loop, and install the release build
  again before any timing.
- Judge a module's mutants by its own tests to stay within five minutes:
  `cargo mutants -f <file> --test-package <crate> -- --lib <module>`.

## Dependencies

A crate is added only when it is clearly better than the standard
library on our data (with duplicates, with sorted input) and widely
used: report its downloads, reverse dependencies, last release and
stars with the measurement.

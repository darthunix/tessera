## Context

See proposal.md, "Why", for the motivation. This section gives the
state of `relhash` that shapes the design.

- A probe of a batch with at least 8 selected rows of a word goes
  vertically (`table/lanes.rs`): every row's bucket, then every row's
  record, then the next records. Before each step it asks for the
  memory of the next one through the region (`Region::prefetch`,
  `Region::prefetch_record`). The raw region (`table/region.rs`)
  answers with the crate's one hint, `lanes::prefetch`; the regions of
  loom and of the proofs answer with nothing.
- `lanes::prefetch` is `prfm pldl1keep` on AArch64 and nothing
  elsewhere and under Miri. Three paths reach it:
  - the probe of a batch, `probe_rows` in `table/batch.rs`;
  - the find or insert of a batch, `resolve_rows` in
    `table/writer.rs`, which probes vertically, then rechecks the rows
    left unresolved and writes the records of the new keys;
  - the gathers of records in no order, `gather::<true>` and
    `gather_words` in `table/batch.rs`, which a sort calls through
    `Table::gather_scattered` and `Table::gather_words`: they locate
    the records of a word of rows and ask for each before reading any.
    On x86-64 that first pass asks for nothing today.
- The table knows its sizes: its index the number of buckets, a lookup
  the count of records it read (`Access::records`), and the format the
  size of a record.
- The measurement of the hint on x86-64, given by every probe
  (`compare-aXu7WT` to `compare-4Dnv47`): it gained 10 to 26 % of
  cycles on a table of 160 MiB and cost up to 14 % on one in L1. The
  calls that gained took 5 to 11 % more instructions, the hint's own,
  and five of them failed on instructions by the rule of 1 %. Its loop
  of the buckets' phase grew from 9 to 14 instructions, and that of the
  insert from 9 to 11, by the hint, its address and a reload of the
  index's base (`disasm-ieHfUc`).
- Two rules stand in the way of any hint on x86-64: the rule of
  `docs/measuring.md` that more than 1 % of instructions is a failure,
  and the rule "No cost to Tessera" of the contract of `relhash`: "A
  hot loop of Tessera compiles no longer through the crate's API than
  before the change". The maintainer chose on 2026-11-01 to propose
  narrowing them for the hint, in a pull request of contracts and
  designs only.
- On the M5 Pro the vertical probe took a lookup that finds its key on
  a table far larger than the caches from about 75 to about 38 cycles a
  row; the share of the hint in that gain, and its cost on a table in
  the cache, were not measured apart.
- The benchmarks: `table_int32` builds its tables from the reading
  fixtures, a few KiB, in L1; `table_large` builds one table of 2^22
  records, 160 MiB, and probes it from a ring of batches of distinct
  keys, so that every call reaches memory. Their `find_or_insert`
  finds keys that are there and creates nothing; no program measures
  a gather.
- The Intel Mac's Core i5-1038NG7 has 48 KiB of L1 data cache and 512
  KiB of L2 a core, and 6 MiB of L3; the M5 Pro's are larger.

## Goals / Non-Goals

**Goals:**

- The hint where it gains, on tables larger than a bound, and on
  smaller tables the machine code of today, on both machines.
- A choice that costs a call one test, and the loops no instruction
  but the hint's own where it is given.
- No change of the public API of `relhash` and no code of Tessera.

**Non-Goals:**

- No other hint (`prefetcht1`, `prefetchnta`, a distance further
  ahead): the bound is the question here.
- No change of the vertical probe or of its bound of 8 rows.

## Decisions

### The rules first

The hint adds its own instructions and lengthens the loops that give
it, so it fails both rules on every table where it is given, whatever
it gains. That is not searched around: the rules are the question. The
pull request that narrows them, of contracts and designs only, says
what they cost, that a hint by size keeps them for every table under
the bound, and what is kept: on such a table, the machine code of
today. This change waits for the maintainer's decision on it; if the
rules stay, the change closes without the hint.

### The choice once a call

The probe, the find or insert and the gathers of a batch compute,
once a call, whether the table is larger than `PREFETCH_MIN_BYTES`:
its buckets times their size plus its records times theirs. The
batch's own arrays are on the stack and small beside a table that
reaches the bound. They then run one of two copies of their loop,
made by a constant parameter, as the gathers already are: one with
the hint and one without. The loops hold no test of their own, and
the copy without the hint is today's code.

`PREFETCH_MIN_BYTES` is a constant for each architecture, beside the
hint in `table/lanes.rs`. A table whose size changes during a call,
as a shared table whose records others append, keeps the choice the
call made at its start: a hint is never wrong, only dearer or cheaper.

The gathers already choose by a parameter, `gather::<true>` for
records in no order; on x86-64 they would get the hint wherever they
ask for it. So they take the same choice by size and are measured
with the probes.

Refused:

- A test of the size before each hint: a branch in the loop, for a
  question whose answer does not change within a call.
- A hint chosen by the caller: a new parameter of the public API, which
  the contract's rule "A small surface" allows only for a reason the
  crate cannot meet itself, and the table knows its own size.
- The size of the caches read from the processor at run time: one
  more source of difference between machines and runs, for a bound
  that the counters can fix.
- Keeping the gathers out by a hint of their own: a second hint in the
  region, for paths that ask for the same thing.

### The bound

Measured, not derived. `table_large` gains a constant of records for
each case, so that it builds tables of about 256 KiB, 512 KiB, 1 MiB,
2 MiB, 4 MiB, 8 MiB, 16 MiB and 160 MiB, each probed from its own ring
of batches, as today. It also gains, for each size:

- a find or insert that creates the records of half the rows of each
  batch, the table brought back to its built state between calls,
  outside the count;
- a gather of the records of a batch in no order.

The bound of an architecture is the smallest measured size from which
the hint gains on every measured size above it, by the rule of
acceptance below, in both runs; the next smaller size, where it does
not gain, shows that the bound is not set too low. No size between
two measured ones is assumed.

### x86-64 and AArch64

x86-64 gets `prefetcht0`, `_mm_prefetch` with `_MM_HINT_T0`, SSE,
which every x86-64 processor has, in an `unsafe` block whose reason is
that it reads nothing and never faults. AArch64 keeps `prfm
pldl1keep`.

Each machine compares the hint with its absence, the base being what
it runs today:

- on the Intel Mac, a candidate that gives the hint on every table
  against the base, which gives none;
- on the M5 Pro, a candidate that gives none against the base, which
  gives it on every table.

If the hint costs the M5 Pro nothing on any size, its bound is 0 and
its code stays as it is.

## Measurement plan

The rules of `docs/measuring.md` hold; the runs as root on the Intel
Mac are the maintainer's commands, and those of the M5 Pro are
relayed.

- The sizes: the program with its new cases against itself
  (`tessera-bench --base WORKTREE`), on both machines, so that the
  cases are sound before any candidate. The base of every later run
  comes after these cases.
- The bound: the comparisons of the hint with its absence above,
  twice on each machine, over `table_int32` and every case of
  `table_large`.
- The choice: the candidate with its bound against the base, twice on
  each machine, over both programs.
- The rule of the contract "No cost to Tessera", as it stands after
  the maintainer's decision: `tessera-bench --base <base> --disasm`
  over every program on both targets, and with `--module` on the M5
  Pro, where PostgreSQL is built; the loops longer than the base's are
  listed with their reasons.
- The rule of acceptance: the hint gains on a case when its minimum and
  its median cycles both fall by more than 3 % and by more than the
  case's spread in the runs of identical sides, in both runs. On a
  table under the bound the candidate must change nothing: the same
  machine code as the base in those loops, and no FAIL. On a table
  above it, a growth of instructions passes only where the listing
  shows it is the hint and its address, as the narrowed rules allow.
  A run with an UNSTABLE on a case the candidate changes is repeated;
  a second stops the work for the maintainer.
- The runs worth citing are copied to `docs/benchmarks/<date>/`.

## Risks / Trade-offs

- [The loops in two copies double their code] → The listing shows the
  size; the copies differ only by the hint. The gathers do it already.
- [A table that grows during a call crosses the bound] → The call
  keeps the choice it made; a hint changes no result.
- [The bound depends on the machine's caches] → One for each
  architecture, measured on one machine each; the AMD servers of the
  target platform stay `linux-x86-support`.
- [The new sizes and operations lengthen the runs] → Each size is a
  case of its own, and the runs filter them under five minutes.
- [Bringing a table back between calls of the creating case warms it]
  → The case is judged against itself, hint against no hint, under the
  same schedule.

## Steps

One step, one pull request, after the maintainer's decision on the
rules: the sizes, the bound, the choice and the close.

## Files

- `crates/relhash/src/table/lanes.rs`: the hint of x86-64 and
  `PREFETCH_MIN_BYTES`.
- `crates/relhash/src/table/batch.rs`,
  `crates/relhash/src/table/writer.rs`: the choice once a call.
- `crates/tessera-capi/benches/support/tabling_large.rs`: the sizes,
  the creating find or insert and the gather.
- `openspec/specs/relhash/design.md`,
  `openspec/specs/relhash/contract.md`: the hint and the forecast.

## Tests

- `cargo test --locked -p relhash`: the probes, the finds and inserts
  and the gathers, whose results no hint changes.
- `crates/tessera-capi/tests/benchmark.rs`: the new cases against the
  fixture's model.
- `.github/scripts/check-trust.sh`: the `unsafe` of the hint in a
  module of the design's list.

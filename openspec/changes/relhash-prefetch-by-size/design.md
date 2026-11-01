## Context

See proposal.md, "Why", for the motivation. This section gives the
state of `relhash` that shapes the design.

- A probe of a batch with at least 8 selected rows of a word goes
  vertically (`table/lanes.rs`): every row's bucket, then every row's
  record, then the next records. Before each step it calls `prefetch`
  for the memory of the next one, through the region
  (`Region::prefetch`, `Region::prefetch_record`), which the raw region
  of the crate implements with the crate's own hint and the regions of
  loom and of the proofs with nothing.
- `prefetch` is `prfm pldl1keep` on AArch64 and nothing elsewhere and
  under Miri. The probe (`batch.rs`, `probe_rows`) and the insert
  (`writer.rs`, `resolve_rows`) give it; the gathers of a sort
  (`gather::<PREFETCH>`) choose theirs by a parameter of their own.
- The table knows its sizes: its index the number of buckets, a lookup
  the count of records it read (`Access::records`), and the format the
  size of a record.
- The measurement of the hint on x86-64 (`compare-aXu7WT` to
  `compare-4Dnv47`): its loop of the buckets' phase grew from 9 to 14
  instructions, and that of the insert from 9 to 11, by the hint, its
  address and a reload of the index's base (`disasm-ieHfUc`). It gained
  10 to 26 % of cycles on 160 MiB and cost up to 14 % in L1.
- On the M5 Pro the vertical probe took a lookup that finds its key on
  a table far larger than the caches from about 75 to about 38 cycles a
  row; the share of the hint in that gain, and its cost on a table in
  the cache, were not measured apart.
- The benchmarks: `table_int32` builds its tables from the reading
  fixtures, a few KiB, in L1; `table_large` builds one table of 2^22
  records, 160 MiB, and probes it from a ring of batches of distinct
  keys, so that every call reaches memory.
- The Intel Mac's Core i5-1038NG7 has 48 KiB of L1 data cache and 512
  KiB of L2 a core, and 6 MiB of L3; the M5 Pro's are larger.

## Goals / Non-Goals

**Goals:**

- The hint where it gains, on tables larger than the caches, and none
  where it costs, on both machines.
- A choice that costs a call no more than one test, and the loops of a
  call no instruction.
- No change of the public API of `relhash` and no code of Tessera.

**Non-Goals:**

- No other hint (`prefetcht1`, `prefetchnta`, a distance further
  ahead): the bound is the question here.
- No change of the vertical probe or of its bound of 8 rows.
- No hint for the gathers of a sort beyond what they have.

## Decisions

### The choice once a call

The probe and the insert of a batch compute, once a call, whether the
table is larger than `PREFETCH_MIN_BYTES`: its buckets times their
size plus its records times theirs. They then run one of two copies of
the vertical loop, made by a constant parameter, as the gathers do:
one with the hint and one without. The loops then hold no test of
their own.

`PREFETCH_MIN_BYTES` is a constant for each architecture, beside the
hint in `table/lanes.rs`. A table whose size the call cannot know
cheaply, such as a shared table whose records others append, takes the
size it read at the start of the call: a hint is never wrong, only
dearer or cheaper.

Refused:

- A test of the size before each hint: a branch in the loop, for a
  question whose answer does not change within a call.
- A hint chosen by the caller: a new parameter of the public API, which
  the contract's rule "A small surface" allows only for a reason the
  crate cannot meet itself, and the table knows its own size.
- The size of the caches read from the processor at run time: one
  more source of difference between machines and runs, for a bound
  that the counters can fix.

### The bound

Measured, not derived. `table_large` gains a constant of records for
each case, so that it builds tables of about 256 KiB, 2 MiB, 16 MiB
and 160 MiB, each probed from its own ring of batches, as today. The
bound is the smallest of those sizes from which the hint gains on both
runs and loses nowhere above it, on each machine; between two sizes it
is the lower one's double.

### x86-64 and AArch64

x86-64 gets `prefetcht0`, `_mm_prefetch` with `_MM_HINT_T0`, SSE,
which every x86-64 processor has, in an `unsafe` block whose reason is
that it reads nothing and never faults. AArch64 keeps `prfm
pldl1keep`; its bound is measured on the M5 Pro, and if the hint costs
nothing there on any size, its bound is 0 and its code stays as it is.

## Measurement plan

The rules of `docs/measuring.md` hold; the runs as root on the Intel
Mac are the maintainer's commands, and those of the M5 Pro are
relayed.

- The sizes: the program with its new cases against itself
  (`tessera-bench --base WORKTREE`), on both machines, so that the
  cases are sound before any candidate.
- The bound: the hint in every probe (a bound of 0) against the base,
  twice on each machine, over `table_int32` and every size of
  `table_large`; the bound read off as above.
- The choice: the candidate with its bound against the base, twice on
  each machine, over both programs.
- The rule of the contract "No cost to Tessera": `tessera-bench --base
  <base> --disasm` over every program on both targets, and with
  `--module` on the M5 Pro, where PostgreSQL is built; the loops longer
  than the base's are listed with their reasons.
- The rule of acceptance: the hint is taken where it gains by more
  than 3 % of cycles in two runs and loses nowhere: no loss of 10 %
  of cycles confirmed by a second run and no FAIL of instructions on a
  table under the bound.

## Risks / Trade-offs

- [The vertical loop in two copies doubles its code] → The listing
  shows the size; the copies differ only by the hint. The gathers do
  the same.
- [A table that grows during a call crosses the bound] → The call
  keeps the choice it made; a hint changes no result.
- [The bound depends on the machine's caches] → One for each
  architecture, measured on one machine each; the AMD servers of the
  target platform stay `linux-x86-support`.
- [The four sizes of `table_large` lengthen its runs] → Each size is a
  case of its own, and the runs filter them under five minutes.

## Steps

One step, one pull request: the sizes, the bound, the choice and the
close.

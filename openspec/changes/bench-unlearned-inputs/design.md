## Context

See proposal.md, "Why", for the motivation. This section gives the
state of the benchmarks that shapes the design.

- A benchmark program registers its cases with the runner,
  `crates/tessera-capi/benches/support/runner.rs`. For each operation
  the runner calibrates a number of calls by cycles, about 20 million
  cycles a block, warms up, and counts 10 blocks of that many calls.
  It records instructions, cycles, branches and missed branches of
  every block. `tessera-bench` runs three processes on each side and
  judges the two sides.
- `tessera-bench` marks an operation UNSTABLE when its blocks disagree
  on instructions by more than 0.1 %, and fails it on 1 % more
  instructions or, confirmed by a second run, on 10 % more cycles of a
  call of at least 500 cycles.
- The inputs of a case are built once, before counting, by
  `support/fixture.rs`. Its generator, xorshift64*, is seeded by the
  row count alone, so every case of 1024 rows draws the same sequence,
  and every process the same inputs.
- The cases drawn at random are:
  - in the reading fixtures that 11 programs share
    (`support/reading.rs`, `support/reading64.rs`): the selection
    `random`, every row with a chance of one half, and the flags
    `nulls-random`, 30 % NULL rows;
  - in `set_int32`: `shuffled`, every row's value in a random order;
  - `table_large` already probes a ring of batches of distinct keys,
    taken in turn, so that its records come from memory; its keys are
    chosen so that every batch does the same work.
- A case of 1024 int4 rows holds about 13 KiB: 4 KiB of values, 8 KiB
  of Datums, 1 KiB of NULL flags and its masks; int8 about 17 KiB. The
  Intel Mac's Core i5-1038NG7 has 48 KiB of L1 data cache and 512 KiB
  of L2 a core; the M5 Pro's are larger.
- `pmu` reads the counters at any point (`Counters::read`,
  `Reading::since`), so a measured operation may count only the calls
  between two reads (`Group::op_blocks`).
- What the predictor learned, as the proposal lists: the fold of
  `column_reader` on `random/nulls-random` misses 0.03 branches a call
  (`compare-F6x7L1`); the halving of `set_int32` on `shuffled` misses
  as on values in order (`compare-jrOJVK`); the int8 division moved by
  96 bytes missed 17 branches a call where it had missed 1 or 2
  (`compare-OmhwZ0`, `compare-4xzRdr`).

## Goals / Non-Goals

**Goals:**

- A random case whose branches miss as often as on data the predictor
  has not seen, on both machines.
- The same instructions in every block, so that the verdicts of
  `tessera-bench` keep their meaning.
- A call of a new case does the work of the case it replaces: the same
  rows, the same shares of selected and NULL rows.
- The regular cases unchanged.

**Non-Goals:**

- No new verdict in `tessera-bench`: the limits stay.
- No change of the kernels, and no decision taken on the new cases:
  that is `whole-word-decisions`.
- No model of each predictor: the criterion is measured, not derived.

## Decisions

### Inputs that the predictor cannot learn

A predictor learns a branch from the history of the branches before
it. Over the same inputs every call, the history repeats, and a large
predictor learns even a long sequence: the halving of `shuffled` runs
about 7000 data-dependent branches a call, and Ice Lake learned them.
So the inputs must not repeat within the predictor's reach. Three
forms do that:

1. **A ring of inputs.** A case holds `K` inputs of the same shape,
   each drawn from its own seed, and every call takes the next. A block
   is a whole number of turns of the ring, so every block does the
   same work and keeps its instructions. The footprint grows `K`
   times: 32 inputs of int4 take about 416 KiB, past L1 but inside L2.
   `table_large` uses this form already, for its caches.
2. **One long input.** A case of `K` times 1024 rows. It needs no
   change of the runner, but a call does `K` times the work, so its
   results are not those of a call of 1024 rows, and the cases of 0
   to 65 rows cannot be made long. Its footprint grows as the ring's.
3. **A fresh input before every call.** The operation counts its own
   calls (`op_blocks`), reads the counters around each call, and draws
   a new input between two calls, outside the count. The footprint is
   one input, in L1 as today. Each call costs two reads of the
   counters, which on macOS go through the kernel; whether a read adds
   to the count, and how much, is what the pilot measures.

The pilot chooses. Each form is tried, as a hypothesis written down
first, on two cases:

- `column_reader`, `dense/words/1024/random/nulls-random/ready/fold`:
  the loop over the selected rows of a word ends once a word, after a
  number of rows that is random, so a predictor that has not seen the
  word misses about once a word, 16 times a call. The model holds on
  both machines.
- `set_int32`, `int4/dense/words/1024/shuffled/nulls-none/ready/keys65`
  on the Intel Mac: about half of the 7 steps of each row's halving
  miss, about 3500 misses a call. On AArch64 the step is a select, so
  this case shows nothing there.

A form passes when, on both runs and both machines where its case
applies:

- the misses a call reach at least 80 % of the model;
- making the inputs more varied, `K` doubled, adds at most 5 % more
  misses, so the predictor learns nothing left;
- the blocks of a side keep their instructions within 0.1 %.

Of the forms that pass, the one that changes the footprint least is
taken: the third if a read of the counters adds nothing measurable to
a call, then the ring with the smallest `K` that passes.

Refused:

- A larger random case of the same form. The predictor learned 7000
  branches; a few times more might not be enough on the next machine,
  and nothing would show it.
- Seeds that change from one process to the next. The three processes
  of a side would differ, and so would the two sides, by chance.

### Which cases change

The cases drawn at random: `random` and `nulls-random` of the reading
fixtures, of both widths, and `shuffled` of `set_int32`. Their names
stay, with their meaning sharpened: drawn at random and not learned.
`table_large` keeps its ring, which already gives each call new keys;
the pilot's form is not forced on it.

The regular cases keep their inputs. A pattern that repeats is what a
predictor of any machine learns, and a call over such data is a case
of its own.

### Checks of the new cases

- The model of each fixture is checked before counting, as today, for
  every input of a case (`cargo test --locked -p tessera-capi
  --benches`, `tests/benchmark.rs`).
- A run of every program against itself (`tessera-bench --base
  WORKTREE`) on the Intel Mac as root and on the M5 Pro: no FAIL and
  no UNSTABLE on the new cases, and their spread recorded. The
  maintainer runs it on the M5 Pro, as for the earlier changes.

## Measurement plan

The rules of `docs/measuring.md` hold: an idle machine on mains power,
the maintainer asked before every run, five minutes a run at most,
the tools and not scripts for the occasion. On the Intel Mac the runs
as root are the maintainer's commands.

- The pilot: each form on its two cases, with `K` of 1, 8, 32 and 128
  where it has one, twice on each machine, `--filter` on the cases.
  The misses a call, the cycles a call and the instructions of the
  blocks are read off the report.
- The runs of identical sides, as above, after the cases change.

## Risks / Trade-offs

- [The ring moves the inputs out of L1, and cycles of whole words and
  of rows shift differently] → The pilot prefers the fresh input,
  which keeps the footprint; where the ring is taken, the README says
  where its inputs lie. Data in a query does not sit in L1 either.
- [A read of the counters around every call adds to the count] → The
  pilot measures an empty call between two reads; the fresh input is
  taken only if that adds nothing measurable.
- [The new cases spread more, and give FAILs by chance] → The runs of
  identical sides record the spread; if it passes the limits, the work
  stops and the maintainer is asked, as for the run on the Intel Mac.
- [A base before this change cannot be compared on the new cases] →
  The first runs of `whole-word-decisions` take a base after it.

## Steps

One step, one pull request: the pilot, the cases, the documents and
the runs, then the close.

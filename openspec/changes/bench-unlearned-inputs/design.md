## Context

See proposal.md, "Why", for the motivation. This section gives the
state of the benchmarks that shapes the design.

- A benchmark program registers its cases with the runner,
  `crates/tessera-capi/benches/support/runner.rs`. For each operation
  the runner calibrates a number of calls, `iters`, from the cycles of
  1000 calls, so that a block spends about 20 million cycles; warms up
  with one block; and counts 10 blocks of `iters` calls. It records
  instructions, cycles, branches and missed branches of every block.
  `Group::op_blocks` lets an operation run its own `iters` calls and
  return the counters of exactly those calls. `tessera-bench` runs
  three processes on each side and judges the two sides per call.
- `tessera-bench` marks an operation UNSTABLE when a side's blocks
  disagree on instructions by more than 0.1 %, before any other
  verdict. It fails an operation on 1 % more instructions, or on 10 %
  more minimum cycles of a call of at least 500 cycles when the median
  grew past the warning too, and a FAIL of cycles counts on the Intel
  Mac when a second run confirms it.
- The inputs of a case are built once, before counting, by
  `support/fixture.rs`. Its generator, xorshift64*, is seeded by the
  row count alone, so every case of 1024 rows draws the same sequence,
  and every process the same inputs. The selection `random` takes each
  row with a chance of one half and the flags `nulls-random` mark each
  row NULL with a chance of 30 %, row by row, so two inputs drawn from
  two seeds select and keep different numbers of rows in each word.
- The cases drawn at random are:
  - in the reading fixtures that 10 programs share
    (`support/reading.rs`, `support/reading64.rs`): `random` and
    `nulls-random`;
  - in `set_int32` (`support/sets.rs`): `shuffled`, every row's value
    in a random order. Its keys are the values of every seventh row,
    65 of them, so they span the values 0 to 448 of the 1024: 575 rows
    are above every key and take the same steps of a search by halving;
  - `table_large` already probes a ring of batches of distinct keys,
    taken in turn, so that its records come from memory; its keys are
    chosen so that every batch does the same work.
- A case of 1024 int4 rows holds about 13 KiB: 4 KiB of values, 8 KiB
  of Datums, 1 KiB of NULL flags and its masks; int8 about 17 KiB. An
  operation over one storage reads part of it: a dense read the values
  and the masks, a Datum read the Datums and the flags. The Intel Mac's
  Core i5-1038NG7 has 48 KiB of L1 data cache and 512 KiB of L2 a core;
  the M5 Pro's are larger.
- What the predictor learned, as the proposal lists: the fold of
  `column_reader` on `random/nulls-random` misses 0.03 branches a call
  (`compare-F6x7L1`); the halving of `set_int32` on `shuffled` misses
  as on values in order (`compare-jrOJVK`); the int8 division moved by
  96 bytes missed 17 branches a call where it had missed 1 or 2
  (`compare-OmhwZ0`, `compare-4xzRdr`).
- `tessera-bench` compares only sides whose benchmark sources are the
  same, file for file (`tools/tessera-pgtool/src/snapshot.rs`), so a
  run cannot take a base from before a change of any benchmark file.

## Goals / Non-Goals

**Goals:**

- A random case whose branches miss as often as a predictor that has
  not seen its inputs would, on both machines.
- The same instructions in every block, within the 0.1 % of
  `tessera-bench`, so that its verdicts keep their meaning.
- A call of a new case does the work of the case it replaces: the same
  rows, and in every word the same numbers of selected and NULL rows
  as in every other input of the case.
- The regular cases unchanged.

**Non-Goals:**

- No new verdict in `tessera-bench`: the limits stay.
- No change of the kernels, and no decision taken on the new cases:
  that is `whole-word-decisions`.
- No model of each predictor: the expected misses are those of a
  predictor that knows how often each branch goes each way but not
  the inputs, computed from the case itself.

## Decisions

### Inputs that the predictor cannot learn

A predictor learns a branch from the history of the branches before
it. Over the same inputs every call, the history repeats, and a large
predictor learns even a long sequence: the halving of `shuffled` runs
about 7000 data-dependent branches a call, and Ice Lake learned them.
So the inputs must not repeat within the predictor's reach.

Every input of a case is drawn so that it does the same work as the
others: in each word the same number of selected rows and of NULL
rows, at positions drawn anew. Each input is the case's first input
with the rows of each word permuted by its own seed, the masks and the
values together. Two inputs then differ in where their rows lie, not
in how many there are, and a call costs the same instructions whatever
input it takes, but for the branches that miss.

Three forms keep the inputs from repeating:

1. **A ring of inputs.** A case holds `K` inputs and every call takes
   the next. The runner learns of the ring: an operation names its
   period, and the runner rounds `iters` up to a multiple of it, after
   calibrating on whole turns of the ring, so that every block takes
   every input as often and the report's division by `iters` stays
   true. The footprint grows `K` times: 32 inputs of a dense int4 read
   take about 140 KiB, past L1 but inside L2. `table_large` uses a ring
   already, for its caches.
2. **One long input.** A case of `K` times 1024 rows. It needs no
   change of the runner, but a call does `K` times the work, so its
   results are not those of a call of 1024 rows, and the cases of 0 to
   65 rows cannot be made long. Its footprint grows as the ring's.
3. **A fresh input before every call.** The operation counts its own
   calls (`op_blocks`), reads the counters around each call, and
   permutes its input between two calls, outside the count. The
   footprint is one input. Each call costs two reads of the counters,
   which on macOS go through the kernel, and the permutation warms the
   input into the cache and leaves branches of its own in the
   predictor's history.

### The model of the misses

For each case of the pilot, the expected misses a call are those of a
predictor that knows how often each data-dependent branch goes each
way but has not seen the inputs: for each branch, the rarer outcome's
share, summed over a call. They are computed from the case itself, by
enumerating its inputs in the benchmark's own model, and printed with
the case's check, so that the number the pilot is held to is the
case's, not a guess.

### The pilot

Two cases, each in its new form:

- `column_reader`, `dense/words/1024/random/nulls-random/ready/fold`:
  the loop over the selected rows of a word ends once a word, after a
  number of rows that the predictor cannot know; on both machines.
- `set_int32`, int4 dense, 65 keys, over values drawn within the span
  of the keys, so that every step of the halving goes either way: on
  the Intel Mac, where the step is a branch; on AArch64 it is a select
  and the case shows nothing there.

Each form is tried on each case, with `K` of 1, 8, 16, 32, 64 and 128
where it has one, as a hypothesis written down first. Three controls
run beside it, each the same in footprint, preparation and schedule
of reads, but over one input:

- for the ring, a ring of `K` copies of the same input;
- for the long input, the same input repeated `K` times;
- for the fresh input, the same input copied anew before every call.

A control's misses show what the predictor learns when the novelty is
gone and all else stays; its cycles show what the footprint or the
schedule costs by itself. The listing of each pilot build
(`tessera-bench --disasm`) shows that the kernels lie at the same
places in their lines of 64 bytes as in the base; a pilot whose
kernels moved is compared with a base that moved them alike, or not
judged on cycles.

A form passes when, in both runs on each machine where its case
applies:

- its misses a call reach at least 80 % of the model, and its control
  stays where the base is;
- its misses at `2K` are within 5 % of those at `K`, so the predictor
  learns nothing left;
- `tessera-bench` reports no UNSTABLE for it.

Of the forms that pass, the one that changes the conditions least is
taken: the fresh input if its control costs no more than the base by
the warning of 3 % of cycles, otherwise the ring with the smallest `K`
that passes. A pilot where no form passes stops the work for the
maintainer.

Refused:

- A larger random case of the same form. The predictor learned 7000
  branches; a few times more might not be enough on the next machine,
  and nothing would show it.
- Seeds that change from one process to the next. The three processes
  of a side would differ, and so would the two sides, by chance.
- Inputs drawn row by row from new seeds. Their counts of selected and
  NULL rows would differ, and with them the instructions of a call.

### Which cases change

The cases drawn at random: `random` and `nulls-random` of the reading
fixtures, of both widths, and `shuffled` of `set_int32`. Their names
stay, with their meaning sharpened: drawn at random and not learned.
`table_large` keeps its ring, which already gives each call new keys.

The regular cases keep their inputs. A pattern that repeats is what a
predictor of any machine learns, and a call over such data is a case
of its own.

### Checks of the new cases

- Every input of a case is checked against the model of the fixture
  before counting, as today (`cargo test --locked -p tessera-capi
  --benches`, `tests/benchmark.rs`).
- A run of every program against itself (`tessera-bench --base
  WORKTREE`) on the Intel Mac as root and on the M5 Pro: no FAIL and no
  UNSTABLE on the new cases. Their spread of minimum and median cycles
  is recorded; it is the noise against which `whole-word-decisions`
  judges its gains.

## Measurement plan

The rules of `docs/measuring.md` hold: an idle machine on mains power,
the maintainer asked before every run, five minutes a run at most,
the tools and not scripts for the occasion. On the Intel Mac the runs
as root are the maintainer's commands; the M5 Pro's are relayed.

- The base of the pilot is a commit with the pilot's fixtures in
  place, so that `tessera-bench` can compare it; each form and control
  is a candidate against it, `--filter` on the pilot's cases, twice on
  each machine.
- The runs of identical sides, as above, after the cases change.
- The runs worth citing are copied to `docs/benchmarks/<date>/`.

## Risks / Trade-offs

- [The ring moves the inputs out of L1, and cycles of whole words and
  of rows shift differently] → The controls measure the footprint
  alone; the fresh input, which keeps it, is preferred; where a ring is
  taken, the README says where its inputs lie. Data in a query does
  not sit in L1 either.
- [A read of the counters around every call, and the permutation
  between calls, add to the count or warm what they touch] → The fresh
  input's control measures both, over the same schedule, and the form
  is taken only if they cost no more than the warning.
- [The new cases spread more, and give FAILs by chance] → The runs of
  identical sides record the spread; if it passes the limits, the work
  stops and the maintainer is asked, as for the run on the Intel Mac.
- [A pilot build moves the kernels in the binary] → The listing of
  each build, and controls built alike.
- [A base before this change cannot be compared on the new cases] →
  The first runs of `whole-word-decisions` take a base after it.

## Steps

One step, one pull request: the pilot, the cases, the documents and
the runs, then the close.

## Files

- `crates/tessera-capi/benches/support/fixture.rs`: the permuted
  inputs.
- `crates/tessera-capi/benches/support/runner.rs`: the period of a
  ring, if a ring is taken.
- `crates/tessera-capi/benches/support/reading.rs`,
  `crates/tessera-capi/benches/support/reading64.rs`,
  `crates/tessera-capi/benches/support/sets.rs`: the cases.
- `crates/tessera-capi/benches/README.md`, `docs/measuring.md`: what
  the cases show.

## Tests

- `crates/tessera-capi/tests/benchmark.rs`: the names of the cases and
  every input against the fixture's model.
- `cargo test --locked -p tessera-capi --benches`: every case checked
  before counting.

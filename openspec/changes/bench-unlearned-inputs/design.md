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
  rows, and the same numbers of full, single-row and other words, of
  selected rows and of NULL rows, as every other input of the case.
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
others, yet repeats neither where its rows lie nor how many each word
holds. Each input is the case's first input with its words put in an
order drawn by its own seed, and then the rows of each word permuted
by the same seed, the masks and the values together. Two inputs then
hold the same words, so a call costs the same instructions whatever
input it takes, but for the branches that miss; yet the count of rows
of the n-th word changes from input to input, and with it every loop
over the rows of a word, whose exit the predictor cannot learn.

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
way, and how a loop's trip counts are spread, but has not seen the
inputs. For each loop over the rows of a word, they are the share of
its exits that a trip count drawn from that spread cannot foretell.
For a search by halving, they are, at each node of the search, the
share of the values that take its rarer way. A node and not a step:
the predictor's history holds the steps before it in the same search,
so it knows which range of keys is left. Both are summed over a call.
They are computed from the case itself, by enumerating its inputs in
the benchmark's own model, and a test holds them, so that the number
the pilot is held to is the case's, not a guess.

### The pilot

Two cases, each in its new form:

- `column_reader`, `dense/words/1024/random/nulls-random/ready/fold`:
  the loop over the selected rows of a word ends once a word, after a
  number of rows that the predictor cannot know; on both machines.
- `set_int32`, int4 dense, 65 keys, on `shuffled`, the case whose
  inputs the form is for: every row selected, its values in random
  order, so that the steps of the halving follow neither the rows nor
  the words; on the Intel Mac, where the step is a branch; on AArch64
  it is a select and the case shows nothing there.

Each form is tried on each case, with `K` of 1, 8, 16, 32, 64 and 128
for the ring and the long input, as a hypothesis written down first.
Beside each runs a control: the same form, footprint, preparation and
schedule of reads, over inputs that repeat:

- for the ring, a ring of `K` copies of one input;
- for the long input, one input repeated `K` times;
- for the fresh input, the same permutation, from the same saved
  input with one seed, before every call, so that the preparation does
  the same work and leaves the same branches, and the data repeat.

A form and its control are two cases of one program, so their code
lies at the same addresses, and the difference of their misses is the
novelty alone; the controls' cycles against the plain case of the same
program, a ring of one input, show what the footprint or the schedule
costs by itself. `tessera-bench` compares no two builds whose
benchmark files differ, its listing included, so the pilot is judged
within its one build, and no case of it against a build before it.
Each case of the pilot also measures the case's reference over the
same inputs and schedule, as the tool asks of every case.

A form passes when, in both runs on each machine where its case
applies, and with no UNSTABLE reported:

- its misses a call reach at least 80 % of the model, while its
  control's stay near the plain case's;
- for the ring and the long input, its misses at `2K` are within 5 %
  of those at `K`, so the predictor learns nothing left; the fresh
  input, never repeated, needs no such step.

Of the forms that pass, the one that changes the conditions least is
taken: the fresh input if its control costs no more than the plain
case by the warning of 3 % of cycles; else the ring with the smallest
`K` that passes; else the long input, with the README saying that its
calls are `K` times as long. A pilot where no form passes stops the
work for the maintainer.

Refused:

- A larger random case of the same form. The predictor learned 7000
  branches; a few times more might not be enough on the next machine,
  and nothing would show it.
- Seeds that change from one process to the next. The three processes
  of a side would differ, and so would the two sides, by chance.
- Inputs drawn row by row from new seeds. Their counts of selected and
  NULL rows would differ, and with them the instructions of a call.
- A case of its own for the sets, of values spread evenly over the
  span of the keys in the order of the rows. A permutation of its words
  and of the rows within each word keeps the values of a word near one
  another, and the predictor learns the steps that a word's rows share
  from the rows before: the first pilot measured it.

### The hypotheses of the pilot

Written before the first pilot's runs. The models, from the cases
themselves (in `tests/benchmark.rs`, the test
`the_pilot_cases_hold_the_models_of_their_misses`):
11 misses a call for the fold of `column_reader`, as 11 of its 16
words hold a count of rows other than the commonest; 3068 of the 7168
steps for the halving of `set_int32` on the Intel Mac, over the case
the first pilot took, values spread evenly over the keys in the order
of the rows, by the model of a step.

- The ring: its misses rise with `K` and reach the model by `K` of 32
  on both cases, where its inputs pass L1, so that its control's
  cycles rise too while its control's misses stay at the plain case's.
- The long input: its misses reach the model at the same `K` as the
  ring's, and its calls cost `K` times the plain case's work.
- The fresh input: its misses reach the model at once, since no input
  repeats; its control's stay at the plain case's; the reads of the
  counters around each call add the same instructions to the form and
  to its control.

### The first pilot

Its runs, at commit `98b72bb`, all PASS and none UNSTABLE: on the
Intel Mac as root, `compare-5jX1S4` and `compare-p7fnPD` for
`column_reader`, `compare-ptHi4q` and `compare-kQdvT1` for
`set_int32`; on the M5 Pro, `compare-US05n5` and `compare-ef8glM` for
`column_reader`. The numbers below are medians of the blocks of the
six processes of a run, run by run.

Its case of the sets was the values spread evenly over the keys, with
a model of 3068 misses a call. No form reached 80 % of it: the ring
held 1081 to 1106 misses from `K` of 32, the fresh input 1118 and
1120. The model was wrong for that case, not the forms. A permutation
moves whole words and the rows within a word, so each word keeps 64
neighbouring values, near 4 of the 65 keys. The first steps of the
halving go the same way for all of them, and the predictor learns
those steps from the rows before. Computed within each word, node by
node, the model is 1013, and the forms reached it. By the rule above
the pilot stopped, and the maintainer chose to run the pilot of the
sets again on `shuffled`, whose values lie in random order, with its
model written first.

What the first pilot shows besides:

- The fold of `column_reader` misses more than its model: about 17 a
  call on the Intel Mac, and 15.4 on the M5 Pro at `K` of 128, where
  the model says 11. The predictor misses nearly every exit of the
  loop, not only those whose count is not the commonest. The rule asks
  for at least 80 % of the model, which these pass.
- The ring passes on `column_reader` from `K` of 16 on the Intel Mac:
  14.6 and 16.4 misses at 8, 16.5 and 16.6 at 16, 16.9 at 32. On the
  M5 Pro it passes from `K` of 64: 12.2 at 8, 13.1 at 16, 14.2 at 32,
  15.0 at 64 and 15.4 at 128; from 32 to 64 the misses grew by 5.0 and
  5.7 %. Its controls' misses stay within the plain case's, and their
  minimum cycles at `K` of 64 grow by 1.2 % at most on the Intel Mac
  and 2.7 % on the M5 Pro.
- The fresh input reaches the model on both machines, but its control
  costs more minimum cycles than the plain case by 37 % on the Intel
  Mac, 24 % on the M5 Pro and 5 % on the sets: past the warning of
  3 %, so it is not taken. Its preparation leaves misses of its own:
  5.5 a call in the control on the Intel Mac, 2.9 on the M5 Pro.
- The long input does not show novelty on the Intel Mac: its control,
  one input repeated, misses as much as the form, `16K + 1` a call, as
  the predictor does not learn a pattern that repeats 1024 rows later
  within one call.
- The plain case of `column_reader` has two modes on the Intel Mac: in
  some blocks the predictor learns it, at 0.07 misses a call, in others
  not, at 17. Its minimum cycles are 1831 and its median 2226, and its
  runs report 20 and 13 operations bistable. The ring from `K` of 32
  holds 16.9 to 17.0 misses.
- The reference of `column_reader`, a scalar loop with a branch a row,
  is learned in a ring of 16 inputs and not in one of 32 on the Intel
  Mac, where it misses 119 times a call in 2.5 times the cycles; on
  the M5 Pro it is learned up to a ring of 64. The ratio of the library
  to the reference that the reports print changes for the cases that
  take a ring.

### The hypothesis of the second pilot of the sets

Written before its runs. The model of `shuffled` by a node, from the
same test, is 1351 misses a call on the Intel Mac; by a step, without
the steps before it in the same search, it is 1792.

- The ring: its misses rise with `K`, and from `K` of 32 they stop
  growing and lie between 80 % of the model, 1081, and 1792, while its
  control's stay at the plain case's.
- The long input: its misses for each 1024 rows are the ring's at the
  same `K`.
- The fresh input: its misses reach the model at once, and its control
  costs about 5 % more cycles than the plain case, as on the first
  case.

The form then taken is the ring, with the smallest `K` that passes on
both cases and on both machines: by the first pilot, 64 or more.

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

- The pilot runs its two programs against themselves
  (`tessera-bench --base WORKTREE --filter /pilot-`), twice on each
  machine; the misses and cycles of its forms, controls and plain case
  are read off one report.
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

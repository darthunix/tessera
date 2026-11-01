## Context

See proposal.md, "Why", for the motivation. This section gives the
state of the drivers that shapes the design.

- A word is 64 rows. The kernels of `crates/tessera-kernels` read a
  column through a reader (`tessera_core::ColumnReader`). A reader
  that exposes the storage of a fully prepared word gives it as a
  block (`word_block`): values and a mask of non-NULL rows (dense), or
  Datums and 64 NULL flags (Datum). Vector code takes a block whole;
  the row path reads the selected rows one by one.
- Every family but the sets decides once a call, from one word:
  - `count`, the aggregates, the arithmetic, the cast, the comparisons
    and the hashes look at word 0, whatever it holds: whole words if it
    is full, or has at least `BULK_MIN_ROWS` (12) selected rows and is
    not a single row, and the reader gives its block;
  - the filters first handle the leading empty and single-row words,
    and decide on the first word with more than one selected row, in
    the same way.
- The sets decide on every word (`set::in_set`): whole if the set has
  at most `SetValue::WORD_KEYS` keys (64 int4, 16 int8) and the word is
  full or has at least 12 selected rows, counted before the block is
  asked for and with its NULL rows among them; only then do they ask
  for the block.
- The decision for a call sits in the function of the row loop: the
  row loop is inlined into the entry, with the leading single-row
  words, and the loop of whole words is a function of its own. On
  x86-64 the decision was compiled out before AVX2, so the row loops
  kept their registers; now they run short of them.
- `count` and the aggregates loop over whole words in one shape on
  x86-64, testing empty and single-row words at once, and in another
  on AArch64, where that shape cost the M5 Pro 14 % more cycles on a
  least of int4 over partly prepared words.
- `word_slots` in `crates/tessera-capi/src/column/ints.rs` makes the
  64 slots of a word from a slice with a checked range; `as_chunks`
  would index whole words and save six instructions of a whole word of
  `count` on x86-64, but it changes the `word_block` of every family.
- `try_fold_words` in `crates/tessera-capi/src/column.rs`, the fold of
  the selected rows that the aggregates, `count` and the reader's tests
  use, folds a full word by a counted loop in a function of its own,
  `try_fold_full_word`, and every other word by a scan of its bits.
- The benchmark cases: the reading fixtures (`support/reading.rs`,
  `support/reading64.rs`), which 10 programs share, have words of 8
  (`eighth`) and 32 (`half`, `random`) selected rows; the filters
  (`support/filtering.rs`, `support/filtering64.rs`) and the
  comparisons (`support/comparing.rs`) build matrices of their own,
  without `half`; `set_int32` adds words of 16 rows (`quarter`).
- What was measured, on the Intel Mac unless named, all on inputs the
  predictor had learned (`bench-unlearned-inputs`, "Why"):
  - words of 8 rows taken whole (`eighth`, `compare-KyEXSY` to
    `compare-tw1q3k`): 15 to 42 % fewer cycles on the filters, up to
    44 % on the hashes, 31 to 48 % on the cast, 45 to 62 % on `count`;
    25 to 85 % more on the sums of int4, 37 to 138 % on `min` and
    `max` of int8, up to 34 % on `+` by a constant, 42 to 44 % on the
    compare of Datum columns, 12 to 152 % on the sets;
  - the sets' bounds at 65 keys (`compare-5cYfxV`, `compare-VGt7XK`):
    65 int4 keys whole take 29 to 77 % fewer cycles on words of 32 to
    64 rows, about as many on words of 16, and 95 to 146 % more on
    words of NULL rows only; 64 int8 keys 23 to 55 % fewer on full
    words, up to 39 % more on words of 32 rows, 91 to 131 % on words of
    16;
  - the reader's fold without the call of a full word
    (`compare-6OWqws`, `compare-RxRTzU`): 5 to 27 % fewer cycles on
    sparse selections, words of 8 rows and calls of a row or none, 2 to
    7 times the cycles on full words;
  - on the M5 Pro the bound of 12 came from 19 to 29 cycles a whole
    word against 2.4 to 2.5 a selected row (`compare-ZxCFic`).
- `tessera-bench` compares only sides whose benchmark sources are the
  same, file for file, so a run cannot take a base from before AVX2,
  nor from before a change of any benchmark file.

## Goals / Non-Goals

**Goals:**

- Each family takes a word whole exactly when that costs fewer cycles
  than its rows, on each machine, to the step of the densities that
  the cases measure.
- A call that reads rows one by one costs on x86-64 no more than in a
  build without the vector path, within the noise of the benchmark.
- The decision of each call or word costs no more than today's.
- The results stay the same: the tests compare whole words with rows.

**Non-Goals:**

- No new kernel and no change of a kernel's vector code.
- No bound learned at run time: the bounds are constants of the build.
- No decision of the reader's fold beyond the call of a full word: the
  loops that keep values across a call stay `sturdy-bulk-loops`.

## Decisions

### Cases at every density, on unlearned inputs

Every word of a density case has exactly `n` selected rows, at
positions drawn anew for each input in the form that
`bench-unlearned-inputs` chooses, so that the branches of the row path
miss as on data never seen. The densities are 8, 12, 16, 24 and 32
rows a word. They are added where each family is measured: to the
reading fixtures of both widths, to the matrices of the filters and of
the comparisons, and to `set_int32`. The regular cases stay.

The base of every run comes after these cases, so that `tessera-bench`
can compare it.

### Each path measured on its own

A driver that takes a word whole at a density cannot show what its
rows would cost there, and the reverse. So each density is measured
between two builds that differ only in the bound:

- the rows build: no word is taken whole (a bound above 64);
- the whole build: every word the reader exposes is taken whole (a
  bound of 2).

`tessera-bench --base <rows build> --candidate <whole build>` on the
density cases gives, for each family, storage and density, the change
in cycles of taking the words whole. The bound of a family is the
least density from which the whole build gains, by the rule of
acceptance below, at that density and every denser one, on both
storages; a family whose storages part has a bound for each if its
driver can tell them apart, and the denser of the two otherwise.

Both builds are candidates on branches never merged. The bound becomes
a constant of each family, one for each architecture, beside the
family's driver; `BULK_MIN_ROWS` stays only while a family uses it. A
bound that one machine does not move stays one value, and its comment
says that both machines measured it.

Refused:

- A model fitted to the costs of the cases as they run today: at 32
  rows a word the drivers already take words whole, so the reports
  mix the two paths, and the regular cases are learned.
- One bound for every family: the measured spread above.
- A bound for each operation within a family: the drivers are shared
  by a family's operations; if their costs part, that is a finding.

### The bound of keys of the sets

The sets add a dimension: the number of keys. `set_int32` measures the
rows build against the whole build at its key counts, 4, 16, 17, 64
and 65, and at more if the transition falls between them, at every
density and on full words. For each key count `K`, the least density
from which a whole word gains is `n(K)`. The sets compute `n(K)` once a
call from a table of the measured key counts, taking for a `K` between
two of them the larger `n`, and compare each word's rows with it, as
they compare them with 12 today. `WORD_KEYS` goes: a set too large for
any word to pay has an `n(K)` above 64.

### Present rows

The sets count the rows that are present, selected and not NULL:
dense blocks give the mask of non-NULL rows with the block, and the
sets read the NULL flags of a Datum block for their result anyway. So
they ask for the block first, then count. A word without a present row
writes nothing found and nothing present, and skips both paths. The
order costs the words that go by rows a call of `word_block`; it is
measured on the cases with NULL rows and without.

The families that decide on one word count present rows too if
`nulls-all` shows that a whole word of NULL rows costs them more than
its rows.

### The cost of the decision on x86-64

What AVX2 costs the row paths is measured against a control: a build
of the base in which the token of AVX2 has no values, so that the
compiler removes the vector path and the decision, as before AVX2. On
the cases that go by rows (empty, sparse, single rows, partly prepared
words, and the densities under each bound), the control against the
base gives the cost of the vector path to those calls.

The decision for a call stays one test of the same conditions; what
moves is the code around it. Forms, each a hypothesis written down
first and checked by the x86-64 listing, then by the counters against
both the base and the control:

1. Today: the row loop inlined into the entry with the decision, the
   loop of whole words out of line.
2. Tried in the pilot of `simd-primitives-avx2` and reverted: the
   decision and the row loop out of line together. The row loop moved,
   and other rows lost.
3. The row loop out of line on its own, so that its function holds no
   code of the vector path and compiles as it does in the control; the
   entry keeps the leading words and the decision.

The goal is a form whose row cases are within the noise of the
control. The search follows "Stop and ask" of `CONTRIBUTING.md`. A
form that changes the code of AArch64 is measured on the M5 Pro too,
and is taken there only where it loses nothing; otherwise the form is
one for each architecture.

### The loops of `count` and of the aggregates

The x86-64 shape is tried on AArch64 again, over the new cases, and
the AArch64 shape on x86-64: one shape for both if it loses on
neither, two otherwise, as now.

### `as_chunks` in `word_slots`

`word_slots` indexes the words of a column cut by `as_chunks`, so a
word needs one comparison of its index, not a range. It changes the
`word_block` of every family, so it is checked by the listing of every
program on both targets and by the counters of every program whose
code it changes.

### The fold of a full word in `tessera-capi`

On x86-64 the call of `try_fold_full_word` costs the loop over the
other words. Forms, each a hypothesis written down first:

1. A run of full words folded by one call, which returns where the run
   ends: fewer calls, the same loop around it.
2. Each word folded by one of two functions out of line, full or not,
   so that the loop over words holds no body of either.

A form is taken on x86-64 when it gains on the words that are not
full and loses nothing on full words; the search ends as above, and a
search that ends without a form leaves the finding.

## Measurement plan

The rules of `docs/measuring.md` hold; the runs as root on the Intel
Mac are the maintainer's commands, and the M5 Pro's are relayed.

- The noise: the runs of identical sides that `bench-unlearned-inputs`
  records, and the same over the density cases once they exist, give
  the spread of minimum and median cycles of each case on each
  machine.
- The bounds: the rows build against the whole build, twice on each
  machine, over the density cases and full words of every program.
- Each bound, form and shape: the candidate against the base, twice on
  each machine where its code changes, `--filter` on the cases it can
  move and every case of a program whose code it changes; the forms of
  the decision also against the control.
- The rule of acceptance: a candidate gains on a case when its minimum
  and its median cycles both fall by more than 3 % and by more than the
  case's noise, in both runs. It loses on a case on 10 % more minimum
  cycles confirmed by a second run, or on a FAIL of instructions. A run
  with an UNSTABLE on a case the candidate changes is repeated, and a
  second UNSTABLE stops the work for the maintainer. A FAIL that the
  listing shows to be where the code lies is named and goes to the
  maintainer.
- The runs worth citing are copied to `docs/benchmarks/<date>/`.

## Risks / Trade-offs

- [A family's costs part by operation or by storage] → A bound for
  each storage where the driver can tell them apart; otherwise the
  denser bound, and the rest is a finding.
- [Counting present rows costs every word] → Measured on the cases
  with NULLs and without; taken only where it loses nothing.
- [The control is not the code before AVX2: the drivers changed since]
  → It is the same code with the vector path removed, which is the
  cost in question; the code before AVX2 cannot be compared.
- [A form moves the code of AArch64] → The M5 Pro's counters, and a
  form for each architecture.
- [Many runs: every program, two machines, two runs] → The candidates
  of one family share their runs; filters keep each run under five
  minutes.
- [One Intel machine decides for every x86-64] → As before, the AMD
  servers of the target platform stay `linux-x86-support`.

## Steps

Three pull requests:

1. The density cases, the bounds of the families and of the sets, and
   the present rows.
2. The cost of the decision on x86-64, the loops of `count` and of the
   aggregates, and `as_chunks`.
3. The fold of a full word in `tessera-capi`, and the close.

## Files

- `crates/tessera-kernels/src/lib.rs`,
  `crates/tessera-kernels/src/int/`,
  `crates/tessera-kernels/src/count.rs`,
  `crates/tessera-kernels/src/cast.rs`,
  `crates/tessera-kernels/src/set.rs`: the bounds and the decisions.
- `crates/tessera-capi/src/column.rs`,
  `crates/tessera-capi/src/column/ints.rs`: the fold of selected words
  and `word_slots`.
- `crates/tessera-capi/benches/support/`: the density cases.
- `docs/kernels.md`: the bounds.

## Tests

- `crates/tessera-kernels/tests/`: a reader that exposes its storage
  against one that does not, for every family, on both sets.
- `crates/tessera-capi/tests/columns.rs`: the fold of selected words.
- `crates/tessera-capi/tests/benchmark.rs`: the density cases against
  the fixtures' model.

## Context

See proposal.md, "Why", for the motivation. This section gives the
state of the drivers that shapes the design.

- A word is 64 rows. The kernels of `crates/tessera-kernels` read a
  column through a reader (`tessera_core::ColumnReader`). A reader
  that exposes the storage of a fully prepared word gives it as a
  block (`word_block`): values and a mask of non-NULL rows (dense), or
  Datums and 64 NULL flags (Datum). Vector code takes a block whole;
  the row path reads the selected rows one by one.
- Every family but the sets decides once a call, on its first word
  with more than one selected row: whole words if that word is full or
  has at least `BULK_MIN_ROWS` (12) selected rows and the reader gives
  its block, rows otherwise. The filters, the comparisons, the
  arithmetic, the aggregates and `count`, the cast and the hashes do
  so in `int/`, `count.rs` and `cast.rs`.
- The sets decide on every word (`set::in_set`): whole if the set has
  at most `SetValue::WORD_KEYS` keys (64 int4, 16 int8) and the word
  has at least 12 selected rows; the word's NULL rows are not counted.
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
- What was measured, on the Intel Mac unless named:
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
- All of these were measured on inputs the predictor had learned
  (`bench-unlearned-inputs`, "Why").

## Goals / Non-Goals

**Goals:**

- Each family takes a word whole exactly when that costs fewer cycles
  than its rows, on each machine, within the steps of the bound that
  the cases can show.
- A call that reads rows one by one costs on x86-64 what it cost
  before AVX2.
- The decision of each call or word costs no more than today's.
- The results stay the same: the tests compare whole words with rows.

**Non-Goals:**

- No new kernel and no change of a kernel's vector code.
- No bound learned at run time: the bounds are constants of the build.
- No decision of the reader's fold beyond the call of a full word: the
  loops that keep values across a call stay `sturdy-bulk-loops`.

## Decisions

### A model of the cost of a word

For each family, width, storage and machine:

- `W`, the cycles of a word taken whole, read off full words;
- `o + r·n`, the cycles of a word read by rows, with `n` selected
  rows: `o` a word, `r` a row, read off two densities, 8 and 32 rows a
  word, over inputs the predictor has not learned.

A word pays whole from `n* = (W − o) / r` rows. The bound of a family
is `n*` rounded to the cases that can show it, 8, 12, 16 or 24 rows,
and is confirmed on the cases at and around it. Words of 8 and 32 rows
exist (`eighth`, `half`); cases of 12, 16 and 24 rows a word are added
to the reading fixtures for this.

The bound becomes a constant of each family, one for each
architecture, beside the family's driver; `BULK_MIN_ROWS` stays only
as long as a family uses it. A bound that one machine does not move
stays one value, and its comment says that both machines measured it.

Refused:

- One bound for every family: the measured spread above.
- A bound for each operation within a family: the drivers are shared
  by a family's operations, and the model shows whether their costs
  part; if they do, that is a finding, not this change.

### The bound of keys of the sets

A word of the sets costs `a + b·K` whole for `K` keys, and `n·R(K)` by
rows, where `R(K)` grows with `K` linearly to 16 keys and by steps of
halving beyond. For a call, whose `K` is fixed, the sets compute once
the least rows from which a word pays whole, `n(K)`, and compare each
word's rows with it, as they compare them with 12 today. `WORD_KEYS`
goes: a set too large for any word to pay has `n(K)` above 64.

The model's constants come from `set_int32` on both machines, at 4,
16, 17, 64 and 65 keys and at 8, 16, 32 and 64 rows a word. More key
counts are added to the program if the fit needs them.

### Present rows

The sets count the rows that are present, selected and not NULL:
dense blocks give the mask of non-NULL rows with the block, and the
sets read the NULL flags of a Datum block for their result anyway. A
word without a present row writes nothing found and nothing present,
and skips both paths. The block is then asked for before the count,
not after it; what that order costs the words that go by rows is
measured.

The families that decide on their first word count present rows too
if `nulls-all` shows that a whole word of NULL rows costs them more
than its rows.

### Where the decision lies on x86-64

The decision for a call stays one test of the same conditions; what
moves is the code around it. Forms, each a hypothesis written down
first and checked by the x86-64 listing, then by the counters:

1. Today: the row loop inlined into the entry with the decision, the
   loop of whole words out of line.
2. Tried in the pilot of `simd-primitives-avx2` and reverted: the
   decision and the row loop out of line together. The row loop moved,
   and other rows lost.
3. The row loop out of line on its own, so that its function holds no
   code of the vector path and compiles as it did before AVX2; the
   entry keeps the leading words and the decision.

The search follows "Stop and ask" of `CONTRIBUTING.md`. A form that
changes the code of AArch64 is measured on the M5 Pro too, and is
taken there only where it loses nothing; otherwise the form is one for
each architecture.

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

The rules of `docs/measuring.md` hold, with the base after
`bench-unlearned-inputs`.

- The model: runs of every program with the base on both sides
  (`tessera-bench --base WORKTREE`), twice on each machine; the
  constants are read off their reports at the densities above. The fit
  and its residuals go into the design.
- Each bound: the candidate against the base, twice on each machine,
  `--filter` on the cases at and around the bound, plus every case of
  the program whose code the candidate changes.
- Each form: the listing of both targets; then the counters of every
  program whose code it changes, twice on each machine where its code
  changes.
- The rule of acceptance: a candidate is taken on a machine when it
  gains by more than 3 % of cycles in two runs, with no loss of 10 % of
  cycles confirmed by a second run and no FAIL of instructions, on
  that machine. A FAIL that the listing shows to be where the code lies
  is named and goes to the maintainer.
- The runs as root on the Intel Mac are the maintainer's commands; the
  M5 Pro's are relayed.

## Risks / Trade-offs

- [The model does not fit: a family's costs part by operation or by
  storage] → The bound is per storage or per operation where the
  drivers allow it; otherwise the larger loss is avoided and the rest
  is a finding.
- [Counting present rows costs every word] → Measured on the cases
  with NULLs and without; taken only where it loses nothing.
- [A form moves the code of AArch64] → The M5 Pro's counters, and a
  form for each architecture.
- [Many runs: every program, two machines, two runs] → The candidates
  of one family share their runs; filters keep each run under five
  minutes.
- [One Intel machine decides for every x86-64] → As before, the AMD
  servers of the target platform stay `linux-x86-support`.

## Steps

Three pull requests:

1. The cases of 12, 16 and 24 rows, the model, the bounds of the
   families and of the sets, and the present rows.
2. The place of the decision on x86-64, the loops of `count` and of
   the aggregates, and `as_chunks`.
3. The fold of a full word in `tessera-capi`, and the close.

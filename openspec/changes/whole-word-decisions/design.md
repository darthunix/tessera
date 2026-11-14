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
  (`eighth`) and 32 (`half`) selected rows, and of about 32, each row
  drawn with a chance of one half (`random`); the filters
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

A density case of `n` selects `n` rows a word on average: the words
hold `n`, `n - 1`, `n + 1`, `n - 2` and `n + 2` rows in turn, the
counts in an order drawn at random, at positions drawn anew for each
of 4096 inputs over one set of values. So the branches of the row path
miss as on data never seen,
the exit of a loop over a word's rows too: by the model of
`bench-unlearned-inputs`, on 12 of the 16 words of 1024 rows. The
densities are 8, 12, 16, 24 and 32 rows a word, the patterns
`density-8` to `density-32`. They are added where each family is
measured: to the reading fixtures of both widths, to the matrices of
the filters and of the comparisons, and to `set_int32`. The regular
cases stay.

The first form held exactly `n` rows in every word. Its runs of
identical sides, at `a357e036`, showed that the predictor learned
where a loop over a word's rows ends, as the count never changed. At 8
rows a word, which every family reads by rows today, the row paths
missed 0 to 4 times a call on the Intel Mac: the fold 0.0
(`compare-nKdYn6`), the aggregates and the cast at most 1
(`compare-MAa5l8`, `compare-qYmLQC`), the filter and the comparison
1.5 (`compare-GMAwPm`, `compare-PLmY7Q`), the hashes 1 to 4
(`compare-oKXyrt`). The fold over `random` misses 17. Over data the
count changes from word to word, and the exit misses on most words.
The row path would have been measured cheaper than it is, and the
bounds moved towards it.

The maintainer chose counts within two of `n`: the exit misses nearly
as over data, and two densities share one count at most. Counts drawn
as a selection of `n / 64` of the rows would be closer to data, but
they spread by 2.6 rows at 8 and by 4 at 32, so the cases of
neighboring densities would mostly hold words of the same counts.

Counts within two of `n` were not enough on the ring of 64 inputs that
`bench-unlearned-inputs` chose for the cases drawn at random. In their
runs of identical sides, at `28785e62`, the fold missed 0.4 times a
call at 8 rows on the Intel Mac and none on the M5 Pro, 2.8 at 12 on
both, where the model says 12; from 24 rows it missed 12 to 17. A loop
over a few rows takes a few branches a word, so the history that tells
the predictor where it is spans many words, and the predictor held the
exits of the 1024 words of the ring. Two probes on `probe-density-ring`,
a branch never merged, measured `density-8` and `density-12` of
`column_reader` over longer rings:

- whole inputs, rings of 256 and 1024, at `e36171fd`
  (`compare-92rqnx` on the Intel Mac, `compare-tmWkj1` on the M5 Pro):
  at 1024 the Intel Mac missed 12.6 to 13.9 times a call, the M5 Pro
  only 4 to 9 at 8 rows. A ring of 1024 inputs takes 13 MB, past the
  Intel Mac's L3, so its cycles measured the memory.
- selections drawn anew over one set of values, rings of 64 to 16 384,
  at `a90bda8f` (`compare-x8RFXs`, `compare-SLufTa`): a ring of 64
  selections missed about as the ring of 64 inputs, so the values play no
  part; at 4096, every operation missed 11.5 to 14.1 times a call on
  both machines. The cycles followed the misses, about 26 cycles a miss
  on the Intel Mac and 22 on the M5 Pro.

The density cases take 4096 selections, the fewest at which every
operation reached 80 % of the model on both machines, the rule of the
pilot of `bench-unlearned-inputs`; the maintainer chose them over
16 384, which miss 13.4 to 14.3 times. Their values are shared and stay
in L1, as those of a batch being processed do; only the masks go
round, 512 KiB read in order. `table_int32` gives the table of its
probes the keys of every selection, all the rows of the values, as the
table of a join's inner side serves batches of any selection.

The sets take the density cases over the values of their case
`shuffled`: the same values, in an order drawn at random. On x86-64 a
step of the sets' search by halving is a branch, and over values that
grow with the row the predictor learned the steps of the rows a
selection takes, whatever rows it takes. At 8 rows a word over 64 and
65 keys the Intel Mac missed 58 to 75 times a call
(`compare-yC36rk`), where a predictor that has not seen the values
misses about 135 by the model, 123 in the search and 12 at the exits.
On the M5 Pro a step is a select, and the order of the values does not
matter there.

The base of every run comes after these cases, so that `tessera-bench`
can compare it.

Their noise was measured at `57275b54`: every program with density
cases ran against itself on both machines, `tessera-bench --base
WORKTREE --filter /density-`. Between identical sides every change is
noise. No run failed and none was unstable. The Intel Mac gave no
warning; the M5 Pro warned twice, on `table_int32` over Datum at 16
rows, by +3.3 % on `probe/count_rows` and +4.6 % on
`probe/reference_count`. For each program: its runs, then the largest
change of the minimum cycles over the operations of 500 cycles or
more, of the median, and the largest MODES, Intel first.

- `column_reader`: `compare-bWb3mB`, `compare-LvwaWf`; 1.1 and 2.9 %;
  5.1 and 1.4 %; 1.46 and 1.28.
- `filter_int32`: `compare-m1G5Rb`, `compare-bfrSk1`; 1.0 and 1.1 %;
  3.4 and 0.3 %; 1.72 and 1.03.
- `filter_int64`: `compare-kpvo63`, `compare-eoFaRr`; 1.8 and 2.6 %;
  1.5 and 0.5 %; 1.49 and 1.05.
- `compare_int32`: `compare-pGHhrV`, `compare-6AxMjz`; 1.7 and 1.8 %;
  11.8 and 0.7 %; 1.68 and 1.76.
- `aggregate_int32`: `compare-EYORno`, `compare-8KJHcj`; 0.6 and
  3.7 %; 8.2 and 2.5 %; 1.92 and 1.43.
- `aggregate_int64`: `compare-DQLA2V`, `compare-gXUtrI`; 1.6 and
  3.6 %; 3.8 and 1.1 %; 1.92 and 1.87.
- `arith_int32`: `compare-6YzBdN`, `compare-gGyYjF`; 3.0 and 3.9 %;
  17.0 and 2.1 %; 2.11 and 1.22.
- `arith_int64`: `compare-y53ZOE`, `compare-WndOBc`; 1.2 and 3.1 %;
  12.8 and 2.7 %; 1.91 and 1.11.
- `cast_int32`: `compare-1BBN9E`, `compare-WxbCsD`; 3.1 and 0.8 %; 6.6
  and 0.9 %; 1.83 and 1.10.
- `hash_int32`: `compare-YumjSG`, `compare-s2Cop6`; 1.1 and 0.8 %; 2.4
  and 0.7 %; 1.72 and 1.45.
- `hash_int64`: `compare-UFcHbG`, `compare-XIPOaz`; 1.1 and 0.6 %;
  14.9 and 1.6 %; 1.70 and 1.62.
- `set_int32`: `compare-yC36rk`, `compare-OS3sqU`; 2.9 and 1.7 %; 22.5
  and 5.1 %; 1.66 and 1.55. These runs took the density cases over
  values in the order of the rows; they are measured again over the
  values of `shuffled`.
- `table_int32`: `compare-HKqNwk`, `compare-fdzWx7`; 2.4 and 7.2 %;
  9.3 and 4.7 %; 1.63 and 1.28. The 7.2 % is a fall, of
  `probe/count_rows` over dense values at 8 rows.

On the Intel Mac the processes of an operation ran at levels up to 2.1
times apart, so its medians moved by up to 22.5 % while its minimum
moved by 3.1 % at most.

At 8 rows every operation missed 13.5 times a call or more on the
Intel Mac and 11.3 or more on the M5 Pro, against the 12 of the model:
the cases reach it. The filters, the comparisons, the aggregates, the
arithmetic, the cast and the hashes decide on the first word. From 16
rows they take every word whole and miss 2 times a call at most; at 12
they take one path or the other by the input and miss 4.4 times or
more. Some operations read every word by its rows and miss 11 to 17
times at every density: `column_reader`, the checked `mul`, `div` and
`mod` of `arith_int64` with a scalar, and `count_rows` and
`reference_count` of `table_int32`.

Two sets before these are kept and explained:

- At `f056687a` each process built and checked the selections of every
  case of its program, not only of those it measured. Three runs on the
  Intel Mac took longer than five minutes: `set_int32` 750 s
  (`compare-3igWXl`), `table_int32` 563 (`compare-iL24ue`) and
  `arith_int32` 459 (`compare-iPDd61`); `set_int32` took 321 s on the
  M5 Pro. Since `57275b54` a process builds only the cases that its
  `--only` ids name. `set_int32` still took 308 s on the Intel Mac
  (`compare-yC36rk`), so its runs take one width at a time.
- At `57275b54` the first set on the M5 Pro failed once and warned
  twice. The failure was `add_column` of `arith_int32` over Datum at 8
  rows, +11.8 % on the minimum (`compare-H6tUt0`). The M5 Pro has one
  cluster of its fastest cores, 12 to 17, and two of slower cores, 0
  to 11. The processes of that set moved between them all through the
  run: 22 378 of its 38 400 blocks started and ended on cores 12 to 17
  and 4151 on cores 0 to 11. In the operation that failed, a block on
  cores 0 to 11 took 1162 cycles a call at the median, one on 12 to 17
  913. In the set above, 38 272 blocks of 38 400 ran on 12 to 17, as
  in the sets before. Both sets ran on mains power; why the processes
  of the first moved is not known.

### Each path measured on its own

A driver that takes a word whole at a density cannot show what its
rows would cost there, and the reverse. So each density is measured
between two builds that differ only in the decision. In both, the
whole decision of every driver, the test of rows, the shortcut of a
full word (`selected == u64::MAX`) and, for the sets, the bound of
keys, is replaced by one value that the compiler cannot see through
(`std::hint::black_box`), so that both paths stay compiled as they are
and only the value differs:

- the rows build: the value is false, and no word is taken whole;
- the whole build: the value is true wherever the reader exposes the
  block, and every such word is taken whole, whatever its rows or the
  set's keys.

Which path a case took is checked, not assumed: by the instructions a
call takes, which differ many times between the paths, and by the
listing of both builds.

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
  cycles confirmed by a second run, or on a FAIL of instructions. An
  UNSTABLE on a case the candidate changes decides nothing: the run is
  kept and explained, as `docs/measuring.md` asks, and never repeated
  for its result; its cause is a hypothesis of its own before another
  run. A FAIL that the listing shows to be where the code lies is
  named and goes to the maintainer.
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
- [A process of the M5 Pro moves to its slower cores, where a block
  costs up to 1.5 times more cycles] → A report names the cores of each
  operation; a run whose cores leave 12 to 17 is read with that in
  mind. The runner sets no class of quality of service, which would
  keep a process on the fastest cores; whether it should is the
  maintainer's to decide.
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

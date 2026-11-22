## Context

See proposal.md, "Why", for the motivation. In short: the kernels sum
`bigint` exactly in 128 bits, but leave each value of 19 digits to the
node, which adds it through `numeric` a row at a time. This section
gives the state of the code that shapes the design.

```
  a batch, words of 64 rows
          │
          ▼
  Integers (tessera-capi)            a value of 19 digits is Other
    bulk: fold_decimals ─────┐       on both paths today
    a row alone: term ──┐    │
                        ▼    ▼
  Sum, SumState (tessera-kernels)
    i128 sum and count, below 10^36
                        │
                        │ the rows refused: a rest mask
                        ▼
  the node (nodes/agg_generic.c)
    int64_to_numeric, numeric_add: a call a row
                        │
                        ▼
  the result: the numeric of the i128 sum plus the numeric rest;
  avg divides it by the count
```

- **The sum state.** `tessera_kernels::decimal::Sum` is the running sum
  of `sum` and `avg` of `numeric` and of the integers: an `i128` at the
  largest display scale met, kept below 10^36 in magnitude
  (`SUM_BOUND`), and the count. `SumState` adds the flags of NaN and the
  infinities and lays the state out in four words of a group's record,
  which partial aggregates send up. The bound leaves room to rescale the
  sum and its terms inside an `i128`; `state_partial` refuses a partial
  state past it.
- **The terms.** A source of a sum gives each row as a `Term`: NULL, a
  `Decimal`, NaN or an infinity, or `Other`, a row the caller adds by
  the core's means. A `Decimal` is a `numeric` of at most 18 digits, an
  `int64` at its scale; its rule of 18 digits carries the kernels'
  reads, comparisons and arithmetic of `numeric`. A source may also hand
  over a word's rows in bulk (`Terms::fold_decimals`): their values of
  one scale, which the loops add up as a word's sum, 64 values at most.
  The trait promises that the term of a row handed over in bulk is a
  `Decimal` of that value and scale.
- **The integers' terms.** `Integers` in `tessera-capi`'s
  `c/decimal.rs` reads `int4` and `int8` columns. Its bulk fold hands
  over a non-NULL row only when its value is below 10^18 in magnitude
  (`decimal_value`), and its term is `Decimal::new(value, 0)`, `Other`
  past 18 digits. So a `bigint` of 19 digits is `Other` on both paths.
  An `int4` never is.
- **The loops.** Two loops add the terms:
  - `decimal::sum_terms`, the sum without groups (`tess_decimal_sum`),
    takes a word's bulk sum by `Sum::add_many`, the other rows term by
    term by `Sum::add`, and sets the rest in a mask;
  - `accumulate::sum_terms`, the sums of groups
    (`tess_table_accumulate_sums`), takes a word's rows in order and
    keeps a local sum for each of its first 16 groups (`LOCAL_GROUPS`).
    At the first row of a 17th group it stops: that row and every later
    row of the word, whatever its group, go to their records one by one
    (`fold_term`, `SumState::add_to`, whose fast path takes a decimal of
    the state's scale without decoding the state). A local sum the
    record refuses at the bound is taken again row by row, decimals of
    the local sum's scale only. With 100 groups whose keys follow one
    another, a word keeps 16 rows and sends 48 one by one.
- **The rest in C.** Without groups, `fast_advance` in
  `nodes/agg_generic.c` takes each row the kernels left. When the
  128-bit sum is not zero it first moves it into the `numeric` rest
  (`fast_flush`), so that the next batches add from zero. Then it turns
  the row into a `numeric` (`sum_state_term`, `int64_to_numeric`) and
  adds it to the rest with `numeric_add`: two allocations and a free a
  row. With groups, `sum_state_rest` counts the row and adds its
  `numeric` to a rest whose address is the word after the record's
  state, without moving the 128-bit sum. The result is the same either
  way: both sums are exact.
- **The partial states.** A final aggregation merges partial states of
  the node's own format, words and a `numeric` rest, as before. A call
  whose sums include partial states reads an `int8` column, the partial
  values of `sum(smallint)`, through `PartialColumn::Int8`, a state of
  one value whatever its size; a call of `int8` columns alone reads them
  through `Integers`, with its rule of 18 digits.
- **The spill.** A grouping with such sums spills by rows: past
  `hash_mem` its table freezes, and the rows of new groups go to disk
  and come back through the same calls. No sum state goes to disk.
- **The checks of the bound** compare `value.abs()` with `SUM_BOUND`,
  in `Sum::add`, in `Sum::add_many` and on the way in: the states that
  `tess_decimal_sum` takes from C and the partial states that
  `state_partial` reads from bytes. The release build has no overflow
  checks, so `i128::MIN.abs()` stays negative and passes. No state the
  node writes holds `i128::MIN` today.
- **The tests.** `test/sql/agg.sql` sums a `bigint` column of 19 digits,
  `9223372036854775807 - i * 1000`, against the core's answers, with
  and without groups, past `hash_mem` and with workers; its comments
  call it "a bigint past 18 digits", a rest by design. One Rust test,
  `sum_states_accumulate_through_the_entry_point` in
  `crates/tessera-capi/tests/table.rs`, sums an `i64::MAX` through
  `tess_table_accumulate_sums` and expects its row in the rest, and the
  sum and count without it. The models of
  `crates/tessera-kernels/tests/decimal.rs` and `tests/table.rs` match
  `Term` and the rule of the bulk fold.
- **The benchmarks.** The family `anyagg` sums `bigint` columns of
  small values (`g_sum_int8`, `g_avg_int8`, `g_group_sum`); no family
  has a `bigint` of 19 digits. Its plans are printed for the cases named
  in two lists at its end. No program of `tessera-bench` calls the sums.

## Goals / Non-Goals

**Goals:**

- Every `bigint` value goes into the 128-bit sum on every path: the
  bulk of a word, a row taken alone, the groups past the 16 a word
  keeps.
- The answers stay those of the core's functions, to the digit and the
  display scale.
- The sums of values of at most 18 digits run the same instructions or
  fewer, within 1 %.

**Non-Goals:**

- `min` and `max` of `date`.
- A wider bound of the sum, or a second bound for integer sums.
- Any change of the partial format, the record's words or the spill.

## Decisions

### Every value of an integer column handed over in bulk

`Integers::fold_decimals` hands over every non-NULL row, of `int4` and
of `int8`. Its condition of 18 digits goes; nothing else in the bulk
loops changes. Their sums of a word already add each value as an
`i128`, and `Sum::add_many` checks the bound and rescales by checked
arithmetic. A sum of integers stays at scale 0: no term of an integer
column has another scale, and the partial states it merges come from
the same aggregate.

The promise of `Terms::fold_decimals` and of `DecimalWord` changes with
it: a row handed over in bulk is a decimal of that scale or, at scale 0,
an integer of any size, and its term is the one `Terms::term` gives.
The numeric `min` and `max`, which make a `Decimal` of every value
handed over (`extreme_of`), read numeric columns only, never
`Integers`; their documents say so.

### A value of 19 digits taken alone, as a term of its own

A row goes term by term when its word's bulk sum is refused at the
bound, and in the grouped loop when it comes at or after the first row
of a 17th group: a grouping whose keys follow one another sends most of
its rows that way. So the term of a single row must carry a value of 19
digits too.

`Term` gets a variant for it, an integer past 18 digits at scale 0.
`Integers::term` gives it for such a value and a `Decimal` for the
others, as today. Each place that takes a term takes the new one where
it takes a decimal of scale 0:

- `Sum::add` and `SumState::add`, as `add_many` takes a sum of one value
  at scale 0, refused at the bound as a decimal is;
- the fast path of `SumState::add_to`, when the state's scale is 0, so
  that the rows of further groups cost what a decimal costs. The fast
  path of a decimal stays as it was, and the integer's comes after it;
  one fast path for both, which built the pair of a value and a scale
  from either term, cost the groupings whose rows go to their records
  one by one 1.3 to 1.9 % more instructions;
- the plain loop's match of a row's term, through a function of its own
  kept out of line: only a row after its word's bulk sum was refused at
  the bound comes there, and inline, its copy of `Sum::add_many` cost
  the sums of `numeric` without groups 1.2 % more instructions; and in
  the grouped loop the retry of a local sum refused at the bound and the
  rows of further
  groups; the local sum itself needs no arm, since an integer column
  hands every value over in bulk;
- `ExtremeState::offer` and the extreme's loop, whose sources never give
  it, take it as `Other`.

Refused:

- **A `Decimal` of 19 digits at scale 0.** It would need no new variant,
  but a `Decimal` of 19 digits breaks the rule its other users rest on:
  the rescaling in `Sum::add`, whose bounds assume 18 digits, the
  comparisons and the arithmetic of decimals, and the writing of
  `numeric`.
- **The bulk alone.** One line, and it serves ClickBench's query 4,
  which has no groups. But a grouping whose keys follow one another
  would still send three rows in four of 19 digits to `numeric_add`.
- **Every integer as the new term.** One kind of term for an integer
  column is simpler to read, but every place that takes a decimal of
  scale 0 today would need the new arm on its path of 18 digits too.

### The checks of the bound by `unsigned_abs`

Every comparison of a magnitude with `SUM_BOUND` by `abs` uses
`unsigned_abs` instead. With values of 19 digits a word's bulk sum can
reach about 6 × 10^20; rescaled by 10^18 and added to a state of scale
18, the sum can land exactly on `i128::MIN`, which both checked
operations allow and `abs` does not measure. The node's integer sums
stay at scale 0, but the C entry points take any state, and the checks
on the way in let `i128::MIN` pass today. The cost is none:
`unsigned_abs` is as cheap as `abs`. The fast path of
`SumState::add_to` already compares both signs by one unsigned
comparison and stays as it is.

It is a fix of its own, with its own test, a state of `i128::MIN`
refused on the way in, so it comes in a commit before the term.

### The bound and the state stay

The bound of 10^36 stays for integer sums. An integer sum would reach
it only after about 10^17 rows of the largest values, so the path past
it through `numeric` stays, tested by the Rust tests that start from a
state near the bound. A bound of its own for integer sums, up to the
range of an `i128`, was refused: `SumState`'s words, their partial
format and the checks of `state_partial` would then hold two ranges,
told apart by the aggregate, for no row that occurs.

A state of its own for `bigint`, as the core's `Int128AggState`, was
refused: `Sum` is already a 128-bit sum and a count, and a second state
would need its own words, partial format, merges and spill.

### What stays in C, and how the Rust is checked

All the logic the change touches is Rust: the integers' terms in
`tessera-capi`, the term, the sum, its state and both loops in
`tessera-kernels`. The C of the node changes only in its comments. Two
pieces of C stay on the path of a `bigint` sum, by choice: the
`numeric` made of the final 128-bit sum and the division of `avg`, done
once a group by the core's own functions, which are the oracle of the
answer; and the `numeric` rest, which calls `numeric_add` and serves
the sums of `numeric` too. A copy of either in Rust would gain no time
and could part from the core.

The Rust is checked in two ways, each where it reaches:

- **Proofs by Kani** of the arithmetic of the integers, in a module of
  proofs beside `decimal.rs` that CI runs with the crate's other proofs.
  The states are those the entry points let in: below the bound, of a
  scale up to 18, with the count of an int64, as C keeps it, so that
  the rows a call adds, which the sum counts unchecked, never reach
  2^64. The terms and bulk sums are those of an integer column: at scale
  0, a value of an `int64` or a sum of up to 64 of them. For each scale
  of a state, a literal in its harness so that the power of ten is a
  constant, `Sum::add_many`, which takes a single integer term as a sum
  of one, never panics, and takes a term exactly when the exact sum, the
  term rescaled to the state's scale, fits an `i128` and is below the
  bound; a sum taken is that exact sum with the count, a sum refused is
  left as it was. A harness of its own shows a state of scale 18 and a
  bulk sum that land exactly on `i128::MIN` refused. `SumState::add_to`
  takes an integer in place as the decoded state does, with the same
  exact sum and count, at scale 0, its fast path, and at scale 1, the
  decoded path, whose arithmetic is `Sum::add_many`'s. Kani does not
  finish a product or a division of two unknowns: no harness needs one,
  since the factor is constant and terms at scale 0 never take the
  branch that divides the bound. Every scale ends within the limit of
  CI: on the M5 Pro the longest proof, scale 18, takes 87 s, and the 23
  proofs 711 s with their build. A bound of the count tied to the rows
  added, or one of 2^64 less a word, took one proof to 465 s.
- **Property tests by `proptest`** of the loops, too large for a proof:
  both loops of the terms against a model that adds row by row in
  `i128`, over random words of `int8` with NULLs, values of 18 and 19
  digits and the edges of `int64`, with one to 64 groups a word and
  from states near the bound. Near the bound the rows refused depend on
  the order, so each test asserts what does not: for each sum, the
  state's sum plus the values of its rest is the sum of its non-NULL
  rows, and the state's count plus the rows of its rest is their
  count. And the promise of the integers' terms: a row handed over in
  bulk and the term of the same row give the same value.

## Measurement plan

The change touches code inside the kernels and leaves the plans as they
are, so counts decide (`docs/measuring.md`, "Counts or times"). One run
of times answers the claim against the core.

**The cases.** The family `anyagg` gets a table of its own,
`bench_digits`, made once like the tables of `exec`: 1 000 000 rows in
the order of `g`; a key `r`, `g / 64 % 100`, whose values come in runs
of 64 rows, so that a word holds one or two groups; a key `p`,
`g % 100`, whose values follow one another, so that a word holds 64; a
`bigint` column `w` whose every value has 19 digits, of both signs, in
no order; and its twin `s`, `w / 10`, of 18 digits. Eight cases, each
of 19 digits beside its twin of 18, the sums of groups read by the
outer query:

- `g_sum_digits19` and `g_sum_digits18`: `sum` without groups;
- `g_avg_digits19` and `g_avg_digits18`: `avg` without groups, the
  shape of query 4;
- `g_runs_digits19` and `g_runs_digits18`: `sum` by `r`, every row
  through a local sum;
- `g_spread_digits19` and `g_spread_digits18`: `sum` by `p`, three rows
  in four one by one.

Both lists of the plans at the end of the family name the eight.

**The base.** The commit that adds the cases: the code without the
change of the terms. `cargo ab` runs the family of the checkout it runs
from on both sides.

**Hypotheses**, written before any run:

1. On the base, each case of 19 digits runs several times the
   instructions of its twin of 18. The whole of query 4 ran about 1075
   instructions a row, with 43 to 55 % of its time in this path; the
   twins give this path's own cost. On the candidate each case of 19
   digits runs within 5 % of its twin.
2. Every other case of `anyagg`, and the twins of 18 digits, run within
   1 % of the base's instructions, or fewer: the bulk loop of the
   integers loses a comparison a row.
3. With Tessera on, the cases of 19 digits on the candidate take less
   time than with it off. On the base they may take more: the base's
   ratio is recorded beside the candidate's.

**Runs**, each within five minutes, the agent asking the maintainer
before each:

- the machine code: `tessera-bench --base <base> --disasm --module
  kernels`, then `--disasm --bench <program>` for each program in turn.
  The loops of the integers' terms in `tess_decimal_sum` and
  `tess_table_accumulate_sums` grow no longer than the base's, and no
  program's loop changes, as no program calls the sums;
- the counts: `cargo ab measure --base <base> --family anyagg
  --counters --deadline 300` on the M5 Pro, on a server settled since
  its restart, judged by the rule of 1 % only when the control without
  Tessera stays at 1.000;
- the times: `cargo ab measure --base <base> --family anyagg --cases
  digits --rounds 1 --deadline 300` with the repetitions that fit, which
  installs each side's release build and times every case with Tessera
  on and off; the ratio of each side is read from its run, and its
  `plans.txt` shows a TessAgg in every case.

A hypothesis that fails stops the work for a profile, as
`docs/measuring.md` says.

## Risks / Trade-offs

- **A sum with a wrong digit.** The sum is exact on both paths, so a
  value counted twice, lost or added at a wrong scale would show only
  as a wrong answer. → The SQL suite compares every case with the
  core's answer, at the edges of `int64` too; the Rust tests compare
  both loops with a model that adds row by row, with values of 19
  digits in bulk, alone and past the 16 groups of a word.
- **The integers' loop of 18 digits.** Removing a condition could
  change how the compiler lays the loop out. → The machine code first,
  then the counts of the twins of 18 digits.
- **The bound left untested from SQL.** No query reaches 10^36 with
  integers. → The Rust tests start from a state near the bound, as the
  tests of `numeric` sums already do, and from one where a bulk sum
  rescaled lands on `i128::MIN`.

## Steps

The proposal, then the cases, then the terms with their tests, then the
measurements, then the close; tasks.md lists them.

## Files

- `crates/tessera-kernels/src/decimal.rs`
- `crates/tessera-kernels/src/decimal/proofs.rs`, new
- `crates/tessera-kernels/src/accumulate.rs`
- `crates/tessera-kernels/tests/decimal.rs`
- `crates/tessera-kernels/tests/table.rs`
- `crates/tessera-capi/src/c/decimal.rs`
- `crates/tessera-capi/tests/decimal.rs`
- `crates/tessera-capi/tests/table.rs`
- `crates/tessera-testing/src/lib.rs`, a strategy of int8 values of 18
  and 19 digits
- `.cargo/mutants.toml`, the equivalent mutants of the fast path
- `include/tessera/decimal.h`
- `include/tessera/table.h`
- `nodes/agg_generic.c`, comments only
- `test/sql/agg.sql`, `test/expected/agg.out`
- `test/sql/parallel.sql`, `test/expected/parallel.out`
- `bench/pg/anyagg.sql`, `bench/pg/README.md`
- `docs/nodes.md`, `docs/kernels.md`, `docs/table.md`

## Tests

- Kani, `crates/tessera-kernels/src/decimal/proofs.rs`: the proofs of
  the arithmetic of the sum the decision above lists.
- `proptest`, in `crates/tessera-kernels/tests/decimal.rs` and
  `tests/table.rs`: both loops of the terms against a model that adds
  row by row, over `int8` words with values of 18 and 19 digits,
  `i64::MIN` and `i64::MAX` among them, with one to 64 groups a word,
  from empty states and states near the bound: the sum taken and the
  rows left make up every value, and no row is left when every value
  fits in any order. The models of the extremes and of the groups take
  the new term.
- Rust, `tessera-kernels`: a word's bulk sum and a group's local sum
  refused at the bound taking their integers again one by one, eleven
  of 64; a state of `i128::MIN` refused by `sum_terms` and
  `merge_partials`, and a sum landing on it refused by `Sum::add_many`.
- `proptest`, in the tests of `tessera-capi`'s `c/decimal.rs`: the bulk
  fold and the term of the integers give each row the same value; beside
  it, a partial state of `i128::MIN` refused from its bytes.
- Rust, `tessera-capi`, through the raw pointers: `tess_decimal_sum` of
  an `int8` column at the edges of `int64` leaving no rest, and a state
  of `i128::MIN` refused with the sum as it was; in
  `sum_states_accumulate_through_the_entry_point` the row of `i64::MAX`
  leaves the rest for the sum and the count.
- SQL, `test/sql/agg.sql`: `sum` and `avg` of a `bigint` column with
  `-9223372036854775808` and `9223372036854775807`, with and without
  groups, many groups, past `hash_mem` with its rows to disk and back,
  and with workers, the plan shown, against the core's answers
  (`agg_same`).
- SQL, `test/sql/parallel.sql`: its `bigint` values of 19 digits, a
  rest of the partial states today, now merge in the words; the rests
  of partial states stay covered by its `numeric` values of more than
  18 digits.
- The mutants of the changed lines, locally and in CI.

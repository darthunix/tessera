# Roadmap

The queue of work: what is not built yet, in the order it is taken. An
entry is one piece of future work. When it is taken up it becomes a
change (`openspec new change <name>`, the entry's name), and when the
change is closed the entry leaves this file; what the change leaves
undone comes back here as a new entry. A discussion of what a feature
would take ends in an edit of this file or of a change.

Most entries were carried over from the working plan when it was frozen
on 2026-10-04; "plan", "the log" and "lines" refer to
[docs/plan/plan-migration.txt](../docs/plan/plan-migration.txt), where
the reasons are recorded at length. Nothing here is a promise of the
product: what Tessera does is in `openspec/specs/`.

## The queue

In this order. The maintainer placed `counters-on-x86-and-linux`, the
AVX2 kernels, `linux-x86-support` and `oltp-guard-bench` on 2026-10-05,
in the review of pull request 45; the first two are closed, and the next
stage of the first, `bench-in-four-cells`, keeps its place. The
maintainer placed `explain-rows-per-loop` first on 2026-10-08, after
pull requests 49 and 50, and `join-pruning-cost` after it the same day;
the other entries keep the order of the decisions of 2026-09-30 to
2026-10-02 (plan lines 2810–2836, 6596–6603, 7378–7390). The maintainer
placed `spill-crate` first on 2026-10-28, after the close of
`table-crate`, and the float kernels second on 2026-11-01, after the
review of what more can move from C to Rust; the float kernels are
closed. The maintainer took the six findings of the AVX2 kernels on
2026-11-01, in three changes, which come after `spill-crate` in this
order; the first, of the benchmarks' inputs, is closed. The maintainer
took six entries out of turn on 2026-11-02, after an analysis of TPC-H
with Tessera on, in the order
`deform-without-restarts` (done), `bpchar-constant-compare` (done),
`right-anti-join` (done), `agg-dependent-columns`, `early-key-filter`
(done), `batch-index-join`; on the same day he moved `agg-dependent-columns`
after `batch-index-join`, once a measurement of Q10 showed its
aggregate's input row-wise:

1. `spill-crate`: The spill as a crate of its own, relspill
2. `whole-word-decisions`: When and where a driver takes a word whole,
   on both machines
3. `relhash-prefetch-by-size`: The prefetch hint of `relhash` by the
   size of the table
4. `explain-rows-per-loop`: The rows a node removed, per loop as the
   core shows them
5. `join-pruning-cost`: The cost of a pruning join over the partitions
   left
6. `bench-in-four-cells`: `tessera-bench` in the four cells of the
   counters
7. `linux-x86-support`: Linux on x86-64 checked as the target platform
8. `oltp-guard-bench`: OLTP guard family of benchmarks
9. `runs-left-from-section-9`: Runs left to the maintainer from
   section 9
10. `tpch-short-set`: TPC-H step 7: the short query set for A/B
11. `tpch-parallel-and-jit`: TPC-H step 8: parallel series and the
    `jit = on` control run
12. `tpch-indexed-schema`: TPC-H step 9: schema with indexes on foreign
    keys and dates
13. `tpch-sf10`: TPC-H step 10: SF10
14. `backward-scan-mark-restore`: Backward scan and mark/restore
15. `postgresql-19`: PostgreSQL 19 support
16. `pg-duckdb-comparison`: Comparison with pg_duckdb

## By measurement

Taken when a measurement of the TPC-H runs shows the loss (plan lines
2810–2814):

- `spill-sum-states`: Spill of sum states instead of rows
- `numeric-reading`: Faster reading of numeric

## Later

After the queue, by decisions of 2026-09-20 to 2026-09-22 (plan lines
210, 2788–2799), which are older than the queue and put it after AVX2;
the AVX2 kernels are done, so these wait for the queue alone:

- `prosupport-batch-functions`: Extension batch functions through prosupport
- `table-key-types`: Keys of more types in the hash table itself; the
  maintainer moved it here from the head of the queue on 2026-10-07
- `join-pruning-other-keys`: Pruning by a join's keys of other ordered
  types; the maintainer placed it here on 2026-10-09

## Not ordered

These have no place in the queue. The maintainer places an entry when it
is wanted; several wait for a measured case. `greengage-port` and
`tpch-on-greengage` came down here from "Later" on 2026-10-05.

- `numeric-keys-by-digits`: numeric keys hashed and compared by their
  digits
- `window-functions`: Batch window functions
- `merge-join`: TessMergeJoin
- `minimal-tuples-for-storing-parents`: Minimal tuples for a parent that
  stores rows
- `mixed-inputs`: Mixed inputs
- `compressed-table-source`: Compressed test table AM with the source
  planning contract
- `parquet-fdw`: Parquet FDW
- `direct-arrow-batches`: Direct Arrow batches
- `datum-fallback-native-forms`: Datum fallback for date, timestamp and
  text representations
- `greengage-port`: Greengage 7 port
- `tpch-on-greengage`: TPC-H on Greengage over AOCO
- `types-outside-tpch`: Types outside TPC-H
- `remaining-expression-forms`: Expression and aggregate forms still on
  the row path or with the core
- `parallel-worker-page-faults`: Parallel workers: page faults and their
  diagnostics
- `parallel-final-grouping`: Parallel final stage of grouping for many groups
- `merge-append`: Batch MergeAppend
- `one-sum-state-shape`: "One sum state" record form
- `hot-code-alignment`: Alignment of hot code
- `avx-512`: AVX-512 as a third SIMD group, if AVX2 proves not enough
- `small-c-leftovers`: Small C leftovers of section 9
- `projected-batch-pins`: Pins of the last projected batch
- `documents-after-the-move`: The documents once every part has its folder
- `aggregate-states-and-sort-items`: The aggregate states and the items
  of a sort, out of the table's guide
- `core-regression-corpus`: PostgreSQL's regression suite as a corpus of
  Tessera's checks
- `force-mode`: A mode that plans Tessera's nodes wherever they are
  possible
- `explain-alternatives`: EXPLAIN shows the alternatives Tessera weighed
- `kernel-errors-on-demand`: A kernel's error only for the rows a plan
  needs
- `top-n-bound-in-scan`: The bound of a top-N sort in the scan below it
- `debug-checks`: A plan check, random column forms and a corpus of
  types in the debug build
- `sturdy-bulk-loops`: Loops over whole words that keep their values
  across the call of a vector kernel
- `pmu-on-x86-linux`: The counters on x86-64 Linux and on two kinds of
  cores, on hardware
- `pmu-on-crates-io`: The crate `pmu` published on crates.io
- `mutants-of-other-crates`: The mutants that survive in the crates
  beside `relhash`
- `relhash-on-crates-io`: The crate `relhash` published on crates.io

From the review of 2026-11-01 of what more can move from C to Rust and
what else would make Tessera more reliable, in the order the review
proposes. The gain of a move is one owner of a rule and a place where a
model test or a proof reaches it; most C calls PostgreSQL and stays.

- `nightly-checks`: Checks that run every night and every week
- `capi-miri-and-fuzz`: Miri over every test of `tessera-capi`, and a
  fuzz target of its arguments
- `golden-corpus`: The answers of PostgreSQL's functions as the
  kernels' oracle
- `kani-beyond-relhash`: Kani proofs of the kernels' arithmetic
- `zone-rules-in-rust`: The rules of local time of a zone in Rust, as
  fast as the C code
- `contracts-of-the-c-path`: Contracts of `tessera-capi` and
  `tessera-kernels`, with no memory taken
- `bounded-rust-calls`: Rust calls bounded in time
- `crosscheck-draws`: What the random queries never draw
- `condition-logic-test`: A random test of the logic of conditions
- `join-output-machine`: The join's rounds and tails as a Rust machine
- `valgrind-and-c-coverage`: Valgrind over PostgreSQL's memory, and the
  coverage of the C
- `linux-arm64-ci`: PostgreSQL on Linux AArch64 in CI
- `grouping-phases-machine`: The grouping's phases as a Rust machine
- `float-aggregate-transitions`: The float transitions of the generic
  aggregates in Rust

## Entries

### agg-dependent-columns

TessAgg with columns its keys determine.

- **What:** TessAgg takes a grouping whose target has columns that the
  grouping's primary key makes functionally dependent, which the core
  drops from the keys: it carries their values once a group, through
  the partial and final stages and the spill.
- **Why:** TPC-H Q10 groups by `c_custkey` and outputs five other
  customer columns: TessAgg refuses it (`not_from_groups`), and the
  core's HashAggregate takes 46 ms serial and 34 with workers.
- **Known:** Not a "first value" aggregate: only columns the planner
  proved dependent. Measured on 2026-11-02: the aggregate takes 35 to
  45 of Q10's 290 ms, its input comes row by row from an index nested
  loop with `nation`, and Q10 with the five columns as `any_value`,
  which TessAgg takes, ran no faster than the core's (303 ms against
  289); after `batch-index-join` the input is batches, and the entry is
  measured again before its proposal.
- **Depends on:** nothing.
- **Capabilities:** aggregate
- **Size:** medium: one pull request.

### batch-index-join

A join that looks a batch of keys up in an index.

- **What:** A nested loop whose inner side is a batch of index lookups:
  the outer batch's keys sorted, one descent each, the rows fetched into
  a heap batch and filtered in batches; inner, semi and anti.
- **Why:** The core's index nested loops are the largest block of TPC-H
  Q4, Q5, Q7 and Q21 serial and of Q3 and Q8 with workers: about 650 ms
  serial. The hash join over a full scan loses to them at SF 1.
- **Known:** It saves the executor's work around each lookup, not the
  index's; measure where the hash join wins first, with nested loops off.
- **Depends on:** nothing.
- **Capabilities:** a new node
- **Size:** large.

### numeric-keys-by-digits

numeric keys hashed and compared by their digits, in the grouping's
dictionary and in the join.

- **What:** The dictionary and the join hash and compare a numeric
  value from its own representation, as `hash_numeric` and `numeric_eq`
  do: the base-10000 digits without the leading and trailing zero
  groups, the weight, the scale left out of the hash, NaN equal to NaN,
  the infinities apart; the existing decimal kernels where both values
  fit them, the core's functions for the rest.
- **Why:** A profile of `any_numeric` (500 000 rows by 1000 numeric
  values, serial, 2026-11-02) puts 1710 of 5074 samples in the
  dictionary: `hash_numeric`, `cmp_numerics`, `cmp_var_common`, the
  unpacking and their fmgr frames.
- **Known:** `1.0` and `1.00` must stay one number and keep their own
  forms in the output. A hash of the value as a coefficient and a scale
  would split them; one through float would be wrong.
- **Depends on:** nothing; it extends the rule of a key's bytes,
  `tess_key_bytes` in `nodes/key_kind.h`.
- **Capabilities:** aggregate, hash-table
- **Size:** medium: one pull request.

### spill-crate

The spill as a crate of its own, `relspill`. The maintainer decided it
on 2026-10-28, after the close of `table-crate`; it is the change
`spill-crate`.

- **What:** The format of a block and of a file, the plan of a level,
  the words of a spill and the protocol of the join's, the grouping's
  and the sort's spills and of a shared build's rounds, as state
  machines, in a crate free of Tessera but `relhash`, with no input or
  output and no memory taken; the spill validated by outside reviews
  first; the checks `relhash` has.
- **Why:** About 7,200 lines of C decide a spill, with rules written
  several times; the format of a file is half in C; damaged bytes
  reach C that trusts them; the checks are thin.
- **Known:** The survey and the decisions are in the change's design.
- **Depends on:** nothing.
- **Capabilities:** relspill, spill-format, hash-table
- **Size:** large: about fifty pull requests.

### whole-word-decisions

When and where a driver takes a word whole, on both machines. Four
findings of the close of the AVX2 kernels and the entry
`x86-row-paths`, taken by the maintainer on 2026-11-01; it is the
change `whole-word-decisions`.

- **What:** A model of the cost of a word, whole and by rows, for each
  family on both machines, and from it a bound of rows for each family
  and a bound of keys of the sets that grows with the rows; the sets
  counting present rows; the place of the decision on x86-64, so that
  the row paths lose what AVX2 cost them; one shape of the loops of
  `count` and the aggregates if one serves both machines; `as_chunks`
  in `word_slots`; a form of the reader's fold of a full word on
  x86-64.
- **Why:** One bound of 12 rows fits no family on Intel: words of 8
  rows gain 15 to 62 % on some and lose up to 2.5 times on others. The
  sets compare whole words of NULL rows, 90 to 151 % dearer than their
  rows, and use one bound of keys for every word. The row paths of
  x86-64 lost up to 224 cycles a call when AVX2 came. The call of a
  full word in the reader's fold costs its other words 5 to 27 %.
- **Known:** The design of the change. The maintainer accepted the
  losses of the row paths for the pilot of the AVX2 kernels on
  2026-10-27, to be settled here once for every driver.
- **Depends on:** nothing. It measures with the cases drawn at random,
  which now take a ring of inputs that the predictor cannot learn.
- **Capabilities:** kernel-abi
- **Size:** large: three pull requests.

### relhash-prefetch-by-size

The prefetch hint of `relhash` by the size of the table. A finding of
the close of the AVX2 kernels, taken by the maintainer on 2026-11-01;
it is the change `relhash-prefetch-by-size`.

- **What:** The probe, the find or insert and the gathers of a batch
  give the hint only on a table larger than a bound of bytes for each
  architecture, chosen once a call; x86-64 gets `prefetcht0` on tables
  above that bound.
- **Why:** On x86-64 the hint gains 10 to 26 % of cycles on a table of
  160 MiB and costs up to 14 % on one in L1, so x86-64 gives none
  today. On AArch64 it is given to every table, and its cost on a
  table in the cache was never measured.
- **Known:** The design of the change. The hint fails the rule of 1 % of
  instructions and the contract's rule "No cost to Tessera" on every
  table where it is given; the maintainer chose on 2026-11-01 to propose
  narrowing both, the contract's rule in a pull request of contracts and
  designs alone. The forecast of the contract names `table/lanes.rs`
  alone for the hint of x86-64; a choice by size touches the loops that
  give it too.
- **Depends on:** the maintainer's decision on the narrowing of the
  rules.
- **Capabilities:** hash-table
- **Size:** small to medium: one pull request.

### explain-rows-per-loop

The rows a node removed, per loop as the core shows them. Found when
the key filter was described; the maintainer decided on 2026-10-08 to
follow the core.

- **What:** Every line "Rows Removed by …" of a Tessera node shows the
  rows per loop, as the core shows its own: the total over every loop
  and every participant, divided by the node's loops, which in a
  parallel plan add up over the participants. TessFilter does so
  already. TessHashJoin shows three such lines as totals, "Rows
  Removed by Join Filter", "Rows Removed by Filter" and "Rows Removed
  by Bloom Filter"; they become per loop. The other counters of
  Tessera stay as the core keeps its own of their kind: the work done
  (batches, rows read, chunks, partitions) as totals, the memory as a
  peak. TessAppend's `Subplans Removed by Join` becomes a mean too,
  per table built: the children the join's keys removed, summed over
  every build and every participant, divided by the prunings, shown as
  the core shows `rows`; the node's two counters of it add up over the
  builds instead of taking the last one's. The maintainer chose this
  on 2026-10-08 over Greengage's average and maximum and over the
  core's way, which prints no count of run-time pruning and leaves it
  to each child's loops. The specs of `key-filter` and
  `partition-pruning`, their designs, `docs/nodes.md` and the expected
  outputs follow.
- **Why:** A line named as the core's must mean what the core's means.
  The core's hash join shows "Rows Removed by Join Filter" per loop, and
  TessHashJoin in its place shows a total. The join suite has a join
  under a parameter that rebuilds its table ten times: it shows
  `rows=159.50` a loop beside "Rows Removed by Bloom Filter: 154713",
  the sum of the ten loops. By the code, in a parallel plan of three
  processes the TessFilter below a join shows a third of the join's
  line; no test shows it yet. `Subplans Removed by Join` shows the
  count of the last table built: a join rebuilt for two loops that
  removes three partitions and then two shows 2, where it removed 2.5 a
  build.
- **Known:** The core's rules, in `commands/explain.c`: per loop the
  rows, the time and every "Rows Removed by"; totals for buffers, index
  searches, heap fetches, the heap blocks of a bitmap, the hits and
  misses of Memoize and the batches of a hash aggregate; a peak, over
  loops and workers, for the memory of a hash and of a hash aggregate; a
  sort shows its last run. Tessera's memory is a peak in each process,
  summed over the participants: with a shared table the sum is the
  table, as the core shows it; without one every participant builds the
  whole table, and the sum counts it once a participant where the core
  shows one. Decide in the change whether that line follows the core
  too. `cargo tpch` already multiplies TessFilter's lines by the loops
  and reads no line of the join.
- **Depends on:** the capability `key-filter` and the second part of
  `hash-table` in `main`.
- **Capabilities:** key-filter, partition-pruning
- **Size:** small: one pull request.

### join-pruning-cost

The cost of a join that prunes its outer side, counted over the
partitions left. Found when the capability `partition-pruning` was
written; the maintainer decided on 2026-10-08 that it is to be fixed.

- **What:** In the cost of a hash join that expects to prune its outer
  side, count the probe side by the rows of the partitions expected to
  be left, as the outer child's cost already is: the outer keys hashed,
  the probe or the Bloom filter's test, the batches published, and the
  spilling of the probe side. The rows matched, the pairs and the join's
  output stay as they are: the partitions pruned hold no row with a
  pair. With fewer probe rows the share of rows with a pair grows, and a
  Bloom filter is expected less often. Count the pruning's own work as
  well: a comparison and a store an inner row, and a run of the pruning
  steps, about the logarithm of the partitions, a distinct key of the
  list.
- **Why:** Today the join's own terms count the rows of every outer
  partition, so a pruning join looks dearer than it is, and the planner
  may keep a plan that does not prune where one that does is faster.
- **Known:** Greengage's planner adds its partition selector after the
  plan is chosen and costs nothing. Its ORCA lowers the probe side's
  rows by a semi-join estimate of the rows that match the build keys,
  and costs both the scan and the hash join's probe by them; the join's
  output stays the same. Spark, Trino, DuckDB and Oracle add their
  run-time filters after the join order is chosen and estimate nothing;
  PostgreSQL costs no run-time pruning, and the costing proposed with a
  hash join's pruning there was not committed. Tessera expects whole
  partitions, which is what pruning removes, so it does not overstate
  the pruning as ORCA's row estimate does for coarse partitions.
  Measurement plan: the plans of the join suite's planner cases and of
  the join family of `bench/pg` before and after, and the time of every
  case whose plan changes, on an idle machine; a slower case stops the
  change.
- **Depends on:** nothing.
- **Capabilities:** partition-pruning
- **Size:** small: one pull request.

### bench-in-four-cells

`tessera-bench` in the four cells of the counters. Left by the close of
`counters-on-x86-and-linux`, 2026-10-25.

- **What:** Run `tessera-bench` in every cell the crate `pmu` counts
  in: on Linux without `sudo` where `kernel.perf_event_paranoid`
  allows the user, and with the cell (kperf or perf, NEON or AVX2) in
  its report, so that a verdict is never compared across cells. Then
  correct the texts that say its runs on Linux are planned: "Accepted
  limits" in `CONTRIBUTING.md`, `docs/measuring.md` and
  `crates/tessera-capi/benches/README.md`.
- **Why:** `pmu` counts in the four cells, but `tessera-bench` still
  runs its programs with `sudo` and reports no cell, so its verdicts on
  Linux, the target platform, cannot be taken.
- **Known:** The first stage changed only the crate, by the
  maintainer's decision of 2026-10-24. `pmu` counts on an M5 Pro and on
  an Intel Core i5-1038NG7 as root, and on GitHub's Arm runner at every
  push; on Linux it needs no root at `kernel.perf_event_paranoid` 2.
  The benchmark runner of `tessera-capi` repeats a block that reads
  zero. `tessera-bench` already runs on the Intel Mac as root: after a
  run of every program against itself there, the maintainer kept the
  limits of its verdicts on 2026-10-27, and the AVX2 kernels were
  accepted by its counts.
- **Depends on:** nothing.
- **Capabilities:** tools, bench
- **Size:** not estimated.

### linux-x86-support

Linux on x86-64 checked as the target platform. From the review of pull
request 45, 2026-10-05.

- **What:** Say what Tessera supports on Linux on x86-64 and check it
  beyond what CI does: build, run the suites, the families of `bench/pg`
  and `cargo tpch` on a Linux x86-64 machine, record the runs in
  `docs/benchmarks`, and fix what they show or enter it here. The list
  of checks is not decided; the proposal of the change sets it.
- **Why:** The target platform is Linux on x86-64; that the maintainer's
  machine is macOS on arm64 does not change it (the maintainer,
  2026-10-05). All of the project's timings were taken on that machine.
  On x86-64 the vector kernels run on AVX2, measured by counters on an
  Intel Mac alone.
- **Known:** The maintainer's words: Linux is covered by CI in some
  form, and it needs more than that. Checked in the code: on
  `ubuntu-24.04`, which is x86-64, CI runs the Rust checks and tests,
  Miri, the loom model, the sanitizers, the suites against PostgreSQL
  master with the release and the debug kernels, and the random queries
  against the core; it checks correctness, not speed. Every run in
  `docs/benchmarks` is from an Apple M5 Pro, and the defaults of the
  planner's cost parameters are the values measured there
  (`docs/costs.md`, "The reference machine"). The thresholds of the
  kernels that were tuned by time there were measured again on an Intel
  Core i5, and x86-64 kept them all (the documentation of
  `tessera-kernels`, "Code shape"). The tools ask macOS
  only for the description of the machine and of its power (`sysctl`,
  `pmset`) and choose the suffix of a module by platform. The counters,
  `tools/pmu`, count on Linux, but on x86-64 no machine with a PMU has
  checked them (`pmu-on-x86-linux`); huge pages are
  `parallel-worker-page-faults`.
- **Depends on:** a Linux x86-64 machine; nothing in code.
- **Capabilities:** ci, bench, tools, cost-model
- **Size:** not estimated.

### oltp-guard-bench

OLTP guard family of benchmarks. From plan 4.16, lines 597–598.

- **What:** The third family planned for `bench/pg`: "foreign queries" —
  `pgbench -S` with prepared statements, to show that the
  `set_rel_pathlist` hooks and the loaded modules do not slow OLTP down.
- **Why:** The tax and win families exist; this one was planned with
  them and never built.
- **Known:** The maintainer's decision 2026-10-05: in the queue, after
  the AVX2 kernels, which are done, so now after `linux-x86-support`.
  Checked in the code: `bench/pg` has no such
  family. The closest numbers are the start-up tax cases (`limit_1`: 3
  µs against 1–2) and `plan_exprs`/`plan_time`. The log does not record
  a decision to drop it.
- **Depends on:** nothing.
- **Capabilities:** bench
- **Size:** small.

### runs-left-from-section-9

Runs left to the maintainer from section 9. From plan 9.14, 9.20, 9.21;
lines 6998–6999, 7285–7286, 7352–7353.

- **What:** Three runs that the "done" paragraphs leave open: the
  processor-counter run for the bit-iterator change of 9.20
  (`tessera-bench --base`, needs sudo); the full manual cargo-mutants
  run on the new Linux arm64 runners of 9.21; the `plan_time` case of
  the join family for the planner helpers of 9.14. No code is expected
  unless a run finds something.
- **Why:** 9.20 was accepted by disassembly only (line 7284–7286). The
  full mutants run "was not started" after the move to `ubuntu-24.04-arm`
  (line 7352–7353). The 9.14 paragraph defers `plan_time` to "the
  measurement session of stage 3", and no later paragraph reports it
  (line 6998–6999).
- **Known:** Estimate for the full mutants run: about an hour per shard
  at 20 s per mutant (line 7353). `tessera-bench --base` refuses to
  compare across the commit that added the `mutants` profile, because
  `Cargo.toml` defines the build (line 7351–7352). The duplicates inside
  the Rust crates (plan 9.18) need no PMU run: the machine code of the
  13 benchmark programs stayed the same on each of their commits.
- **Depends on:** nothing.
- **Capabilities:** tools, ci, bench
- **Size:** small (runs, no code).

### tpch-short-set

TPC-H step 7: the short query set for A/B. From plan 8.1, lines 6246,
6265, 6361.

- **What:** Establish the short set Q1, Q3, Q6, Q9, Q18 as the A/B check
  for changes to the nodes: a run of at most five minutes that compares
  two builds.
- **Why:** The methodology of 2026-09-30 asks for it (line 6246: "a
  short set Q1, Q3, Q6, Q9, Q18 (after Kersten et al. 2018) for the A/B
  of the items of sections 4–5, five minutes at most").
- **Known:** Checked in the code: `cargo tpch` already accepts
  `--queries core` and has `compare <A> <B>`. The log does not say what
  step 7 must produce beyond that (a recorded baseline, a make target, a
  link from `cargo ab`).
- **Depends on:** nothing; section 9 went first (line 6602–6603).
- **Capabilities:** bench, tools
- **Size:** small.

### tpch-parallel-and-jit

TPC-H step 8: parallel series and the `jit = on` control run. From plan
8.1, lines 6243, 6249, 6266.

- **What:** Run the 22 queries with 2 workers (check, participation,
  timing) and one control run with `jit = on`; store the golden
  participation file for the 2-worker run.
- **Why:** The methodology asks for both (lines 6243, 6249). The log
  notes that parallel plans scale worse than the core's (4.19) and that
  TPC-H at SF1 passes the core's JIT thresholds.
- **Known:** Step 5 already checked answers with 2 workers and
  `work_mem` 4 MB: 20 of 20 same (line 6351). Checked in the code: the
  tool has `--workers` and `--jit`; `bench/tpch/` holds only
  `participation-sf1.txt`, no `-w2` file. Q17 and Q20 time out at 30 s
  on the primary-key schema in both modes (line 6346–6348).
- **Also carries:** the tax family was never measured with workers, so
  LIMIT in parallel plans is unmeasured (plan 6.4б, line 5265).
- **Depends on:** `tpch-short-set` by the order of the steps.
- **Capabilities:** bench, parallel-gather, cost-model
- **Size:** small unless the run finds losses.

### tpch-indexed-schema

TPC-H step 9: schema with indexes on foreign keys and dates. From plan
8.1, lines 6247–6248, 6267; list A item 5, line 6383.

- **What:** Run the check, participation and timing on the second schema
  variant (indexes on foreign keys and dates) and store its golden
  participation file. This is the first TPC-H run that uses the index
  modes of TessHeapScan (6.4а).
- **Why:** It changes the plans of Q17 and Q20, which time out without
  the indexes, and it shows whether merge joins over index scans get a
  core `Material` (8.9). Line 6474–6475: step 9 "will show whether
  mark/restore is needed".
- **Known:** Indexes on one key column or date of one table are allowed
  by the specification (line 6247–6248). Checked in the code:
  `bench/tpch/indexes.sql` and `--schema indexed` exist; no
  `participation-sf1-indexed.txt`. On the primary-key schema no plan has
  a Materialize above a Tessera node (line 6349–6350).
- **Depends on:** `tpch-parallel-and-jit` by the order of the steps.
- **Capabilities:** bench, scan-index, cost-model
- **Size:** small unless the run finds losses.

### tpch-sf10

TPC-H step 10: SF10. From plan 8.1, lines 6223, 6228, 6268; 4.16 tail,
lines 659–660, 5273–5275.

- **What:** Load and run SF10: `shared_buffers` 16 GB, `pg_prewarm`, Q11
  with fraction 0.00001, answers checked against DuckDB's SF10 answers
  after its SF1 answers are checked. Profile the serial path at this
  volume.
- **Why:** 8.8 waits for "stable SF1 and SF10" (line 6596). An older
  finding is open: on ten-times tables the serial on/off ratios are
  0.02–0.10 worse than at scale 1 (our path grows 30–70 % per row, the
  core's 10–20 %), "a separate profile" (lines 659–660, 5273–5275).
- **Known:** Q17 and Q20 hit a ten-minute limit at SF10 without indexes
  in pg_duckdb's own runs (line 6225–6227). The tool sets 16 GB at SF10
  (line 6288). The setop family was never measured at scale 10 (line
  4293).
- **Depends on:** `tpch-indexed-schema` by the order of the steps.
- **Capabilities:** bench, scan-heap
- **Size:** medium (a long run plus a profile).

### backward-scan-mark-restore

Backward scan and mark/restore. From plan 8.9, lines 6457–6475.

- **What:** Where the runs show a core `Material` above a Tessera node,
  add the missing ability, one step at a time and each measured against
  `Material`: mark/restore in TessSort (the position is a batch and row
  number in the sorted result); backward scan and mark/restore in the
  index modes of TessHeapScan; backward scan in TessHeapScan (pages and
  rows in reverse order, the same filter). The ERROR guard of the other
  nodes stays.
- **Why:** The core supports backward scan in SeqScan, mark/restore in
  Sort and both in Index Scan; Tessera's nodes do not, so a SCROLL
  cursor or the inner side of a merge join gets a `Material` that copies
  rows (lines 6464–6468).
- **Known:** The maintainer's decision 2026-09-30 to put it in the plan.
  No error can occur by construction; the cost is the copy. The tool of
  8.1 counts Materialize above Tessera nodes: zero on the primary-key
  schema at SF1 ("nothing to fix on the pk schema", line 6349–6350).
  Checked in the code: only TessSort sets
  `CUSTOMPATH_SUPPORT_BACKWARD_SCAN`; no node sets mark/restore.
- **Depends on:** `tpch-indexed-schema` (its facts decide whether
  anything is done).
- **Capabilities:** sort, scan-index, scan-heap, node-contract
- **Size:** medium — up to three separate steps, each optional.

### postgresql-19

PostgreSQL 19 support. From plan 8.8 and 4.33, lines 2555–2568, 6441–6445.

- **What:** Make the modules build and pass on REL_19_STABLE beside
  master. Three places differ: the layout of the table counters, the
  names of the abbreviated-key comparators, and the index-only scan by
  batches (about 60 lines of a copy of core 19 code). The branches go to
  `compat.h` as designed in 7.1, a row for 19 goes to CI, and the
  version check in `abi.h` drops to 190000.
- **Why:** pg_duckdb supports PostgreSQL 14–18 and its main builds with
  REL_19_STABLE; Tessera builds only with 20devel, so the comparison
  needs a common version (line 6441–6442).
- **Known:** The maintainer's decision 2026-10-01: only master now, 19
  together with 8.8 (line 2563–2564). With temporary replacements of the
  first two places and without the index suite, 40 of 41 suites passed
  against 19 beta 3; one parallel plan in `union` differs under 19 (line
  2562–2563). Lesson recorded: a lower version is proven by a build, not
  by a search for names (line 2574). Checked in the code: `abi.h` stops
  the build below 200000.
- **Depends on:** nothing in code; it introduces `compat.h`, which step
  7.1 of `greengage-port` also needs.
- **Capabilities:** bridge-api, scan-index, sort, ci
- **Size:** medium.

### pg-duckdb-comparison

Comparison with pg_duckdb. From plan 8.8, lines 6434–6456; step 11 of 8.1.

- **What:** Run the 22 queries in three modes on one PostgreSQL build
  and the same heap tables: the core, Tessera, and pg_duckdb with
  `tessera.enable = off`. Equal cores and declared memory limits, data
  in memory after `pg_prewarm`, two schemas, answers checked as in 8.1,
  separate statuses for pg_duckdb's fallback to the core and for
  TIMEOUT; result per query and the geometric mean.
- **Why:** pg_duckdb is the only rival on the same heap tables (line
  6434–6435). Its own numbers come from one run without a check of
  answers.
- **Known:** Research of 2026-09-30. pg_duckdb does not execute queries
  inside functions without
  `duckdb.unsafe_allow_execution_inside_functions`, so time is taken on
  the client (the tool already does). In the tool pg_duckdb is a third
  mode, set by commands at connection (line 6324–6325). Open question in
  the log: whether Tessera's hooks fire inside the heap scan pg_duckdb
  builds (line 6452–6453). Other extensions (pg_mooncake, Hydra and
  Citus columnar, ParadeDB, pgpro_tam, pg_deltax) read their own
  columnar copy and belong in a separate "after conversion" table (line
  6453–6456).
- **Depends on:** `tpch-sf10` (stable SF1 and SF10), `postgresql-19`.
- **Capabilities:** bench, tools
- **Size:** medium.

### spill-sum-states

Spill of sum states instead of rows. From plan 4.23 item 4б, lines 1574,
1634–1635, 1642–1643, 1738–1739.

- **What:** Groupings whose aggregates keep a sum state in the record
  (sum and avg over numeric and bigint, avg over integer and smallint)
  spill rows to disk today, and in the partial mode do not spill states
  at all. Write the states themselves and merge them on disk.
- **Why:** The log sets it aside until a measurement on many groups with
  sums shows a loss, naming TPC-H Q18, `GROUP BY l_orderkey` (line
  1642–1643).
- **Known:** Named again in the order of 2026-09-30 as "by measurement"
  (line 2813). In the partial mode merging on disk is "a word per
  aggregate" today (line 1634–1635). The log gives no design.
- **Depends on:** the TPC-H runs (`tpch-parallel-and-jit`,
  `tpch-indexed-schema`, `tpch-sf10`) for the measurement.
- **Capabilities:** aggregate, spill-format, hash-table
- **Size:** the log does not say.

### numeric-reading

Faster reading of numeric. From plan 4.23 analysis of 2026-09-30, lines
1740–1752.

- **What:** Two hypotheses to test: a fast path of numeric parsing for
  the short header and the scale of the sum; and parsing numeric while
  the tuple is deformed, so that the value's bytes are not read twice.
- **Why:** `sum(n)` without grouping is only ×0.40–0.435 of the core,
  where integer sums reach ×0.2. Three quarters of Tessera's time there
  is reading numeric (46 %) and deforming the varlena column (28 %).
- **Known:** Estimate for the first hypothesis: −3…4 ms of 19.4 (×0.40 →
  about ×0.33); the gain of the second is unknown. Not measured; the
  rule is to count instructions per row with objdump first (line
  1750–1752). Named in the order of 2026-09-30 as "by measurement" (line
  2813–2814).
- **Depends on:** nothing.
- **Capabilities:** type-support, aggregate, scan-heap
- **Size:** the log does not say.

### prosupport-batch-functions

Extension batch functions through prosupport. From plan 4.15, lines 578–582.

- **What:** After its table, the function registry's `find` asks the
  support function of a function (`pg_proc.prosupport`) with a request
  `ExtensibleNode{extnodename = "tessera.batch_function"}` carrying
  `abi_version`, `struct_size`, `funcid`, `inputcollid`. The answer is a
  `const TessFunction *` under the same contract as registered
  functions, or NULL. A cache by `funcid` is invalidated by PROCOID.
- **Why:** Another extension can then give a batch implementation of its
  own SQL function without registering it in Tessera's registry. The log
  gives no measurement.
- **Known:** Support functions must return NULL for a request they do
  not know. Built-in functions get no prosupport (the catalog is not
  edited) and stay in the registry. The test is an external function
  with a support function in a separate test module and
  `CREATE FUNCTION … SUPPORT`. Checked in the code: `bridge/function.c`
  says prosupport "is not asked". Since 4.27 the registry is a dynahash
  by OID (line 2144).
- **Depends on:** nothing; the old decisions ordered it after the AVX2
  kernels, which are done, so it waits for the queue alone.
- **Capabilities:** batch-functions, bridge-api
- **Size:** small.

### table-key-types

Keys of more types in the hash table itself. From the review of pull
request 48, 2026-10-07: asked for as early as possible, then moved to
"Later" the same day, once it was clear that the other types already
reach the table through a stand-in.

- **What:** Let the table keep and compare keys of more types itself,
  so that a join and a grouping stop paying for a stand-in. The table
  has two kinds of key, `int4` and `int8`. `int2`, `bool` and `date` go
  in as `int4`, and `timestamp` and `timestamptz` as `int8`. A key of
  any other type whose equality hashes goes in as a number of 64 bits:
  a grouping numbers its values in a dictionary of its own, in C; a
  join puts in the value's hash from the type's function, called
  through fmgr for every row, and then decides the match by the
  equality as a residual clause. Candidates, each measured before it is
  built:
  - more word kinds: `time`, `xid8` and `pg_lsn`, 8 bytes passed by
    value and equal exactly when their bits are, as `int8` at once;
    `money` too, which has no hash operator class, so that today its
    keys stay with the core altogether; `macaddr8`, 8 bytes passed by
    reference; `oid` and `"char"`, left out of the word keys on purpose
    (`types-outside-tpch`);
  - `float4` and `float8`, once -0 and every NaN have one form, since
    their equality is not an equality of bits;
  - keys of 16 bytes, such as `uuid`, in two slots;
  - `text`, `varchar` and `bytea` under a deterministic collation,
    where equal values are equal bytes: compared byte for byte by the
    kernels, through a reference to the value's copy, as a payload
    keeps such values.
- **Why:** The join's cost model, measured on the developer's machine,
  gives a hashed key 18.6 ns a row on top of 1.9 ns for a probe
  (`docs/nodes.md`), and keys of text are common outside TPC-H.
- **Known:** A record has one size and a slot of 8 bytes a key, so a
  key of variable length stays outside the record, in the node's chunks
  of values, or is compared through its hash and then its bytes.
  `interval` cannot be a word key: '1 month' and '30 days' are equal
  with different bits. A join of two different types other than
  integers stays the core's.
- **Depends on:** nothing. A new kind of key changes the table's
  format and its C API, so the work is an OpenSpec change of
  `hash-table`, and of the join and the grouping; in the nodes it adds
  its cases to `nodes/key_kind.h`, which owns the kinds of their keys.
- **Capabilities:** hash-table, join-hash, aggregate, type-support
- **Size:** several pull requests: the word kinds first, then keys of
  16 bytes, then strings.

### join-pruning-other-keys

Pruning by a join's keys of other ordered types, text first. Found when
the capability `partition-pruning` was written; the maintainer placed
it in "Later" on 2026-10-09, in the review of pull request 51.

- **What:** A hash join whose key the table keeps only by its hash, a
  `text`, `varchar`, `numeric` or another ordered type, notes the key's
  values for pruning as it notes a word's: a copy of each value while
  at most 1024 inner rows have a key, and the lowest and the highest by
  the column's order (the collation for text) always. TessAppend then
  prunes by them as it does by words, with parameters of the key's
  type.
- **Why:** Today such a join reads every partition of its outer side: a
  fact table partitioned by a code of text or numeric and joined with a
  few rows of a dimension is read whole. Trino, DuckDB and Spark
  prune by keys of any ordered type: Trino and DuckDB by the values
  while few and their ends past that, Spark by a list.
- **Known:** The join puts only a hash of such a key into its table
  (`table-key-types`), so the values come from the inner batches while
  the table is built. The price is a copy of a value an inner row while
  the list lasts, and a comparison through the type's function for the
  ends. The core's pruning compares by the partition key's collation,
  which must be the join's for a key of text; a key of another
  collation does not prune. A participant of a shared table must hand
  its values to the others through shared memory, in place of the
  words of 8 bytes the list holds now.
- **Depends on:** nothing.
- **Capabilities:** partition-pruning, join-hash
- **Size:** medium.

### window-functions

Batch window functions. From plan 5.13 step 5, lines 4649–4650,
4867–4868, 4977; 4.21, lines 969, 1012.

- **What:** A batch node for window functions. Today the core's
  `WindowAgg` runs them, and a query with window functions keeps the
  core's aggregation as well.
- **Why:** The batch tree breaks at nodes that do not exist (line
  4632–4633). Under a core `WindowAgg` TessSort is slower than the
  core's Sort: `out_sort` ×1.20–1.24 after 4.28 (line 2201–2202),
  because the window's tuplestore forms a minimal tuple from each
  virtual-slot row.
- **Known:** No design in the log. 5.13 step 5 is "missing nodes by the
  facts of measurements"; of its list INTERSECT/EXCEPT and the index
  scans are done, window functions and merge join remain, and the choice
  between them was left open (line 4977). The decision of 2026-10-01 on
  4.29 expects this node to remove the `out_sort` loss (line 2209).
- **Depends on:** nothing stated.
- **Capabilities:** planner-coverage, node-contract, sort
- **Size:** the log does not say; a new node.

### merge-join

TessMergeJoin. From plan 5.10, lines 4063–4092; 8.7, line 6432–6433.

- **What:** A merge join node over two sorted batch children (TessSort
  or an ordered source): keys are normalized words, batches are merged
  by a kernel that intersects sorted arrays (SIMD for I32/I64, galloping
  for very different densities), a run of equal inner keys is buffered
  in the node itself so the child needs no mark/restore, and a large run
  spills in the same format. Kinds: inner, left, right, full, semi,
  anti.
- **Why:** The core takes a merge join for inputs that are already
  ordered, for inputs larger than memory, and for FULL JOIN with
  conditions that do not hash.
- **Known:** Step 0 (2026-09-27) measured that there was nothing to win
  while index scans were not batch: in `merge_sorted` (both sides
  ordered by indexes) the core's Merge Join ran in both modes (59 / 68
  ms), and TessHashJoin with merge forbidden took 63–76 ms, that is, not
  faster (line 4078–4080). The node was "deferred until 6.4а" (line
  4092); RIGHT and FULL went into the hash join instead. 6.4а is done
  since 2026-09-28. The decision of 2026-09-27 says merge join is wanted
  for its own sake, not only by TPC-H facts (line 6433). Join pruning
  (5.14) and `backward-scan-mark-restore` mention it as absent.
- **Depends on:** `tpch-indexed-schema` would show merge joins over
  index scans in TPC-H; `backward-scan-mark-restore` is the alternative
  for the inner side.
- **Capabilities:** planner-coverage, node-contract, sort, spill-format,
  kernel-abi
- **Size:** large (a node, a kernel, spill).

### minimal-tuples-for-storing-parents

Minimal tuples for a parent that stores rows. From plan 4.29, lines
2205–2211, 2819–2823.

- **What:** When the parent is a core node that stores rows (WindowAgg,
  Material, Sort, Hash, CTE), find that out (an `ExecutorStart` hook or
  a walk of the plan after planning) and hand it minimal tuples formed
  by the batch instead of virtual-slot rows.
- **Why:** The rest of the `out_sort` loss, about 50 ms: forming a
  minimal tuple from a virtual slot in the window's tuplestore (35 ms
  against a memcpy of the core Sort's ready tuple) and TessSort's
  row-by-row output (about 26 ms).
- **Known:** The maintainer's decision 2026-10-01: not now; return to it
  if the loss under WindowAgg remains after batch window functions, or
  shows under Material, Sort, Hash or a CTE of the core. A side note in
  the same place: TessPack over a Subquery Scan with reordered columns
  makes it project (24 ms in `out_sort`); physical columns of the
  subquery under the pack would remove that, "separately, by
  measurement" (line 2209–2211).
- **Depends on:** `window-functions`.
- **Capabilities:** runtime-helpers, sort, pack
- **Size:** the log does not say.

### mixed-inputs

Mixed inputs. From plan 5.4, lines 3197–3198.

- **What:** Either side of a join and the child of a grouping may be a
  row plan of the core read through TessPack; that already works. The
  item is the rest: tests, cost, and a limit on pack over wide rows.
- **Why:** Point 8 of the "what blocks TPC-H" list (line 6167–6168):
  TessAgg over a join reads rows through TessPack with a copy of every
  column, a loss on wide rows that the 0.9× heuristic did not see.
- **Known:** The item has no "done" paragraph. Later items changed the
  ground: 4.28 gave TessPack a cost per copied value
  (`tessera.pack_value_share`) and TessAgg its own share over a copying
  pack; 8.10 replaced the join's multiplier with a time model. The log
  does not say whether that closes 5.4.
- **Depends on:** nothing.
- **Capabilities:** pack, cost-model, join-hash, aggregate
- **Size:** small, if anything is left.

### compressed-table-source

Compressed test table AM with the source planning contract. From plan
6.5 and 2.8, lines 6002–6003, 108–109; also 5222–5223, 2790–2791, 335.

- **What:** Port the compressed test table access method from pg_batch:
  first native batches, then pruning of groups and exact filtering. With
  it, add the planning contract of a source: the scan node passes the
  query's conditions to the source and the source answers, per
  condition, UNSUPPORTED, PRUNE_ONLY or EXACT, so PostgreSQL rechecks
  only what the source does not check itself. The same item carries what
  was postponed "to the first columnar source": lookup and caching of
  optional, named, versioned interfaces of physical columns, and typed
  int32/int64 columns passed to the kernels through FFI without widening
  to Datum.
- **Why:** It is the first source that produces native batches; only the
  heap source exists. The typed columns were valued as architectural (at
  most 1 ms on the `expr` case after the word-wise widening, line 2791).
- **Known:** 2.8 was deliberately deferred to this item (line 108–109,
  114–115). New source callbacks enter the ABI only with the first real
  consumer (line 6011). `get_datum_column` stays the mandatory common
  path (line 5203). pg_batch's nanoarrow submodule is not carried over
  (line 7478). Checked in the code: no PRUNE_ONLY classification exists.
- **Depends on:** nothing stated. `greengage-port` step 7.5 (registry
  walk over sources) touches the same contract.
- **Capabilities:** batch-format, bridge-api, node-contract, kernel-abi,
  filter, planner-coverage
- **Size:** large.

### parquet-fdw

Parquet FDW. From plan 6.6, line 6004, where it is an Arrow FDW; the
review of pull request 45, 2026-10-05.

- **What:** Add a foreign data wrapper over Parquet files as a source:
  it reads the files and gives the nodes Arrow batches. It takes the
  place of the plan's Arrow FDW.
- **Why:** The maintainer's decision 2026-10-05: a Parquet FDW gives the
  same Arrow batches, but from Parquet files, the form of cold storage.
  The plan gave the Arrow FDW one line and no reason.
- **Known:** The plan's item was to be done without nanoarrow as a Git
  submodule; pg_batch's dirty nanoarrow submodule is not carried over
  (line 7478). Nothing is decided about what reads the Parquet files.
- **Depends on:** `compressed-table-source` (the source contract).
- **Capabilities:** batch-format, bridge-api
- **Size:** the log does not say.

### sturdy-bulk-loops

Loops over whole words that keep their values across the call of a
vector kernel. From the review of pull request 68 on 2026-10-14.

- **What:** The loops that read a column a word of 64 rows at a time
  call a vector kernel for each word. Whether the compiler saves and
  restores the loop's values around that call depends on the rest of
  the function, so an unrelated change can make every word dearer. Give
  these loops a form that keeps their values without such saves: a
  kernel inlined into its loop, or fewer values alive across a call.
  Look by itself at the path of a word whose rows are not all prepared
  and at the division of a Datum column.
- **Why:** The masks of `relhash` lost a form of a window of bytes that
  nothing built; the kernels' loops then compiled otherwise. On 1024
  rows, aggregates over whole words took up to 12 % more instructions
  and a count over a Datum column 19 % more cycles, arithmetic up to
  2.5 % more instructions, while batches of 65 rows and counts over
  dense columns got faster. The maintainer kept the masks without the
  form and took the cost.
- **Known:** A try that marked `#[inline]` the `*_lanes` kernels of
  `simd/aggregate.rs`, `simd/aggregate64.rs`, `simd/arith.rs` and
  `simd/arith64.rs` took back most of it: against the masks with the
  form, the failing cases of `aggregate_int32` fell from 43 to 9 and of
  `arith_int32` from 31 to 22, and dense sums of columns and products
  ran 6 to 14 % faster. Left after it: aggregates of a Datum column over
  words not all prepared, 6 to 8.5 % more cycles; division of a Datum
  column, 5 %; Datum batches of 63 rows, 4 % more instructions; empty
  batches, five instructions a call.
- **Depends on:** nothing.
- **Capabilities:** kernel-abi
- **Size:** small to medium.

### direct-arrow-batches

Direct Arrow batches. From plan 6.7, lines 6005–6006.

- **What:** Hand Arrow batches to the nodes directly, with column
  selection and filter pushdown. The Arrow interface reports physical
  properties — time units, epoch, encoding, dictionary representation;
  the nodes do not derive them from the PostgreSQL type OID.
- **Why:** Follows the rule that a logical type and a physical
  representation are independent (lines 73–85).
- **Known:** Runtime picks a native kernel only when type, operation,
  collation and physical representation all match; otherwise the Datum
  path is mandatory (line 80–82). The masks of rows take words alone:
  their form over a window of bytes from a bit offset, the form of
  Arrow's bitmaps, went when nothing built it, so a batch of Arrow
  brings it back or copies its bitmaps into words (see
  `sturdy-bulk-loops` for what the form's absence cost).
- **Depends on:** `parquet-fdw`.
- **Capabilities:** batch-format, kernel-abi, type-support
- **Size:** the log does not say.

### datum-fallback-native-forms

Datum fallback for date, timestamp and text representations. From plan
6.8 remainder, lines 6007–6009.

- **What:** Check the contract on date, timestamp and text coming from a
  source in a native form: unsupported time units, collations and string
  representations must fall back to Datum.
- **Why:** The second half of 6.8 (comparisons of date and timestamp,
  min/max of date, text equality and LIKE under a deterministic
  collation) went to 8.2 and is done; this check is what is left, and it
  has a subject only once a source gives native representations.
- **Known:** The log does not say more.
- **Depends on:** `compressed-table-source` or `direct-arrow-batches`.
- **Capabilities:** type-support, batch-format
- **Size:** small.

### greengage-port

Greengage 7 port. From plan 7.1–7.10, lines 6013–6141.

- **What:** One tree and one set of modules for PostgreSQL and Greengage
  7 (PostgreSQL 12.22), with the differences kept in narrow seams.
  Steps, in the log's order:
  1. 7.1 Compatibility layer and build: `include/tessera/compat.h`,
     point edits, the `TESS_PARALLEL_QUERY` flag, Makefile that detects
     the core.
  2. 7.2 Own deform metadata (`TessDeformLayout`), no `CompactAttribute`
     or master-only fields; tax/win measured before and after on
     PostgreSQL.
  3. 7.3 Fork patches in a branch of the Greengage tree:
     `ExecSquelchNode` for `T_CustomScanState`, `planstate_walk_kids`,
     `plan_tree_mutator`; the maintainer rebuilds and restarts the
     cluster.
  4. 7.4 Smoke test on the demo cluster: filter and aggregate over a
     heap table through Tessera nodes on every segment; LIMIT, join,
     sort above the node; result in `docs/greengage.md`.
  5. 7.5 Walk of the node registry (`each`), `tess_batch_scan_path` over
     all sources.
  6. 7.6 `TessAocsScan` in `sources/aocs/` for ao_column tables, suites
     and timing.
  7. 7.7 TessAgg over two-stage aggregation (`agg_host_gg.c`).
  8. 7.8 `TessAoScan` for ao_row.
  9. 7.9 Tests on Greengage: `dynamic_library_path`, alternative
     expected files, `\if` by version, `make installcheck` against the
     demo cluster.
  10. 7.10 Later, by need: counters from segments in EXPLAIN, RLE runs
      as input to kernels, rewriting ORCA plans through `planner_hook`.
- **Why:** Greengage gives AO/AOCO tables and compression that
  PostgreSQL lacks; AOCO stores fixed-width values of a block in a row,
  which is real columnar input (lines 6017–6018, 6053–6055).
- **Known:** The maintainer's decision 2026-10-05: lowered, no place in
  the queue. Decision 2026-09-22, design only, no code. Facts from the
  fork's sources: ORCA bypasses path hooks, so `optimizer = off` is
  needed; there are no PostgreSQL parallel queries; plans travel to
  segments and modules must be preloaded in every segment backend;
  `ExecSquelchNode` crashes on a custom scan, so nothing runs without
  the fork patch; aggregation is two-stage; `-Werror` is on in the fork
  (lines 6038–6051). Elsewhere in the log: the node skeleton of 9.11 was
  written with Greengage nodes in mind (line 6911); the run-time
  partition pruning API differs and lands as one branch in `compat.h`
  (line 4490–4492); the own partial state format of sums was chosen
  partly because Greengage 7 lacks `numericvar_serialize` (line
  1617–1618); the builtin collation provider does not exist there (line
  3837); a batch motion is "a separate 7.x topic with fork patches"
  (line 5923–5925). Stale details in the item text: step 7.4 preloads
  `tessera_limit`, a module removed in 4.32; the checks speak of 30
  suites where there are 42 now.
- **Depends on:** the log treats 7.1 as doable in parallel with section
  8 (line 2798). `postgresql-19` is in the queue and creates `compat.h`
  first.
- **Capabilities:** greengage, bridge-api, scan-heap, aggregate,
  node-contract, planner-coverage
- **Size:** large — a series; steps 7.1–7.4 are the first milestone
  (line 6138–6139).

### tpch-on-greengage

TPC-H on Greengage over AOCO. From plan section 8, list A item 6, line
6384–6385.

- **What:** Run the TPC-H-derived queries on Greengage over AOCO tables
  through `TessAocsScan`.
- **Why:** Listed among the "later, by the facts of the runs" items of
  the TPC-H track.
- **Known:** Only the line "tpch on Greengage over AOCO (after 7.6)". The
  log does not say how the tool would talk to a Greengage cluster.
- **Depends on:** `greengage-port` step 7.6.
- **Capabilities:** greengage, bench
- **Size:** the log does not say.

### types-outside-tpch

Types outside TPC-H. From plan section 8, lines 6413–6415; 5.13 step 1,
lines 4678–4679.

- **What:** Batch support for the maintainer's wider set of types. The
  note listed bool, "char", int2, float4/8, tid, time, timestamptz
  "later, as a separate section". Most of it is done since: bool, int2,
  float4/8 and timestamptz have kernels (4.21) and bool, int2, date,
  timestamp, timestamptz are word keys (5.13 step 1). Left of the list:
  time (as int8), "char" and oid (as int4), tid (only for scanning).
- **Why:** The note of 2026-09-26 records the set as the maintainer's
  general one.
- **Known:** oid and "char" were left out of the word keys on purpose:
  an oid key would come out sign-extended, unlike the core's Datum (line
  4678–4679). 4.20 puts oid in lane class I32 and time and money in I64
  (line 931–932). Checked in the code: the kernels register nothing for
  time, oid, "char", uuid or interval; an interval comparison is row by
  row in the coverage suite. The keys of this list, `time`, `"char"`
  and `oid`, go with `table-key-types`.
- **Depends on:** nothing.
- **Capabilities:** type-support, batch-functions, batch-expressions
- **Size:** small per type.

### remaining-expression-forms

Expression and aggregate forms still on the row path or with the core.
From plan 4.21 and 4.23, lines 989, 1165, 1241–1242, 1313–1314,
1520–1521, 1643.

- **What:** A bundle of forms the "done" paragraphs name as left:
  comparison of timestamptz with a date
  (`date_trunc('month', d) = date '…'`); intervals; an IN list of more
  than 32 texts; an integer IN list longer than 64 int4 or 16 int8 keys
  goes by search (a bitmap of keys for a dense range "if needed");
  `ORDER BY` inside an aggregate and ordered-set aggregates ("later, by
  a case"); float aggregates and min/max of numeric in the parallel
  grouping with GROUP BY.
- **Why:** These lines still read `row` or `core` in the coverage suite;
  the log gives no measured case for any of them.
- **Known:** Checked in the code (`test/expected/coverage.out`): row by
  row are `round(n)`, text `>` under "C", `t || 'x'`,
  `date_trunc('month', d) = date`, `iv > interval`; with the core are
  `sum(DISTINCT numeric)`, `string_agg(… ORDER BY …)`,
  `percentile_cont`, ROLLUP, and `ORDER BY f8 LIMIT`. The log's "what we
  do not do" keeps regular expressions, ILIKE, upper/lower outside
  ASCII, md5, JSON, GROUPING SETS with the core or the row path (lines
  982, 1012–1013), so those are not part of this entry.
- **Depends on:** nothing.
- **Capabilities:** batch-expressions, batch-functions, aggregate, type-support
- **Size:** small per form; the log does not say which are wanted.

### parallel-worker-page-faults

Parallel workers: page faults and their diagnostics. From plan 4.19,
lines 794–842; 6.4а step 8, line 5690; 6.4б, lines 5265–5269.

- **What:** What is left of the page-fault item: check `huge_pages = on`
  and the density of faults on a Linux stand and write it down as an
  installation requirement; optionally a "Page Faults" line (getrusage,
  summed over participants) in `EXPLAIN (ANALYZE, VERBOSE)` of
  TessHeapScan, about 20 lines; a paragraph in `docs/nodes.md`. The old
  6.4б notes ask in the same area for counters per worker under VERBOSE
  and the I/O counters of the streaming read (`SO_SCAN_INSTRUMENT`).
- **Why:** A fresh worker pays about 1 µs per 8 kB page of shared
  buffers it touches first; the cost is equal for the core, but
  Tessera's scan does several times less work per page, so its workers
  are 2.7 times slower than the leader against 1.5 for the core, and the
  speed-up on the test machine is capped at ×1.7 with two workers (lines
  816–825).
- **Known:** Recorded 2026-09-21 as deferred, refined 2026-09-29. Parts
  (a) and (b) of the proposal are done as step 8 of 6.4а:
  `shared_preload_libraries` is the recommended setting, and the
  partial-path model has a start-up cost and a per-page cost of a
  worker. Nothing in Tessera cures the faults themselves;
  `MADV_POPULATE_READ` and `madvise` on macOS were not tried (line
  840–842). Step 8 leaves "huge pages on Linux" for after it (line
  5690). Checked in the code: no `getrusage`, no `SO_SCAN_INSTRUMENT`;
  `docs/costs.md` mentions huge pages in one line.
- **Depends on:** a Linux stand (`linux-x86-support`). The plan's
  accepted limits said that measurements on Linux were not planned (line
  7485); since 2026-10-04 they are, and the counters, `tools/pmu`,
  count on Linux.
- **Capabilities:** scan-heap, parallel-gather, cost-model
- **Size:** small.

### parallel-final-grouping

Parallel final stage of grouping for many groups. From plan 5.11 item
10, lines 4386–4401; 4.23, line 1644.

- **What:** When the groups are about as many as the rows, the parallel
  stack groups every row twice and cannot win. The one variant with a
  gain is a parallel final stage: the groups are divided among the
  participants by the hash of the key and each finishes its part.
- **Why:** GROUP BY over a UNION ALL subquery, 2.5 million rows and 2
  million groups, 2 workers: the stack takes 244 ms, the serial path 220
  ms (the core 422–480). The planner picks the stack on an estimate of
  200 groups, the default for a column of an append relation.
- **Known:** "the decision comes later, with a case in the measurements"
  (line 4401). Variants weighed: (a) an own estimate of the groups of an
  append relation — the core's paths would stay estimated at 200; (b)
  partial grouping stops grouping when it does not fold — at best a tie;
  (c) the parallel final stage — "closer to the shared table of section
  5 than to 5.11". Simply not building the stack does not help: the
  core's parallel plan (480 ms) would win on the same estimate.
  pg_vexec (compared on 2026-10-09) builds variant (c) as a node of its
  own between the partial and the final grouping in every participant:
  the partial groups go by the hash of the key into shared tuple
  stores of one shared file set, and past a barrier each participant
  claims partitions by an atomic counter and finishes their groups.
- **Depends on:** nothing.
- **Capabilities:** aggregate, parallel-gather, hash-table, cost-model
- **Size:** large.

### merge-append

Batch MergeAppend. From plan 5.11 е item 6, lines 4303–4305, 4527–4565.

- **What:** A node TessMergeAppend in `append.c` beside TessAppend:
  children give batches, the merge is done by the kernel
  `tess_sort_merge` over key words (as TessGatherMerge does), keys of
  other types by comparisons in C, the output is copied into the node's
  own batch. The merge code moves out of `gather.c` into a shared file.
- **Why:** Under a batch parent the core's Merge Append gives rows that
  TessPack copies.
- **Known:** Analysed 2026-09-29, recommendation: not now. Expected gain
  2–8 %, and only for ordered reads by indexes under a batch parent;
  without indexes TessSort over TessAppend is already faster than any
  merge. Return if a case appears: batch Index Only Scan (done since),
  correlated indexes in partitions, TPC-H with partitions. No parallel
  mode (the core's MergeAppend is never partial).
- **Depends on:** a measured case.
- **Capabilities:** append, sort, planner-coverage
- **Size:** medium — three commits planned (line 4527–4531).

### one-sum-state-shape

"One sum state" record form. From plan section 9, finding 13, lines
7428–7432; 9.16, lines 7110–7114.

- **What:** The group lookup `resolve_rows` is specialized by the
  record's shape (one or two keys, one or two words after the keys); a
  group with a sum state (flags and five words) always takes the general
  path. Add a specialized shape for "one sum state" (six words) — after
  an A/B measurement says it pays.
- **Why:** In 9.16 `sum(int2)` over 100 groups became 2–3 % slower with
  a cheaper addition (profile: `resolve_rows` 611 samples against 533).
  The same general path already serves every grouping with one sum or
  average of numeric or bigint.
- **Known:** "to be checked by an A/B measurement before a decision: an
  item of its own, not in section 9" (line 7432). The 2–3 % was accepted
  in 9.16.
- **Depends on:** nothing.
- **Capabilities:** hash-table, aggregate, kernel-abi
- **Size:** small.

### hot-code-alignment

Alignment of hot code. From plan 9.2 and 9.12, lines 6717–6721, 6962–6966.

- **What:** Decide whether and how to align hot loops (Rust) and
  functions (C), so that a neutral edit elsewhere does not move a loop
  head across a 64-byte line.
- **Why:** Twice an unchanged loop got slower only by its address: after
  9.2 division and remainder of int64 by a scalar are 16–50 % slower in
  cycles, the target of the backward jump lying in the last 4 bytes of a
  64-byte line; after the cut of `agg.c` `g_sum_int8` is +5 %, and with
  `-falign-functions=64` on both sides it is 1.01.
- **Known:** Both were accepted as they are; "the alignment of code (C
  functions, Rust loops, see the acceptance of 9.2) is a separate
  question" (line 6965–6966). `tessera-bench --disasm` reports the
  offset of loop heads within the 64-byte line, and `tessera-ab` takes
  `--copt` to test with alignment (9.19).
- **Depends on:** nothing.
- **Capabilities:** kernel-abi, tools
- **Size:** the log does not say.

### avx-512

AVX-512 as a third SIMD group, if AVX2 proves not enough. From plan 3.9,
line 214; the review of pull request 45, 2026-10-05.

- **What:** AVX-512 (VL/BW, masks in registers) as a third vector set:
  a third implementation of the primitives of `simd` and a token of its
  own, the kernels unchanged.
- **Why:** The log gives no gain; it only reserves the place.
- **Known:** The maintainer's decision 2026-10-05: only if AVX2 proves
  not good enough. Before it: "later, and only for a particular machine
  where it can be measured". AVX-512 exists on part of the Xeons and on
  Zen 4+, and lowers the frequency on old Xeons (line 212–213). What
  AVX2 showed on an Intel Core i5-1038NG7, an Ice Lake that has
  AVX-512 too: every family opened on it, and on full words of 1024
  rows its kernels take 79 to 93 % fewer cycles than the row path in
  the filters, the comparisons, the cast and the hashes, and those of
  the least and greatest of int8 62 to 83 % fewer on full, half and
  random words. Its limits, which AVX-512
  lifts: 16 registers, so the int8 kernel of the sets spills two masks
  a quarter of a word; no least or greatest of int8 lanes, which the
  kernels make of a compare and a select; the bits of a comparison
  taken by `movemask` rather than written to a mask register. No case
  was found that AVX2 does not serve.
- **Depends on:** a case that AVX2 does not serve.
- **Capabilities:** kernel-abi
- **Size:** the log does not say.

### small-c-leftovers

Small C leftovers of section 9. From plan 9.12, 9.17 and the survey;
lines 6936–6937, 6943, 6661–6662, 7220–7221, 1535–1536.

- **What:** Two small things: `nodes/internal.h` is still an umbrella
  that every `.c` includes, kept "for one release"; the comment of
  `join_many` in the join suite says memory goes over `hash_mem` while
  the output stays within it. The row spill of aggregation, its
  `RowWriter` and the merge of the three copies of `store_value` went
  to the change `spill-crate`, whose fix of an expanded value needs one
  copy.
- **Why:** One place per pattern; the survey counted these among the
  duplicates (lines 6661–6662).
- **Known:** Checked in the code: 21 files of `nodes/` include
  `internal.h`; the comment still says "the memory goes over it".
- **Also carries:** `generic_fits` has an unreachable INTERNALOID branch
  and counts an internal state as 8 bytes, "a separate trifle, not
  fixed" (plan 4.23, lines 1665–1667).
- **Depends on:** nothing.
- **Capabilities:** spill-format, aggregate, join-hash, runtime-helpers
- **Size:** small.

### projected-batch-pins

Pins of the last projected batch. From plan 4.24 item 17, lines 1778–1779, 1812.

- **What:** `projection_release` does not pass the release on, so the
  page pins of the last batch are held until the end of the scan; and
  add a ceiling of pages per batch by `GetAdditionalPinLimit`.
- **Why:** Verdict of the correctness review: pins do not pile up (a
  batch is 64 rows), so it was marked "later".
- **Known:** "17: later, as written above" (line 1812). Checked in the
  code: `GetAdditionalPinLimit` is not used anywhere.
- **Depends on:** nothing.
- **Capabilities:** scan-heap, batch-format
- **Size:** small.

### documents-after-the-move

The documents once every part has its folder. From the review of pull
request 44, 2026-10-04.

- **What:** When the last part of the system has its folder under
  `openspec/specs/`, rewrite "Where the knowledge is" of
  `CONTRIBUTING.md`, which says that a part without a folder is still
  described in `docs/` and that `docs/` holds the design of the nodes,
  the hash table, the spill and the costs; and turn the links of the
  README and of the guides in `docs/` to the capabilities.
- **Why:** That text describes the move while it lasts and becomes false
  when it ends.
- **Known:** `docs/` keeps the guides for authors of extensions. Each
  capability's pull request moves its own text and leaves a pointer
  behind.
- **Depends on:** the capabilities of every part.
- **Capabilities:** -
- **Size:** small.

### aggregate-states-and-sort-items

The aggregate states and the items of a sort, out of the table's guide.
Left by the change that described the shared build in `hash-table`,
2026-10-09.

- **What:** Describe what `docs/table.md` still holds in the
  capabilities it belongs to: the aggregate states a grouping keeps in a
  payload (`tess_table_accumulate`, `tess_table_accumulate_sums`,
  `tess_table_accumulate_extremes`) in the grouping's, and the items a
  sort makes of records (`tessera/sort.h`) in the sort's. Then delete
  the guide and turn its links (the README, `docs/bridge.md`,
  `docs/node.md`, `docs/nodes.md`, `tessera/table.h`, `tessera/sort.h`)
  to the capabilities.
- **Why:** These calls are C API too, and nothing ties their promises
  to tests; the guide is the last text of the table outside its
  capability.
- **Known:** The design of `hash-table` lists them under "What is not on
  this page".
- **Depends on:** the capabilities of the grouping and of the sort.
- **Capabilities:** aggregate, sort
- **Size:** a part of each of those capabilities' pull requests.

### core-regression-corpus

PostgreSQL's regression suite as a corpus of Tessera's checks. From the
comparison with pg_vexec on 2026-10-09.

- **What:** Two runs of PostgreSQL's own `parallel_schedule`. First,
  the suite passes with its expected files unchanged, with Tessera
  installed, preloaded and off (`tessera.enable = off`), and preloaded
  and on where its plans print nothing of Tessera. Second, a
  differential run: every query of the suite with Tessera on, and in
  `force-mode`, against Tessera off; rows compared as multisets unless
  the query orders them, plans masked, errors and their SQLSTATE
  compared from the server's log. Each known difference is kept as a
  file that must match byte for byte, with its reason beside it; the
  run fails on any other. A CI job runs both.
- **Why:** Tessera's suites and `tessera-crosscheck` test the queries
  their authors thought of. The core's suite holds thousands of queries
  over every type, NULL, collation and error the core promises, with
  their answers. pg_vexec runs it this way and keeps 16 differences a
  session.
- **Known:** pg_vexec reads the SQLSTATEs from the log by
  `log_line_prefix = '%a|%e|'`, masks volatile output by regular
  expressions kept with the differences, and stores no reasons; Tessera
  should. The suite needs its `regress` module built against the same
  server.
- **Depends on:** nothing; `force-mode` widens it.
- **Capabilities:** ci
- **Size:** medium: a runner, a CI job and the review of the first
  differences.

### force-mode

A mode that plans Tessera's nodes wherever they are possible. From the
comparison with pg_vexec on 2026-10-09.

- **What:** A setting under which every planner hook of Tessera keeps
  its path and drops or disables the core's alternatives, so that a
  node of Tessera stands wherever one can; and a debug setting that
  raises an error when a node could have stood and the plan has none.
- **Why:** The suites, `tessera-crosscheck` and `core-regression-corpus`
  check only the nodes the costs chose; with this, every query goes
  through Tessera's nodes where they apply. pg_vexec's `vexec.mode =
  force` and `vexec.debug_require_vector` do so.
- **Known:** A path that only the core has (an index scan for a nearest
  neighbour, say) stays. The setting changes the plans visible from SQL,
  so it is a change with its spec.
- **Depends on:** nothing.
- **Capabilities:** planner-coverage
- **Size:** medium.

### explain-alternatives

EXPLAIN shows the alternatives Tessera weighed. From the comparison
with pg_vexec on 2026-10-09.

- **What:** An option of EXPLAIN, `TESSERA`, that lists for each place
  where Tessera offered a node the cost of its path beside the core's
  it lost or won against, and for each place it could not offer one the
  reason: a join type, a key type, a setting. The planner records them
  through `planner_setup_hook` and `planner_shutdown_hook` into the
  plan's extension state, and `explain_per_plan_hook` prints them only
  with the option.
- **Why:** Today a plan without Tessera's node says nothing of why;
  each such case of the cost models (TPC-H Q5, a pruning join) was found
  by reading code. pg_vexec's `EXPLAIN (VEXEC)` prints "not chosen",
  the two costs and the reason.
- **Known:** pg_vexec caps the records at 256 a query and keeps one a
  join relation, cheapest first; EXPLAIN without the option prints
  nothing, so the expected outputs of every suite stay.
- **Depends on:** nothing.
- **Capabilities:** planner-coverage, cost-model
- **Size:** medium.

### kernel-errors-on-demand

A kernel's error only for the rows a plan needs. From the comparison
with pg_vexec on 2026-10-09.

- **What:** A kernel raises no error: it marks the rows on which the
  function would fail, and goes on. A node raises the error of a marked
  row only when the plan needs that row, by computing it again with the
  core's evaluator, in the order of the rows, so that the SQLSTATE and
  the message are the core's.
- **Why:** `docs/limitations.md` accepts that a data error in a row the
  core's plan never reaches, past a LIMIT or in a row an earlier clause
  removes, can stop a query under Tessera. pg_vexec keeps the core's
  behaviour this way.
- **Known:** It changes the kernels' entry points (a mask of failed rows
  beside the status) and every node that evaluates expressions: a
  change of the C API and of the architecture, the maintainer's call.
- **Depends on:** nothing.
- **Capabilities:** kernel-abi, batch-expressions
- **Size:** large.

### top-n-bound-in-scan

The bound of a top-N sort in the scan below it. From the comparison
with pg_vexec on 2026-10-09.

- **What:** TessSort with a bound hands the scan below it the worst key
  it keeps, and the scan drops a row whose first key is past it before
  it deforms the row's other columns, and before the clauses where
  they cannot fail. For a small bound, the sort may carry only the keys
  and the tuple's id, and fetch the other columns of the rows it
  returns.
- **Why:** `ORDER BY ... LIMIT` over a large table reads and filters
  every row today, though the top-N keeps only the rows past the bound.
  pg_vexec does both.
- **Known:** Ties stay: only a key strictly past the bound drops.
  Measure first, on ClickBench-like `ORDER BY ... LIMIT` queries.
- **Depends on:** nothing.
- **Capabilities:** sort, scan-heap
- **Size:** medium.

### debug-checks

A plan check, random column forms and a corpus of types in the debug
build. From the comparison with pg_vexec on 2026-10-09.

- **What:** Three checks of the assert build. After planning: every
  plan node's id unique, no node of Tessera when Tessera is off. A
  setting with a seed that draws, for each column of each batch, the
  native form or the Datum form, so that one run of the suites goes
  through both paths. And a corpus of a table of every class of type
  Tessera reads, with NULLs at the edges of a mask's word and of a
  batch, over several batches.
- **Why:** Tessera compares the native path with the Datum path only
  where a test of a type does it by hand. pg_vexec checks its plans so
  in its debug build and runs its differential suite with random
  layouts as one of its sessions.
- **Known:** The seed must be printed so that a failure repeats.
- **Depends on:** nothing.
- **Capabilities:** batch-format, planner-coverage
- **Size:** small to medium.

### pmu-on-x86-linux

The counters on x86-64 Linux and on two kinds of cores, on hardware.
Left by the close of `counters-on-x86-and-linux`, 2026-10-25.

- **What:** Run the tests of `tools/pmu` with `PMU_REQUIRE=hardware` on
  an x86-64 Linux machine with a PMU, Intel and AMD, and on a processor
  with two kinds of cores, an Intel of the 12th generation or later or
  an Arm board with big and little cores, bound and not bound to one
  kind. Correct what they show.
- **Why:** The cell perf × AVX2 has never counted on hardware: GitHub's
  x86-64 runners give no PMU. The choice of the core PMU on two kinds
  of cores is tested only over written sysfs trees.
- **Known:** CI runs the same code of the group on software events on
  x86-64, and real counts on GitHub's Arm runner. A Google Cloud C4
  machine gives a guest a PMU with `--performance-monitoring-unit`;
  Hyper-V gives one only on Intel and only when asked. The README of
  the crate asks the first user with such a machine to run the tests.
- **Depends on:** such a machine; nothing in code.
- **Capabilities:** tools
- **Size:** small, unless the machine shows a fault.

### pmu-on-crates-io

The crate `pmu` published on crates.io. Left by the close of
`counters-on-x86-and-linux`, 2026-10-25.

- **What:** Publish `tools/pmu` at version 0.1.0, once the maintainer
  decides it: its repository and documentation links, a tag, and the
  first `cargo publish`.
- **Why:** No crate on crates.io counts in the four cells with the same
  meaning in each; the crate was made to stand alone for that.
- **Known:** CI already packs the crate alone (`cargo publish
  --dry-run`), watches its public API on five targets, and builds its
  documentation without warnings. Its license is BSD-2-Clause, its
  least Rust version the workspace's.
- **Depends on:** the maintainer's decision.
- **Capabilities:** tools
- **Size:** small.

### mutants-of-other-crates

The mutants that survive in the crates beside `relhash`. Found by a
run of every mutant of the workspace, at the maintainer's request,
2026-10-27.

- **What:** Run 40518206966 of `.github/workflows/mutants.yml` made
  5389 mutants of every line of the workspace. The 1201 of `relhash`
  were all caught. Elsewhere 353 survived: 297 in `tessera-kernels`
  (the calendar 64, the comparisons and arithmetic of integers 86,
  decimals 23, hashing 19, the sort 19, filters 16), 34 in
  `tessera-capi` (decimals 18, the calendar 14), 21 in `relspill`
  (the packing of a record 15) and 1 in `tessera-core`. Give each a
  Rust test that catches it, or list it in `.cargo/mutants.toml` with
  the reason it is equivalent.
- **Why:** Only Rust tests judge a mutant. Code that only the SQL
  suites reach has no judge, so a fault there shows only through
  PostgreSQL, if at all. A pull request's mutants of its changed lines
  find new gaps, not these old ones.
- **Known:** The run's shards took two and a half to three and a half
  hours. A run by hand for one crate takes the workflow's input
  `package`. Many survivors are likely entry points that only C calls,
  and some will be equivalent.
- **Known, from the review of 2026-11-01:** A reading of every
  survivor at the run's commit 50168777 found 151 equivalent, 85
  compiled out on the Arm runner (the stubs of the bulk paths off
  AArch64, which the x86-64 job of `.cargo/mutants-avx2.toml` should
  judge), 116 missing a test and 1 unsure; 22 of the 91 timeouts were
  survivors whose doctests ran past the limit. The gaps that change
  results: the lanes of the int8 IN list that no test hits; the NULL
  bit of a second column appended by partition; the calendar (leap
  years, quarters, the units Day and Week, `negate`, `extract`);
  `Decimal::round` at scales 2 to 4 and the plain numeric sums; the
  flags of the spill's words. One test closes the first for every
  kernel: for each lane i of a word, a word whose only hit is lane i
  gives bit i alone. Exclusions that name lines drift with edits:
  `cast\.rs:7[34]` covers two of the seven lines of the guard it
  means; the choice of the first word as one function, excluded by
  name, would hold.
- **Depends on:** nothing.
- **Capabilities:** none; tests only.
- **Size:** about four pull requests: `relspill`, `tessera-core`
  and `tessera-capi` in one, `tessera-kernels` by its parts.

### relhash-on-crates-io

The crate `relhash` published on crates.io. Left by the close of
`table-crate`, 2026-10-28.

- **What:** Publish `crates/relhash` at version 0.1.0, once the
  maintainer decides it: a public repository for the links of its
  documentation, a tag, and the first `cargo publish`.
- **Why:** The crate was made to stand alone, free of Tessera, so that
  another engine can take its hash table as it is.
- **Known:** CI packs the crate alone (`cargo publish --dry-run`),
  checks its public API against a snapshot, builds its documentation
  without warnings, and refuses a 32-bit target. Its README is the
  crate's page; its links to the design need the repository public.
  Fuzzing of one to two hours goes before each release
  (`.github/workflows/fuzz.yml`, by hand).
- **Depends on:** the maintainer's decision.
- **Capabilities:** relhash
- **Size:** small.

### nightly-checks

Checks that run every night and every week. From the review of
2026-11-01.

- **What:** One workflow on a schedule. Every night:
  `tessera-crosscheck` with the date as its seed and some 20,000
  queries, serial and with workers, at small `work_mem`, its shrunk
  findings kept as artifacts; each fuzz target for twenty minutes, its
  corpus cached; the suites against PostgreSQL's latest master beside
  the pinned commit. Every week: every mutant of the workspace, failing
  on a survivor that `.cargo/mutants.toml` does not list. And the
  random queries in the sanitizer job.
- **Why:** CI runs the random queries with one seed, `--seed 1
  --queries 3000`, so after the first green run they find nothing new.
  No workflow has a `schedule:` trigger; fuzzing and the full mutant
  run start only by hand. Code that no pull request changes is never
  looked at again.
- **Known:** `relhash` keeps its zero survivors by a list that decides
  (2c8ec60c). A shard of the full mutant run took two and a half to
  three and a half hours.
- **Depends on:** nothing.
- **Capabilities:** ci, tools
- **Size:** one or two pull requests.

### capi-miri-and-fuzz

Miri over every test of `tessera-capi`, and a fuzz target of its
arguments. From the review of 2026-11-01.

- **What:** Miri runs every test of `tessera-capi`, not only `columns`
  and `table`, with fewer property cases under Miri. A fuzz target,
  like `relhash`'s `arguments`, calls every entry point with any
  arguments over real buffers the fuzzer sizes: a wrong `struct_size`,
  row counts that disagree between a column and its masks, null
  pointers, codes out of range. Each call returns a code, never a
  panic, and leaves its outputs as its documentation says.
- **Why:** The other entry points hold most of the crate's unsafe code:
  `c/decimal.rs`, `c/spill.rs`, `c/calendar.rs`, `c/int.rs`,
  `c/text.rs`, `c/sort.rs`. Both faults of undefined behavior found at
  this boundary so far were found by Miri (69eb701b, a640abb8).
- **Known:** The spill's part is task 2.5 of `spill-crate`. Random
  pointers are not fuzzed: a false length is the caller's undefined
  behavior, not a fault of a kernel. Under Miri the vector code is cut
  out, so it checks the scalar paths.
- **Depends on:** nothing.
- **Capabilities:** kernel-abi
- **Size:** small for Miri, medium for the fuzz target.

### golden-corpus

The answers of PostgreSQL's functions as the kernels' oracle. From the
review of 2026-11-01.

- **What:** A tool on `tools/tessera-pgtool`, which already starts a
  cluster, runs PostgreSQL's own functions over values drawn at the
  edges and writes lines of function, arguments, and result or
  SQLSTATE into test files of `tessera-kernels`. Rust tests read them
  and run the kernels on the same values. The files are made again
  when `.github/postgres.env` moves. First families: the calendar
  (`date_trunc` of every unit, `extract` of every field, months across
  month ends and leap years, dates BC, infinities), decimals (`round`
  and casts to integers at every scale 0 to 18, sums near 10^18),
  integer arithmetic at the limits of each type, text (`bpchar`
  comparison, `substr` with zero and negative lengths), floats.
- **Why:** Only Rust tests judge a mutant, and the kernels' semantics
  are the core's. Today the oracle is an SQL suite, which judges no
  mutant, or a reference in Rust that repeats the code it checks: the
  calendar's property test of intervals calls the same `days_in_month`
  and `is_leap` it checks. A corpus puts the core in the loop with no
  PostgreSQL in the test.
- **Known:** A reading of the survivors found 54 of the calendar's 64
  missing a test. A file stays small, a few thousand lines a family.
  `timestamptz` needs a zone, so it stays with the SQL suites and
  `crosscheck-draws`. The "Not placed" item of one test over every
  entry of the function registry is the same idea on the C side; it
  kills no Rust mutant.
- **Depends on:** nothing.
- **Capabilities:** batch-functions, tools
- **Size:** medium: the tool, then a pull request a family.

### kani-beyond-relhash

Kani proofs of the kernels' arithmetic. From the review of 2026-11-01.

- **What:** Proofs in the existing Kani job: the prepared divisors of
  int32 and int64 (`quotient` and `remainder` equal `/` and `%` for
  every `n` and every `|d| >= 2`; int64 split by shift);
  `Decimal::round` against rounding half away from zero in i128 at
  every scale 0 to 18; `WordValues` of `tessera-core` (the rows it
  gives are in range, unique and increasing, at most 64 steps, and a
  refusal comes before any read); the encoding of a sort key (order
  and the place of NULL); the split of a timestamp into days and time.
- **Why:** These are a few operations over words, where Kani proves
  for every input what a test samples. The unsafe C adapters rely on
  the promise of `WordValues`.
- **Known:** Kani was decided against in section 9 of the log. The
  proofs of `relhash` have since found an overflow no test had, a
  shift of partitions summed in u32 (3b0813b6). Kani does not follow
  NEON or AVX2 intrinsics: the scalar code is proven, and the
  differential tests on both architectures hold the vector code to it.
  `Divisor::quotient` is `#[cfg(test)]` today and must be visible to
  `cfg(kani)`. Products of 128 bits are slow to prove.
- **Depends on:** nothing.
- **Capabilities:** batch-functions
- **Size:** two or three pull requests.

### zone-rules-in-rust

The rules of local time of a zone in Rust, as fast as the C code. From
the change `int2-and-date-bounds-in-rust`, which tried them and left
them out.

- **What:** The rules of `kernels/date.c` that read a `timestamptz` in
  the session's zone move to `tessera-kernels`: the cache of 32 spans
  of the zone's offset and 1024 local midnights, an instant's local
  time, a day's local midnight, how `date_trunc` turns a truncated
  local time back into an instant, and the casts between `date` and
  `timestamptz`. Property tests over random model zones hold them to a
  reference, as the attempt's tests did.
- **Why:** These rules decide the answers around the changes of summer
  time, and only hand-written SQL suites judge them
  (`crosscheck-draws`); in Rust a model test reaches every rule, and a
  mutant of it meets a Rust test.
- **Known:** The attempt is on the branch `zone-rules-in-rust-rounds`.
  Rust never called PostgreSQL there: a call answered what the cache
  knew and returned the rest as questions, sorted; C asked the time
  zone library and called again with the answers. Against the C code
  (docs/benchmarks/2026-11-02-int2-and-date-bounds) the cases of the
  family `win` ran 1.68 (`tstz_cast`), 1.78 (`tstz_trunc`) and 2.47
  (`tstz_spans`) times as long by the median, and `tstz_spans` longer
  than PostgreSQL without Tessera. The profiles put the time in the
  work around each row, not in PostgreSQL: a row the cache lacks goes
  through Rust twice and through C once, where the C code takes it
  once; a binary search of the answers changed nothing. The way left
  is one pass: C gives Rust a function that answers a question, and
  Rust calls it at a miss. An error of PostgreSQL must not unwind
  through Rust's frames, so the function either runs the library
  under `PG_TRY`, at a cost at each miss, or relies on
  `pg_next_dst_boundary`, `DetermineTimeZoneOffset` and `pg_localtime`
  raising none. The maintainer decides which; the cases `tstz_*` of
  `win` decide whether it is fast enough.
- **Depends on:** nothing.
- **Capabilities:** batch-functions, kernel-abi
- **Size:** one or two pull requests.

### contracts-of-the-c-path

Contracts of `tessera-capi` and `tessera-kernels`, with no memory
taken. From the review of 2026-11-01.

- **What:** A `contract.md` for each crate, as `relhash` has, with rules
  checked in CI: Rust never calls PostgreSQL (no `extern "C" {` import
  and no `extern "C-unwind"` under `crates/`); every entry point runs
  under the guard that catches a panic, but a listed few that only
  give a layout; every slice made from a C pointer is made in one
  module, with a `// trust:` mark; no call over C memory takes memory,
  also when it fails; an error is a closed enum with a static text,
  written into the status without an allocation, and its SQLSTATE
  comes from a `match` the compiler checks.
- **Why:** When Rust's allocator fails the process aborts, and
  PostgreSQL restarts every backend of the cluster; PostgreSQL's own
  allocator raises an ERROR. Today an error is an `anyhow` value made
  with `format!` at the moment memory may be short, and `classify`
  finds its kind by downcasting, so a type it forgets becomes XX000
  without a warning. Slices from C pointers are made in many files:
  `c/spill.rs` 15, `c/table.rs` 5, `c/decimal.rs` 4, besides the
  readers of arguments.
- **Known:** Two documents disagree. The guard says an entry point
  writes its outputs at its end, so nothing is written on failure
  (`c/status.rs`); the kernels say an error does not roll back the
  words already done (`tessera-kernels/src/lib.rs`). Each entry point
  must say which holds. The kernels' root says unsafe code lives in two
  modules, while `loom`, `phases` and `shared_spill` allow it too.
  `relspill` has these rules in its contract already.
- **Depends on:** nothing.
- **Capabilities:** kernel-abi, batch-functions
- **Size:** medium: mechanical over many files.

### bounded-rust-calls

Rust calls bounded in time. From the review of 2026-11-01.

- **What:** List each entry point whose work is not bounded by its
  batch or its chunk, and give it a budget of steps, as
  `tess_table_probe` has, or measure its worst time at the largest
  `work_mem`. Found so far: `tess_sort`, which sorts every item held in
  memory in one call, and `tess_table_regrow`, which rebuilds the
  whole index.
- **Why:** C checks for interrupts between Rust calls, so a cancel
  waits for a long call to end.
- **Known:** The wait has not been measured. Each new machine
  (`join-output-machine`, `grouping-phases-machine`) states the most
  work it does before it returns.
- **Depends on:** nothing.
- **Capabilities:** sort, hash-table
- **Size:** small to medium.

### crosscheck-draws

What the random queries never draw. From the review of 2026-11-01; it
takes the "Not placed" item on subqueries in FROM (plan 9.8, lines
6823–6824).

- **What:** The generator of `tessera-crosscheck` draws, in this order:
  `timestamptz` columns under `SET TimeZone` to a zone with summer
  time, `date_trunc` and `extract` of every unit and field, intervals
  of months and days, `float4`; a correlated subquery whose inner side
  is a Tessera node, so that it rescans; subqueries in FROM; a scroll
  cursor fetched backward. A strict mode for kernels fixes the order of
  evaluation, compares the SQLSTATE, and compares the order of rows
  where it is total. The runner records which node and path ran
  (native or Datum, a spill, workers), and keeps the SQL of a finding,
  not only its seed.
- **Why:** It draws none of these today. The calendar holds the most
  surviving mutants, and the time-zone caches of `kernels/date.c` are
  judged only by hand-written suites. Faults of rescans (f26ce59d) and
  of the direction of a scan (2f3c00f3, 8948fbfc) were found by other
  means.
- **Known:** The broad mode accepts two different errors on purpose;
  `kernel-errors-on-demand` is why.
- **Depends on:** nothing.
- **Capabilities:** tools
- **Size:** two pull requests.

### condition-logic-test

A random test of the logic of conditions. From the review of
2026-11-01.

- **What:** A test of the C test module draws trees of AND, OR, NOT,
  IS [NOT] NULL, IS [NOT] TRUE, FALSE or UNKNOWN, ANY and ALL, four to
  six levels deep, over columns with NULLs at the edges of a word. It
  evaluates each tree with the batch evaluator and with a three-valued
  evaluator a row at a time, compares the masks of true and of
  unknown, and checks that no leaf ran on a row its left side had
  already decided.
- **Why:** The logic of conditions stays in C (see "Decided against"):
  it is lazy, and its leaves may raise. `test/tessera_expr_test.c`
  draws no random trees, and the random queries nest two levels.
- **Depends on:** nothing.
- **Capabilities:** batch-expressions
- **Size:** one pull request.

### join-output-machine

The join's rounds and tails as a Rust machine. From the review of
2026-11-01.

- **What:** The decisions of `TessHashJoin` once a batch is probed, in
  `nodes/hashjoin_pairs.c`: the next record of each row, the LEFT
  join's NULL round, compact batches, SEMI and ANTI over rounds of
  residual clauses, the tails of RIGHT and FULL. They become a state
  machine in Rust that C steps once a round or a batch of a tail. C
  keeps the residual clauses, the child node, the copies of values
  and the interrupts, as actions the machine asks for.
- **Why:** The join's semantics for every join type in one place, which
  a property test compares with a nested-loop join without
  PostgreSQL. Faults of this logic: an ANTI join published rows with
  other rows' inner values, `count(b.v)` 600 instead of 0, found by the
  crosscheck (f8c24e52); in a shared RIGHT or FULL join a participant
  that stopped while it probed left pairs unmarked, and the last one
  returned their inner rows as rows without a pair (501119a7).
- **Known:** About 450 lines decide; the rest copies values and calls
  PostgreSQL. The machine of the join's spill (`spill-crate` 3.13 to
  3.16) should take the rounds in memory as a level with no partitions
  on disk; otherwise the tails are decided in two places. One call a
  round of up to 64 rows; check by the disassembly and the join
  family. It changes the C API of the nodes.
- **Depends on:** `spill-crate` 3.13 to 3.16.
- **Capabilities:** join-hash
- **Size:** medium: three pull requests, the model, the machine and the
  node.

### valgrind-and-c-coverage

Valgrind over PostgreSQL's memory, and the coverage of the C. From the
review of 2026-11-01.

- **What:** A nightly job under Valgrind, PostgreSQL built with
  `USE_VALGRIND`, over the suites of the spill, joins, aggregation,
  sorts and parallel plans; `RANDOMIZE_ALLOCATED_MEMORY` in the assert
  build of CI; and, per release, a run of the suites and the random
  queries on a PostgreSQL configured with `--enable-coverage`, which
  lists each C function nothing reaches.
- **Why:** ASan sees blocks of `malloc`, but a memory context keeps
  freed chunks and reset blocks for reuse, so a read after `pfree` or
  after a reset inside a context is invisible to it. Faults of this
  class were found by wrong answers and crashes: an expanded value
  written as a pointer into memory freed with its batch (a5f5ac6e), a
  key kept on disk as its address (24222429). No job measures which
  lines of C the tests reach.
- **Known:** Valgrind is slow, so only the suites named.
- **Depends on:** nothing.
- **Capabilities:** ci
- **Size:** one pull request each.

### linux-arm64-ci

PostgreSQL on Linux AArch64 in CI. From the review of 2026-11-01.

- **What:** The PostgreSQL jobs, the suites and the sanitizers, also on
  Linux AArch64, beside Linux x86-64 and macOS arm64.
- **Why:** In CI the NEON paths meet PostgreSQL only on macOS. The
  mutants job runs on Linux Arm but loads no PostgreSQL.
- **Known:** GitHub's `ubuntu-24.04-arm` runner, which the mutants job
  uses, serves.
- **Depends on:** nothing.
- **Capabilities:** ci
- **Size:** small.

### grouping-phases-machine

The grouping's phases as a Rust machine. From the review of
2026-11-01.

- **What:** The decisions of the grouping become a Rust machine, as in
  `join-output-machine`: which path a row takes (numbers of the
  dictionary, a probe only for the right side of INTERSECT or EXCEPT,
  a frozen table, the replay of spilled rows), when the table regrows,
  when the groups are drained. The key dictionary and the generic
  aggregates stay in C.
- **Why:** The phases checked against a plain grouping in memory, the
  model `spill-crate` writes for its grouping. Faults: the mask of new
  groups filled whole by each call, so a generic aggregate read a
  float8 as an array (b742ebb7); groups given out twice when an index
  regrew in the middle of a merged chunk (ca8792b8).
- **Known:** Less gain than the join's machine: fewer faults here.
- **Depends on:** `spill-crate` 3.17 and 3.18.
- **Capabilities:** aggregate
- **Size:** medium: two or three pull requests.

### float-aggregate-transitions

The float transitions of the generic aggregates in Rust. Left by the
close of `float-kernels-in-rust`, 2026-11-01.

- **What:** `fast_float_advance` in `nodes/agg_generic.c` computes the
  states of the float aggregates (sums, averages, extremes and the
  Youngs-Cramer variances of float4 and float8) a row at a time, in C,
  with the core's checks. It moves to Rust over the rules of
  `tessera_kernels::float`, a call over a batch of rows and of states
  rather than a call a row.
- **Why:** It is the last float arithmetic in C; only the SQL suites
  judge it, as they judged the float kernels before they moved.
- **Known:** The core's order of additions stays (see "Decided
  against"), and so do its rounding of a float4 sum after each step and
  the checks inside the variance's steps. The state is allocated in C;
  a grouped aggregate needs the states of a batch's groups by
  reference.
- **Depends on:** nothing.
- **Capabilities:** aggregate, batch-functions
- **Size:** medium: two or three pull requests.

## Not placed: the maintainer decides

Remarks of finished plan items that the plan neither closes nor
schedules, and findings of closed changes without a decision. Each
becomes an entry, joins one, or is dropped.

- **The key filter's order under correlated clauses** (the close of
  `early-key-filter`, a finding of its review). A TessFilter of batch
  clauses alone compares the filter's share of all its rows with the
  clauses' share of the rows the filter kept; where both keep the same
  rows it may hold the costlier order for the whole scan, by under half
  a check a row. Suggested: a window of each side measured first now
  and then, once a query shows the cost.
- **Two costs of a case over a ring on the M5 Pro** (the close of the
  change of the benchmarks' inputs, 2026-11-03). Over a ring of inputs
  a process of the M5 Pro runs an operation at one of two costs about
  10 % apart, or steps from one to the other within the run, Datum
  operations most. A ring of 4 inputs inside L1 spreads its processes
  as a ring of 64 does; one input did not, in one run. The minimum of
  identical sides moved by 7.2 % in `arith_int32`, the median with it,
  and by 21 % on one block of `compare_int32`. Until the cause is
  known, a `FAIL cycles` on a case drawn at random counts there when a
  second run confirms it. Suggested: record the time of a block beside
  its cycles, to read its clock; then rings of 1 and 2 inputs over
  several runs, to see whether taking the inputs in turn brings the
  second cost.
- **TessHeapScan under a row-wise hash join** (the TPC-H analysis of
  2026-11-02). In Q22 the scan of orders feeding the core's hash join
  takes 56 ms against 36 for the core's Seq Scan, where
  `scan_planner.c` says the native scan is faster under any parent.
  Suggested: a slot into the pinned page, as
  `minimal-tuples-for-storing-parents` weighs; `right-anti-join`
  removed the case of Q22.
- **A build in every participant** (the same analysis). Q14 with two
  workers builds `part` in each participant, 200 000 rows three times,
  where the core's Parallel Hash shares the build. Suggested: read the
  rule that chooses the shared table.
- **TessAgg over a join's output** (the same analysis). In Q14 TessAgg
  takes 24.7 ms over 76 000 rows where the core's Aggregate takes 7.8.
  Suggested: a profile of Q14 after `deform-without-restarts`.
- **The weak queries on Greengage** (the same analysis). Q5, Q8, Q9,
  Q11, Q16, Q19 and Q22 gain least there, and only totals were taken.
  Suggested: EXPLAIN ANALYZE of them on Greengage, Tessera on and off.
- **C regions given to Rust as initialized bytes** (the review of
  `spill-crate` task 3.6, 2026-11-02). An entry point that takes a
  region C allocated makes a `&mut [u8]` over it, which Rust allows only
  over initialized bytes. The set's writer gets its buffer, scratch and
  lists zeroed; `tess_spill_columns_init` still gets the sort's chunks
  as `palloc` leaves them (`nodes/sort_external.c`), as the packing's
  entry points did. No compiler is known to miscompile it. Suggested:
  zero such regions where they are made, or take them as
  `MaybeUninit` in `tessera-capi`.
- **The cost of a hashed join key** (the close of `native-key-hashes`,
  2026-11-02). `tessera.join_hashed_key_cost` was measured when every
  key a word does not hold went through its type's hash function. A
  text, char(n) or bytea key now is hashed by its bytes without the
  call, which was about half of such a key's hashing in a profile, so
  the model overstates those keys. Suggested: measure the joins of `anykey` on
  mains power and set the cost per kind of key, if they differ.
- **The cost of a partial grouping that folds little** (the close of
  `partial-dictionary-keys`, 2026-11-01). The cost of `TessAgg`'s
  partial stage counts a dictionary's lookups but not its memory per
  group, so the point where the planner expects the groups to pass
  `hash_mem` comes late for keys through a dictionary. And nothing
  counts a partial table that goes up whole each time it fills, as
  tables of sum states and of such keys do: past that point the final
  stage meets almost every row. Suggested: a case of many groups under
  a `Gather` in a family first, then a cost by the share the partial
  stage folds.
- **`Memory Usage` leaves the dictionaries out** (the same close).
  `EXPLAIN` shows the memory of `TessAgg`'s table and `DISTINCT` sets,
  not of the keys' dictionaries, which count when the node decides the
  table is full. Suggested: a small fix with a test.
- **Aggregate transitions through the core's functions** (profiles of
  2026-11-02, before `native-key-hashes`). `bit_or`, `bit_and`,
  `bool_and` and the text extremes call their transition function for
  every row. `max(text)` over 500 000 rows (`g_max_text`) spends 2345
  of 4400 samples in `text_larger`, most of it the locale's `strcoll`;
  the call itself, the collation's lookup and the unpacking are about
  7 %. A native form helps the bit and boolean aggregates and the
  extremes under the C collation only. Suggested: measure `g_bit_or`
  first.
- **Comparisons computed as values** (an outside review, 2026-11-02).
  A comparison that a WHERE clause runs in batches runs row by row in a
  target list (`SELECT a > b`): it is registered as a condition, not as
  a value, so the projection evaluates it through the core. UNKNOWN
  must become NULL. No case measures it. Suggested: a case in a family
  first.
- **The integer entry points borrow a whole column struct** (the close
  of `float-kernels-in-rust`, 2026-11-01). `DatumColumn::ints`, behind
  every int4 and int8 entry point of `tessera-capi`, makes a reference
  to the whole `TessDatumColumn` before it checks `struct_size`. A
  caller that allocates only the required fields
  (`TESS_DATUM_COLUMN_MIN_SIZE`) gets undefined behavior, as the reader
  of the calendar and float entry points did until that change read
  the fields one by one. Suggested: the same, with a Miri test, in
  `capi-miri-and-fuzz` or alone.
- **A grouping inside a subquery** (plan 5.11 and 5.13, lines 4328–4330,
  4913–4914): no partial (parallel) stack for it, and GROUP BY without
  aggregates in a subquery stays the core's. Suggested: check the
  coverage suite first.
- **One table-driven test over every entry of the function registry**
  (plan 4.20, lines 938–939). Suggested: a small entry, or drop.
- **The costs of join pruning** (plan 5.14, lines 5139–5142): a worker's
  start-up once per Append, star-schema conditions, the Bloom filter's
  cost through TessAppend, hash partitioning. Suggested: an entry when a
  measured case appears.
- **The index tuple parsed through a slot** (plan 6.4а step 4, line
  5420): about 14 % of that profile. Suggested: an entry if
  `tpch-indexed-schema` shows it.
- **The first VACUUM after COPY on master** (plan 8.1, lines 6334–6337)
  freezes rows but marks no page all-visible: possibly a bug of the
  core, not analysed. Suggested: report upstream, or drop.
- **`pmu`: findings without a decision** (the close of
  `counters-on-x86-and-linux`, 2026-10-25).
  - XNU's `cpc_task_set_owner` (xnu-12377.121.6) finds that another
    task holds the counters, sets `EACCES` and returns 0, so kpc never
    refuses a second process. Suggested: report it to Apple, or drop.
  - OpenSpec 1.14.3 fails `check-specs.sh` on every spec, on its
    warning that a requirement runs past 500 characters; CI pins
    1.13.2. Suggested: shorten the requirements before CI moves on, or
    stay on 1.13.2.
  - The mutants workflow takes the diff of `crates/` and leaves `tools/`
    out, so no mutant of `pmu` runs. Its hardware tests would skip on
    the workflow's Arm runner anyway, at `kernel.perf_event_paranoid`
    4. Suggested: mutants of `core_pmu.rs` and of the decoding of a
    read, which need no PMU, or drop.

- **The append of rows by partition reads each row's chunk again** (the
  change `spill-crate`, the counters of task 2.6).
  `append_partitioned` reads the header of a row's chunk, checks its
  magic and its width, for every row: at 4 words a row takes 249
  instructions against 170 of a plain loop of the same lanes, and some
  140 of them do not depend on the words. Suggested: keep the shape of
  each partition's chunk for a batch, or drop.

- **A grouping's split notes no moment of its copies** (the close of
  `table-growth-one-owner`, 2026-11-01). When a grouping starts to
  spill, it copies each chunk of its table into the partitions' chunks
  and frees the original without a note of memory; a split of a
  partition read back resets its source and flushes the chunks of the
  level below before it notes. The copies live beside the originals and
  the index, so Memory Usage misses that moment: an underestimated
  `count(*)` by one key at a `hash_mem` of 256 kB holds three chunks of
  64 kB when it spills. Suggested: a note before each free, which needs
  the old chunks counted outside the spill's contexts, in
  `grouping-phases-machine` or alone.
- **A grouping that spills passes `hash_mem` far** (the same close).
  The bench case `regrow_128kb`, 300 000 groups at a `hash_mem` of
  256 kB, shows a Memory Usage of 2403 kB with 8 partitions, 129
  regrows and 71 partitions split. Not analysed: this case spills
  records, and Memory Usage of such a spill counts its memory contexts,
  the allocator's share and its files' buffers included. Suggested:
  find where the bytes go, then decide.
- **A serial build near its limit spills for the room of two chunks**
  (the same close). Before each batch the serial join counts its chunks
  and values, room for two more chunks and its index; a build whose
  table and index fit but leave less than that room spills. The bench
  case `mixed_text_16mb`, 500 000 rows with a text column at a
  `hash_mem` of 32 MiB, fitted in 31881 kB and now spills, 1.33 times
  as slow by the median; `build_300k`, 300 000 keys at 16 MiB, 1.64.
  Suggested: count what the next batch can allocate in place of the
  room, with task 3.23 of `spill-crate`, or accept.
- **The planner's gate of DISTINCT leaves out the set's index** (the
  same close). A grouping with a DISTINCT aggregate never spills; the
  planner takes it when its estimate of the pairs of group and value
  fits `hash_mem`. The estimate counts the pairs' records and leaves
  out each set's index and the room left in its last chunk, so a set
  may pass the estimate by its index. Suggested: count the index in the
  gate, with a case near the limit, or drop.

- **Whole-word division leaves NULL slots unwritten** (an outside review
  of `int2-and-date-bounds-in-rust`, 2026-11-02). The int4 and
  smallint entry points promise a written placeholder in every
  selected row, NULL ones too, and allow an output the caller has not
  initialized (`include/tessera/kernels.h`). Division and remainder of
  a whole word without a prepared divisor write only the present lanes;
  with one they write nothing when the whole word is NULL. 64 rows of
  12 with row 3 NULL, divided by 1, leave slot 3 as it was. Tessera's
  own caller zeroes its output (`palloc0_array` in `runtime/expr.c`),
  so no query reads such a slot today. Suggested: write the
  placeholders on these paths, with a test of NULLs in a whole word
  and of a word all NULL.
- **The smallint multiply checks the int32 overflow too** (the counts
  of the same change). The vector smallint multiply checks that the
  int32 product did not overflow and that it fits a smallint; the
  product of two smallints cannot overflow an int32, so the first
  check is spent. In the program `arith_int32` a row of a dense column
  times a constant takes 9.8 instructions as smallint against 5.0 as
  int4. The case `int2_arith` of `win` shows no loss against the C
  code. Suggested: drop the check where both operands are smallints,
  which the entry points promise, or leave it.

## Decided against

Not future work: each was weighed and refused, or left to the core, with
the reason at the plan lines given. Proposing one again needs a new
fact.

- A guard of Tessera's join cost against the core's underestimate of a
  join on several keys (TPC-H Q9: lineitem with partsupp estimated at
  2392 rows, 6 M actual, then a nested loop of 6 M index probes). The
  estimate is the core's, and with workers the core alone takes the
  same plan: left to the core's planner (the maintainer, 2026-11-02).
- Batches longer than 64 rows; parallel RIGHT and FULL joins without the
  shared table (4.27, line 2127).
- An own radix sort; an external crate only when clearly better and
  widely used (5.7, lines 3909–3913).
- Compression of spill files (5.6, lines 3746–3750); a skew table for
  the join's spill (4.27, lines 2162–2163).
- Sorted DISTINCT and DISTINCT ON (5.9, line 4051); MergeAppend with
  Unique for UNION (5.11, lines 4169–4170).
- TID scans, and skipping the recheck on exact pages (6.4а, lines 5306,
  5319–5320); a Gather with one copy (6.4в, line 5981).
- A hash join payload wider than 64 inner columns (4.27, lines
  2105–2106; line 6001).
- GROUPING SETS, regular expressions, ILIKE, upper and lower outside
  ASCII, md5, JSON (4.21, lines 982, 1012–1013).
- Float sums in another order of addition than the core's (4.21, lines
  1306–1308).
- A general rule that narrows a constant against a widened column (4.20,
  lines 892–893); a NEON kernel for blending by a mask (4.20, line 927).
- Row layouts and condition logic moved to Rust (4.23, lines 1522–1523);
  the key dictionary moved to Rust unless the crosscheck finds errors
  there (9.17, lines 7135–7137).
- A filter in the hash table's bucket: done and rolled back, the Bloom
  filter took its place (5.1, lines 2967–2980).
- The heap deform cursor rewritten in Rust or vectorized (6.1, line 5187).
- The refresh functions and the throughput part of TPC-H (8.1, lines
  6242, 6325); tuning constants by TPC-H (8.10, lines 6581–6582).
- cbindgen, Rust under ASan, a mutation tool for C, the parallel
  build's automata in Rust, overflow checks in the release profile,
  `-Werror` by default (section 9, lines 7367–7376). Kani, refused
  there too, is back: see `kani-beyond-relhash`.
- An Instruments template to count a backend's instructions (4.16, line 602).
- A late worker of a shared build that spilled joining the rounds,
  whether by going round the partitions on disk after the build's last
  phase or by keeping every participant on the build's barrier as the
  core does: a rare path no test reaches, or a move of the RIGHT and
  FULL joins' tail, for help a worker of a parallel append mostly gives
  to the next child anyway (pull request 52).

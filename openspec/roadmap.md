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

In this order. The maintainer placed `counters-on-x86-and-linux`,
`simd-primitives-avx2`, `linux-x86-support` and `oltp-guard-bench` on
2026-10-05, in the review of pull request 45, and
`explain-rows-per-loop` first on 2026-10-08, after pull requests 49 and
50; the other entries keep the order of the decisions of 2026-09-30 to
2026-10-02 (plan lines 2810–2836, 6596–6603, 7378–7390):

1. `explain-rows-per-loop`: The rows a node removed, per loop as the
   core shows them
2. `counters-on-x86-and-linux`: Performance counters on x86 and on Linux
3. `simd-primitives-avx2`: SIMD primitives layer and AVX2 for x86-64
4. `linux-x86-support`: Linux on x86-64 checked as the target platform
5. `oltp-guard-bench`: OLTP guard family of benchmarks
6. `runs-left-from-section-9`: Runs left to the maintainer from section 9
7. `tpch-short-set`: TPC-H step 7: the short query set for A/B
8. `tpch-parallel-and-jit`: TPC-H step 8: parallel series and the
   `jit = on` control run
9. `tpch-indexed-schema`: TPC-H step 9: schema with indexes on foreign
   keys and dates
10. `tpch-sf10`: TPC-H step 10: SF10
11. `backward-scan-mark-restore`: Backward scan and mark/restore
12. `postgresql-19`: PostgreSQL 19 support
13. `pg-duckdb-comparison`: Comparison with pg_duckdb

## By measurement

Taken when a measurement of the TPC-H runs shows the loss (plan lines
2810–2814):

- `spill-sum-states`: Spill of sum states instead of rows
- `numeric-reading`: Faster reading of numeric

## Later

After the queue, by decisions of 2026-09-20 to 2026-09-22 (plan lines
210, 2788–2799), which are older than the queue and put it after AVX2:

- `prosupport-batch-functions`: Extension batch functions through prosupport
- `table-key-types`: Keys of more types in the hash table itself; the
  maintainer moved it here from the head of the queue on 2026-10-07

## Not ordered

These have no place in the queue. The maintainer places an entry when it
is wanted; several wait for a measured case. `greengage-port` and
`tpch-on-greengage` came down here from "Later" on 2026-10-05.

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
- `hash-table-participants`: How participants agree over a shared hash
  table

## Entries

### explain-rows-per-loop

The rows a node removed, per loop as the core shows them. Found when
the key filter was described; the maintainer decided on 2026-10-08 to
follow the core.

- **What:** Every line "Rows Removed by …" of a Tessera node shows the
  rows per loop, as the core shows its own: the total over every loop
  and every participant, divided by the node's loops, which in a
  parallel plan add up over the participants. TessFilter does so
  already. TessHashJoin shows three such lines as totals, "Rows Removed
  by Join Filter", "Rows Removed by Filter" and "Rows Removed by Bloom
  Filter"; they become per loop. The other counters of Tessera stay as
  the core keeps its own of their kind: the work done (batches, rows
  read, chunks, partitions) as totals, the memory as a peak. The spec of
  `key-filter`, its design, `docs/nodes.md` and the expected outputs
  follow.
- **Why:** A line named as the core's must mean what the core's means.
  The core's hash join shows "Rows Removed by Join Filter" per loop, and
  TessHashJoin in its place shows a total. The join suite has a join
  under a parameter that rebuilds its table ten times: it shows
  `rows=159.50` a loop beside "Rows Removed by Bloom Filter: 154713",
  the sum of the ten loops. By the code, in a parallel plan of three
  processes the TessFilter below a join shows a third of the join's
  line; no test shows it yet.
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
- **Capabilities:** key-filter
- **Size:** small: one pull request.

### counters-on-x86-and-linux

Performance counters on x86 and on Linux. From the review of pull
request 44, 2026-10-04; the plan's accepted limits had refused Linux
(line 7485).

- **What:** Count instructions and cycles of the kernels on x86 and on
  Linux as `tessera-bench` does on macOS with Apple silicon, through a
  backend of `tools/tessera-pmu` for each.
- **Why:** The counters come from kperf and work only on macOS with
  Apple silicon, while the target platform is Linux on x86-64
  (`linux-x86-support`). AVX2 kernels cannot be accepted by counters
  without x86 (`simd-primitives-avx2`, next in the queue), and what is
  left of the page-fault item asks for a check on Linux
  (`parallel-worker-page-faults`).
- **Known:** The maintainer's decision 2026-10-05: in the queue before
  `simd-primitives-avx2`, so that the tool to measure with is there
  while the kernels are written. The decision of 2026-10-04: x86 and
  Linux are planned, and a run will go differently there than on macOS,
  where kperf needs root. On an Intel Mac `tools/tessera-pmu` needs a
  table of Intel events and a fresh calibration (plan 3.9). The log
  gives no design for Linux. CI checks correctness, not speed.
- **Depends on:** nothing; `simd-primitives-avx2` waits for the x86 part.
- **Capabilities:** tools, bench
- **Size:** the log does not say.

### simd-primitives-avx2

SIMD primitives layer and AVX2 for x86-64. From plan 3.9, lines 210–278;
4.20, lines 928–941.

- **What:** First a pilot: write the int32 filter on NEON over a small
  set of primitives (a trait over a lane class, about 12 primitives of
  1–5 intrinsics) and accept it only if the disassembly of the bulk loop
  is unchanged. If the pilot holds, move the NEON kernels one by one
  (filter → arith → aggregate → cast → hash) and add AVX2 as an
  implementation of the primitives; if not, write AVX2 as a second
  parallel implementation in `simd/avx2/`. On x86-64 the block path is
  chosen at run time by `is_x86_feature_detected!("avx2")`, with no
  `-C target-cpu` flags.
- **Why:** Linux on x86-64 is the target platform (the maintainer,
  2026-10-05), and vector kernels exist only for AArch64; on other
  processors the scalar kernels run. AVX2 is on every Intel Mac since
  2013 and all server x86 of the last ten years (line 211–213).
- **Known:** The maintainer's decision 2026-10-05: taken after
  `counters-on-x86-and-linux`. Decision
  2026-09-20; revised 2026-09-25 (primitives, on the maintainer's
  condition that the trait must not cost performance); clarified
  2026-09-30 (the control loops of both widths are already shared
  through `IntLane`, the pilot concerns only the vector code) and
  2026-10-01 (on Intel re-measure the decisions tuned by time on the M5
  Pro: `BULK_MIN_ROWS = 12`, the bounds of `SetValue::WORD_KEYS`, the
  counted full-word loop, the branchless select in `read_masked`). Tests
  run under Rosetta 2; counters under Rosetta are useless, so acceptance
  by counters happens only on an Intel Mac, where `tools/tessera-pmu`
  needs a table of Intel events and a fresh calibration. A test may not
  skip silently: with `TESSERA_REQUIRE_SIMD=1` a missing feature is an
  error. The acceptance rule of 3.5 is unchanged: a vector path stays
  only with a reproducible gain in cycles. The 21 stub bodies of the
  non-NEON build get AVX2 bodies here (line 6889–6891). Commits per
  kernel: filter → arith (with the divisor) → aggregate → hash → table
  prefetch. Checked in the code: no `avx2` anywhere, `simd/` is not
  split into `neon/` and `avx2/`.
- **Depends on:** `counters-on-x86-and-linux`, the tool to measure with
  while the kernels are written; it goes first. The lane helpers both
  widths share, the packing of lane masks into row bits among them, are
  in `simd/mod.rs`. The plan named an Intel Mac for the acceptance (line
  2789).
- **Capabilities:** kernel-abi, tools, ci
- **Size:** large — a series with its own acceptance per kernel.

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
  2026-10-05). All of the project's timings were taken on that machine,
  and on x86-64 the scalar kernels run until `simd-primitives-avx2`.
- **Known:** The maintainer's words: Linux is covered by CI in some
  form, and it needs more than that. Checked in the code: on
  `ubuntu-24.04`, which is x86-64, CI runs the Rust checks and tests,
  Miri, the loom model, the sanitizers, the suites against PostgreSQL
  master with the release and the debug kernels, and the random queries
  against the core; it checks correctness, not speed. Every run in
  `docs/benchmarks` is from an Apple M5 Pro, and the defaults of the
  planner's cost parameters are the values measured there
  (`docs/costs.md`, "The reference machine"), as are the thresholds of
  the kernels that `simd-primitives-avx2` lists. The tools ask macOS
  only for the description of the machine and of its power (`sysctl`,
  `pmset`) and choose the suffix of a module by platform. Counters on
  Linux are `counters-on-x86-and-linux`; huge pages are
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
  `simd-primitives-avx2`. Checked in the code: `bench/pg` has no such
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
- **Depends on:** nothing; ordered after `simd-primitives-avx2` by the
  old decisions.
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
  `hash-table`, and of the join and the grouping.
- **Capabilities:** hash-table, join-hash, aggregate, type-support
- **Size:** several pull requests: the word kinds first, then keys of
  16 bytes, then strings.

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
  path is mandatory (line 80–82).
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
  7485); since 2026-10-04 they are (`counters-on-x86-and-linux`).
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

- **What:** AVX-512 (VL/BW, masks in registers) as a separate third
  group of vector kernels.
- **Why:** The log gives no gain; it only reserves the place.
- **Known:** The maintainer's decision 2026-10-05: only if AVX2 proves
  not good enough. Before it: "later, and only for a particular machine
  where it can be measured". AVX-512 exists on part of the Xeons and on
  Zen 4+, and lowers the frequency on old Xeons (line 212–213).
- **Depends on:** `simd-primitives-avx2` and what it shows.
- **Capabilities:** kernel-abi
- **Size:** the log does not say.

### small-c-leftovers

Small C leftovers of section 9. From plan 9.12, 9.17 and the survey;
lines 6936–6937, 6943, 6661–6662, 7220–7221, 1535–1536.

- **What:** Four small things: the row spill of aggregation
  (`rows_write`) still lays rows out by partitions in C and is to move
  to `tess_spill_columns_append` "later, as decided"; `RowWriter` in
  `agg_spill.c` duplicates `RunWriter` of the external sort; the three
  copies of `store_value` are to be merged in C; `nodes/internal.h` is
  still an umbrella that every `.c` includes, kept "for one release";
  the comment of `join_many` in the join suite says memory goes over
  `hash_mem` while the output stays within it.
- **Why:** One place per pattern; the survey counted these among the
  duplicates (lines 6661–6662).
- **Known:** Decision 2026-09-29: `store_value` is merged in C, not
  moved to Rust (lines 1535–1536, 6661, 7372). Checked in the code:
  `rows_write` and `RowWriter` are in `agg_spill.c`, the external sort
  and TessGather already write through `spill_columns_append`; three
  stores exist (`join_store_value`, `side_store`, `store_value` in
  `runtime/rows.c`); 21 files of `nodes/` include `internal.h`; the
  comment still says "the memory goes over it".
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

### hash-table-participants

How participants agree over a shared hash table. Left by the changes
that wrote the capability `hash-table`: the first took the table itself,
the second partitions, the Bloom filter and the marks of RIGHT and FULL
joins.

- **What:** Describe in `hash-table` what `docs/table.md` still holds of
  the table: the phases of a shared build and of the rounds over
  partitions on disk (`tess_build_*`, `tess_round_step`) and the shared
  words of a spill (`tess_table_spill_*`). The aggregate states in a
  payload (`tess_table_accumulate*`) go to the capability of the
  grouping, and the items of a sort (`tessera/sort.h`) to the sort's.
- **Why:** These calls are the C API of the table too, and until they
  have a spec nothing ties their promises to tests.
- **Known:** The loom model covers the phases, the split and the
  rounds. `docs/table.md` keeps their text with a pointer to the
  capability. The stop word of a RIGHT or FULL join's participant that
  leaves while it probes is in the capability already, with the marks.
- **Depends on:** nothing.
- **Capabilities:** hash-table, aggregate, sort
- **Size:** one pull request for the table's part, under ten
  requirements.

## Not placed: the maintainer decides

Remarks of finished plan items that the plan neither closes nor
schedules, and findings of closed changes without a decision. Each
becomes an entry, joins one, or is dropped.

- **A grouping inside a subquery** (plan 5.11 and 5.13, lines 4328–4330,
  4913–4914): no partial (parallel) stack for it, and GROUP BY without
  aggregates in a subquery stays the core's. Suggested: check the
  coverage suite first.
- **One table-driven test over every entry of the function registry**
  (plan 4.20, lines 938–939). Suggested: a small entry, or drop.
- **Subqueries in FROM in the crosscheck generator** (plan 9.8, lines
  6823–6824): it makes none. Suggested: a small `tools` entry, or drop.
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
- **`spill-format`: the code against its own intent** (found when the
  capability was described and reviewed, pull request 46).
  - The lists of blocks are memory that nobody counts: 16 bytes a block
    in arrays that start at 16 places and double, and a copy for every
    reader of another participant's file. `tess_spill_memory` leaves
    them out. Counting them is not a correction of a check: only the
    serial join asks a set for its memory, and a list of 16 places for
    each partition that has a block is 256 bytes, about 512 kB at 1024
    partitions on two sides, more than the whole `hash_mem` of a small
    `work_mem`. It would change when the join sends partitions to disk,
    so it needs a change of its own with a measurement
    (`docs/measuring.md`). It was tried in pull request 46 and taken
    back: with the lists counted, a join of the suite `join` that
    spills at a small `work_mem` showed "Memory Usage: over hash_mem"
    where it had been within it, and the counts of chunks and of bytes
    on disk moved in five plans. The join's rule has no room for the
    lists: counting them needs a reserve in that rule first.
  - Every node passes `MaxAllocHugeSize` as the longest body of its
    sets, so the bound on the size of a block holds nothing for them: a
    damaged entry of a shared file's list can ask a reader for a buffer
    as large as the file's blocks. The join, the grouping and the sort
    know their longest chunks and could say them.
  - A reader that opens a participant's file before the participant's
    first write to disk gets "no blocks" without an error. The format
    cannot tell this from a participant without blocks or from a worker
    that never started, so a mark of a finished file would not cure it:
    the guard is the nodes' barriers, and the spec states it as what the
    caller ensures. The suite shows what an open of a file that is on
    disk but not finished gives: damaged data.
- **`spill-format`: what no test shows yet** (the same source).
  - The one form of a reference to a value (pull request 46) adds an
    operation for each by-reference value appended to a chunk of
    columns, and a check for each one a sort or a grouping reads back.
    Its cost is below what the A/B tool flags, and its cause is not
    found. The cases `runs_text` and `rows_text` of the exec family (a
    sort whose runs carry text, a grouping by a text key, both at a
    work_mem of 4 MB) came out 1.0 to 2.6 % slower beyond the control
    in a run of 51 repetitions and two rounds a side. The tool flags a
    case past 2 % by both its least time and its median; neither case
    reached that by its median. The run was on battery power with one
    core busy by a system service, against `docs/measuring.md`: a run
    on mains power is owed. The disk written is the same. The machine
    code differs by the check, about five instructions a value, and by
    the sort's function that takes a row from a run, no longer inlined,
    about ten instructions a row. A variant that inlined it again and
    reported a damaged reference once a row took the same time as this
    code (0.4 to 1.7 % beyond the control in two runs, one of them with
    functions aligned to 64 bytes), so it was not kept. No counter
    benchmark of `tessera-bench` runs the kernels of a spill, so their
    instructions are not counted.
  - A join checks the chunk a reference names and not the byte in it,
    and no node checks that a value's own length ends inside its chunk.
  - The order of blocks in a partition (values before the blocks that
    refer to them; pairs of values and columns in a sort and in a
    grouping's rows) is in the design only: a rule of the join, of the
    grouping and of the sort, for their capabilities.
  - `docs/spill.md` and the comments of the reserves in
    `nodes/hashjoin.h` and `nodes/agg_spill.c` still count a buffer of a
    page for each partition's file, though a set has one buffer. Whether
    the reserves themselves are still right is to be decided.

## Decided against

Not future work: each was weighed and refused, or left to the core, with
the reason at the plan lines given. Proposing one again needs a new
fact.

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
- Kani, cbindgen, Rust under ASan, a mutation tool for C, the parallel
  build's automata in Rust, overflow checks in the release profile,
  `-Werror` by default (section 9, lines 7367–7376).
- An Instruments template to count a backend's instructions (4.16, line 602).

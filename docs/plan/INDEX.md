# Index of the working plan

One entry for each item of [plan-migration.txt](plan-migration.txt),
which is frozen. Lines are those of the file as frozen. The status is
what the log itself records: `done` (the item has a «Сделано» paragraph
or its equivalent), `done (unmarked)` (early items that predate that
marker), `done, leftovers` (done, but the item records a part as not
done), `open`, `deferred`, `moved` (absorbed by another item) and
`unclear`. "Explains" names at most three parts of the system that the
item mainly explains, by the names of the capabilities under
`openspec/specs/`; items about process, CI, tools or benchmarks say
`process`, `ci`, `tools` or `bench`. A line number in an entry's last
sentences points at the paragraph that records a leftover, a move or a
decision; a leftover stays in the status even when a later item took it,
and the entry then names that item.

## 1. Repository foundation (lines 89–94)

- **1.1** Git repository with PostgreSQL licence, README, development
  rules. Status: done (unmarked); line 91. Explains: process.
- **1.2** Architecture rules: C/Rust boundary, memory ownership, review
  order. Status: done (unmarked); line 92. Explains: process.
- **1.3** No remote and no copied pg_batch history. Status: done
  (unmarked); line 93. Explains: process.

## 2. The Tessera bridge (lines 95–118)

- **2.1** Extensible ABI rules: abi_version, struct_size, append-only
  fields. Status: done (unmarked); line 99. Explains: bridge-api.
- **2.2** TessRowMask and operations on the row bit mask. Status: done
  (unmarked); line 100. Explains: batch-format.
- **2.3** Format-independent TessBatch with lazy Datum columns. Status:
  done (unmarked); lines 101–102. Explains: batch-format.
- **2.4** TessLayout and TessRequest with filter and projection masks.
  Status: done (unmarked); line 103. Explains: batch-format.
- **2.5** Request binding to TupleTableSlot: attach, find, seal, detach.
  Status: done (unmarked); line 104. Explains: slot-binding.
- **2.6** Batch life cycle: publish, get, consume, release. Status: done
  (unmarked); lines 105–106. Explains: slot-binding.
- **2.7** Source registry with stable names. Status: done (unmarked);
  line 107. Explains: bridge-api.
- **2.8** Source planning contract and classification of quals. Status:
  open; lines 108–109. Explains: bridge-api, planner-coverage. Put off
  to go with 6.5.
- **2.9** Registry of batch nodes. Status: done (unmarked); line 110.
  Explains: bridge-api.
- **2.10** Two independently built test modules: provider and consumer.
  Status: done (unmarked); line 111. Explains: bridge-api, slot-binding.
- **2.11** docs/bridge.md with a full example and pointer lifetimes.
  Status: done (unmarked); line 112. Explains: bridge-api, slot-binding.

## 3. Basic parts in Rust (lines 119–335)

- **3.1** Cargo workspace with tessera-core and tessera-capi. Status:
  done (unmarked); line 121. Explains: kernel-abi. Its build settings
  for panics are described at 299–302.
- **3.2** Safe Rust views of TessRowMask, validity bitmap, borrowed
  columns. Status: done (unmarked); line 122. Explains: batch-format,
  kernel-abi.
- **3.3** Dense int32 and Datum column views; ColumnReader trait.
  Status: done (unmarked); lines 123–129. Explains: batch-format,
  kernel-abi.
- **3.4** int32 comparison filter kernel. Status: done (unmarked); line
  130. Explains: batch-functions.
- **3.5** PMU-counter benchmarks (tessera-pmu, tessera-bench) and the
  NEON filter. Status: done, leftovers; lines 131–162. Explains: bench,
  batch-functions. 162: the pg_batch C filter was not measured through
  FFI.
- **3.6** int32 arithmetic kernels that return error codes. Status:
  done; lines 163–185. Explains: batch-functions.
- **3.7** count, sum, min, max kernels over int4. Status: done; lines
  186–195. Explains: batch-functions.
- **3.8** Hash kernels for one and several keys. Status: done; lines
  196–209. Explains: batch-functions, hash-table.
- **3.9** AVX2 SIMD for x86-64. Status: open; lines 210–278. Explains:
  batch-functions. Revised at 258 (one set of lane primitives), 271,
  274.
- **3.10** int8 kernels (int64 module) and a type-independent count.
  Status: done, leftovers; lines 304–335. Explains: batch-functions,
  aggregate. 334: sum(int8), int8 hash (came with 5.2), arithmetic with
  int4 on the left.

Other paragraphs:

- 280–302: Standing rules for Rust: unsafe only in tessera-capi and
  tessera-pmu, no PostgreSQL calls from Rust, panics and errors.

## 4. Runtime and the first chain of nodes (lines 336–2840)

Item 4.29 has no header: it is a candidate described at the end of 4.28.

- **4.1** Obligations of every batch node (docs/node.md). Status: done;
  lines 338–348. Explains: node-contract.
- **4.2** Rust-to-C boundary in tessera-capi: entry points, TessStatus,
  panics. Status: done; lines 349–363. Explains: kernel-abi.
- **4.3** Batch function registry: kernel choice by function OID.
  Status: done; lines 364–376. Explains: batch-functions, bridge-api.
- **4.4** TessBuilder: own batch from row cells; the runtime library.
  Status: done; lines 377–385. Explains: runtime-helpers.
- **4.5** TessOutput: batch publication and the row-by-row fallback.
  Status: done; lines 386–396. Explains: runtime-helpers, slot-binding.
- **4.6** TessInput: request, fetch, finish and rescan of a child.
  Status: done; lines 397–406. Explains: runtime-helpers, slot-binding.
- **4.7** Named plan data codec, planner helpers, tessera.enable.
  Status: done; lines 407–432. Explains: node-contract, runtime-helpers.
- **4.8** TessPack: batches from any row child of the core. Status:
  done; lines 433–453. Explains: pack.
- **4.9** TessUnary helper for nodes with one child. Status: done; lines
  454–472. Explains: runtime-helpers.
- **4.10** TessLimit example node, set_tuple_bound,
  docs/writing-a-node.md. Status: done; lines 473–491. Explains: limit,
  node-contract. Also starts bench/pg (489).
- **4.11** Restricted int4 expression compiler: a linear chain of steps.
  Status: done; lines 492–511. Explains: batch-expressions.
- **4.12** TessFilter: batch prefix of quals plus a row residual.
  Status: done, leftovers; lines 512–532. Explains: filter,
  planner-coverage. 531: partial paths (came with 6.4), index and bitmap
  scans under the filter (6.4а), row estimates.
- **4.13** Lazy projection of expressions: TessProjection, PROJECTED
  layout. Status: done, leftovers; lines 533–551. Explains:
  runtime-helpers, batch-expressions. 550: cost of expressions in the
  path, whole-row Var.
- **4.14** TessAgg without GROUP BY: count, sum, min, max. Status: done;
  lines 552–577. Explains: aggregate, batch-functions. 575 lists what
  was left out; partial aggregation, GROUP BY and more types came with
  6.4, 5.3 and 4.21.
- **4.15** Batch functions of extensions through prosupport. Status:
  open; lines 578–582. Explains: batch-functions, bridge-api.
- **4.16** PostgreSQL-level benchmarks: bench/pg families tax and win.
  Status: done; lines 584–745. Explains: bench. The planned pgbench
  family (597) is never reported.
- **4.17** Lazy pack: TessHeapBatch keeps tuples and deforms on request.
  Status: done, leftovers; lines 747–765. Explains: pack, scan-heap.
  764: pack still does not read the request masks.
- **4.18** Batches passed through under the limit, no row step. Status:
  done, leftovers; lines 773–792. Explains: limit, pack,
  batch-expressions. 791: nested chains (came with 4.20), Subquery Scan
  with quals.
- **4.19** Page faults in fresh parallel workers. Status: deferred;
  lines 794–842. Explains: parallel-gather, cost-model. Proposals а) and
  б) of 831 were carried out as 6.4а step 8 (5638).
- **4.20** Expression trees, qual order, mixed widths, three-valued
  logic, CASE. Status: done; lines 843–941. Explains: batch-expressions,
  filter. 919, 927: a NEON blend kernel waits for evidence; 928: how
  types extend.
- **4.21** Ordinary-query expressions: text, dates, float, numeric,
  aggregate FILTER/DISTINCT. Status: done, leftovers; lines 942–1378.
  Explains: batch-expressions, type-support, aggregate. Steps а)–ж5 from
  1039; 1241: timestamptz against date stays row-wise.
- **4.22** Complex C kernels moved to Rust: numeric, calendar, text.
  Status: done; lines 1379–1475. Explains: type-support, kernel-abi.
  1474: the gap on BETWEEN is kept.
- **4.23** Rust IN-list set, numeric sum states, own Finalize. Status:
  done, leftovers; lines 1476–1752. Explains: aggregate, type-support,
  batch-functions. 1522: steps 2 and 3 dropped by decision; 1738: spill
  of sum states left for a measurement.
- **4.24** Review B fixes: correctness, points 8–18. Status: done,
  leftovers; lines 1753–1812. Explains: batch-functions, bridge-api,
  kernel-abi. 1778, 1812: point 17 (pins of the last batch) left for
  later.
- **4.25** Review E fixes: size, structure, duplication, points 33–38.
  Status: done; lines 1813–1944. Explains: batch-functions, aggregate,
  join-hash.
- **4.26** Review points 39–41: table ergonomics, proptest, tuning rule.
  Status: done; lines 1945–2085. Explains: hash-table, tools. Its step 8
  is recorded at 2169–2174, inside the range of 4.27.
- **4.27** Review G fixes: execution, points 42–45. Status: done; lines
  2086–2174. Explains: runtime-helpers, sort, bridge-api.
- **4.28** out_sort loss: pack copy cost, TessAgg share, gather_words.
  Status: done; lines 2175–2204. Explains: cost-model, pack, sort. The
  remainder is 4.29.
- **4.29** Minimal tuples for parents that store rows (candidate).
  Status: deferred; lines 2205–2211. Explains: runtime-helpers. No
  header; described inside 4.28; decision at 2819–2823.
- **4.30** Review H: counters under VERBOSE, GUC_EXPLAIN, reliance on
  internals. Status: done; lines 2212–2299. Explains: node-contract,
  planner-coverage.
- **4.31** Review points 26–32: installed headers, kernel table ABI,
  errors. Status: done; lines 2300–2383. Explains: bridge-api,
  kernel-abi.
- **4.32** Planner review: cost factor, TessGather rescan, TessLimit in
  tessera_nodes. Status: done; lines 2384–2461. Explains: cost-model,
  parallel-gather, limit.
- **4.33** Review C: portability, build and CI. Status: done, leftovers;
  lines 2462–2574. Explains: ci. 2555: PostgreSQL 19 as the lower
  version is not done; goes with 8.8 (2563).
- **4.34** Review points 48–50: docs sweep, plan in docs/plan, PRs.
  Status: done; lines 2575–2652. Explains: process, planner-coverage.
  Adds docs/limitations.md.
- **4.35** README rework. Status: done; lines 2653–2705. Explains:
  process.
- **4.36** Sanitizers in CI. Status: moved; lines 2706–2716. Explains:
  ci. To 9.7 (2716).
- **4.37** Random queries against the core. Status: moved; lines
  2717–2730. Explains: tools, ci. To 9.8 (2730).
- **4.38** Parallel hash join crash over a spilled shared table. Status:
  done; lines 2731–2769. Explains: join-hash, hash-table. Found at the
  CI of PR #2.

Other paragraphs:

- 2770–2839: Ordering decisions («Порядок») from 2026-09-20 to
  2026-10-01; 2801–2803 maps 8.2–8.6 onto the steps of 4.21, 2819–2823
  defers 4.29.

## 5. Stateful nodes and temporary storage (lines 2841–5179)

- **5.1** Hash table in a borrowed region. Status: done; lines
  2862–2988. Explains: hash-table. 2980: the bucket filter (step 3) was
  made and reverted.
- **5.2** TessHashJoin in memory: keys, duplicates, residual,
  SEMI/ANTI/LEFT, Bloom. Status: done; lines 2989–3174. Explains:
  join-hash, hash-table. Six series from 3018.
- **5.3** Grouped TessAgg in memory over the hash table. Status: done;
  lines 3175–3196. Explains: aggregate, hash-table.
- **5.4** Mixed inputs: row-plan children through TessPack. Status:
  open; lines 3197–3198. Explains: pack, cost-model.
- **5.5** Parallel hash join with a shared table in DSA. Status: done,
  leftovers; lines 3199–3491. Explains: join-hash, hash-table. Also
  immovable table chunks (3308), exact count of duplicates (3397), Bloom
  filter below the join (3431), loom (3466); 3465: SQL measurements
  left.
- **5.6** Spill for TessHashJoin and TessAgg (tessera-spill, TessSpill).
  Status: done; lines 3492–3809. Explains: spill-format, join-hash,
  aggregate. Series 0–7; 3750: compression (7д) not wanted; the
  remainder at 3796 was taken by 5.12.
- **5.7** TessSort: full sort in memory, external, parallel. Status:
  done; lines 3848–4012. Explains: sort, spill-format. Text keys (series
  е) came with 5.13 step 4а (4821).
- **5.8** Top-N mode of TessSort. Status: done; lines 4013–4036.
  Explains: sort, limit.
- **5.9** DISTINCT: SELECT DISTINCT and DISTINCT inside aggregates.
  Status: done; lines 4037–4062. Explains: aggregate.
- **5.10** Merge join; RIGHT and FULL joins in TessHashJoin instead.
  Status: deferred; lines 4063–4116. Explains: join-hash. TessMergeJoin
  is put off (4081, 4092); RIGHT and FULL are done in the hash join
  (4083–4116).
- **5.11** Set operations: TessAppend, UNION through TessAgg, partition
  pruning. Status: done, leftovers; lines 4117–4565. Explains: append,
  aggregate. INTERSECT and EXCEPT came with 5.13 step 5 (4869); 4525,
  4560: MergeAppend only on a measured case; 4293: scale 10 not
  measured.
- **5.12** Faster spill: one file per set, columnar blocks. Status:
  done; lines 4566–4624. Explains: spill-format, join-hash. 4618: step
  г) is not done, by decision.
- **5.13** Batch tree for any types: generic aggregates and keys.
  Status: done, leftovers; lines 4625–4977. Explains: aggregate,
  join-hash, sort. Steps 0–4, then INTERSECT/EXCEPT as the first node of
  step 5 (4869); 4977: window functions and merge join of step 5 were
  not taken.
- **5.14** Run-time partition pruning by join; Bloom through TessAppend.
  Status: done; lines 4979–5177. Explains: append, join-hash,
  cost-model.

Other paragraphs:

- 2843–2860: Decision of 2026-09-23: the hash table comes first and
  lives in a borrowed region; old and new numbers of the items.
- 3810–3847: Common basis of 5.7–5.10: normalised sort keys, collation
  providers, choice of the sort algorithm.

## 6. Data sources (lines 5180–6012)

Item 6.4 has two lettered sub-items with rows of their own, 6.4а and
6.4в; 6.4б is the body of 6.4.

- **6.1** Incremental heap deform cursor (heap_deform.h). Status: done;
  lines 5182–5200. Explains: scan-heap.
- **6.2** TessHeapScan: native batch scan of heap tables. Status: done,
  leftovers; lines 5201–5233. Explains: scan-heap. 5222: named
  interfaces of physical columns wait for 6.5.
- **6.3** Lazy deform for filters and projections. Status: done; lines
  5234–5238. Explains: scan-heap, runtime-helpers. Closed through 4.17,
  4.13 and 6.2.
- **6.4** Bitmap/BRIN and parallel heap scan; the parallel chain.
  Status: done, leftovers; lines 5239–5275. Explains: scan-heap,
  runtime-helpers, aggregate. The body is 6.4б (TessSharedStats, partial
  paths, partial TessAgg); 5265: limit in parallel plans, counters per
  worker, cost model.
- **6.4а** Batch bitmap, index, BRIN, index-only scans; scan cost model.
  Status: done, leftovers; lines 5276–5893. Explains: scan-index,
  cost-model, scan-heap. Steps: 4 index-only 5356, 5 scan choice 5423, 6
  parallel paths 5532, 7 parallel index 5584, 8 preload and parallel
  model 5638, 9 filter model 5745, 7б parallel bitmap 5804, 10 parallel
  index model 5854. 5355, 5420: TID scan, parallel index-only scan.
- **6.4в** TessGather and TessGatherMerge: batches through Gather.
  Status: done, leftovers; lines 5894–6001. Explains: parallel-gather.
  5980: follow-ups; 6001: wide payload of the hash join
  (JOIN_MAX_PAYLOAD 64).
- **6.5** Compressed test table AM as a native source. Status: open;
  lines 6002–6003. Explains: bridge-api, planner-coverage. Carries 2.8.
- **6.6** Arrow FDW. Status: open; line 6004.
- **6.7** Direct Arrow batches, column choice, filter pushdown. Status:
  open; lines 6005–6006. Explains: batch-format.
- **6.8** Contract check for date, timestamp and text. Status: unclear;
  lines 6007–6009. Explains: type-support. No done marker. The kernels
  went to 8.2 (6009) and from there into 4.21; the fallback check for
  other physical formats has no source to run on before 6.5–6.7.

## 7. Two cores: PostgreSQL and Greengage 7 (lines 6013–6143)

- **7.1** compat.h layer and the build on Greengage. Status: open; lines
  6116–6119.
- **7.2** TessDeformLayout: own deform metadata. Status: open; lines
  6120–6121. Explains: scan-heap.
- **7.3** Fork patches in the Greengage tree (squelch, walk_kids,
  mutator). Status: open; line 6122.
- **7.4** Smoke test on the demo cluster. Status: open; lines 6123–6127.
- **7.5** Node registry walk; tess_batch_scan_path over all sources.
  Status: open; line 6128. Explains: bridge-api, planner-coverage.
- **7.6** TessAocsScan for AOCO tables. Status: open; lines 6129–6130.
  Design at 6100–6113.
- **7.7** TessAgg over two-stage aggregation. Status: open; line 6131.
  Explains: aggregate.
- **7.8** TessAoScan for ao_row tables. Status: open; line 6132.
- **7.9** Test suites on Greengage. Status: open; lines 6133–6134.
  Explains: tools.
- **7.10** Later: segment counters in EXPLAIN, RLE runs, ORCA plans.
  Status: open; lines 6135–6136. Explains: planner-coverage.

Other paragraphs:

- 6015–6115: Design of 2026-09-22, "one tree, two cores": facts about
  the fork, what differs, AOCO columns, the seams (compat.h), the AOCO
  source.

## 8. The path to TPC-H (lines 6144–6604)

The section has two lists. List A (lines 6177–6385) holds 8.1–8.6, list
B (lines 6417–6587) holds 8.4–8.10; the numbers 8.4–8.6 occur in both
with different meanings.

- **8.1** TPC-H harness and conformance run (tools/tessera-tpch).
  Status: done, leftovers; lines 6177–6361. Explains: bench, tools.
  6361: steps 7–11 of the start order (6253–6269) are not done; they
  wait for §9 (6602).
- **8.2** Cheap gates: dates, batch prefix rule, IN, text equality.
  Status: moved; lines 6362–6370. Explains: planner-coverage,
  batch-expressions, type-support. Into 4.21 в), д), е) (2802, 6593).
- **8.3** numeric as decimal64. Status: moved; lines 6371–6377.
  Explains: type-support. Into 4.21 ж) (2803, 6593).
- **8.4 (list A)** Stateful nodes in the order of §5. Status: moved;
  lines 6378–6382. Explains: join-hash, aggregate, sort. A pointer to
  5.1–5.10, done there (merge join 5.10 deferred); no marker of its own.
- **8.5 (list A)** Indexes 6.4а for a TPC-H schema with secondary
  indexes. Status: open; line 6383. Explains: scan-index, bench. 6.4а
  itself is done; the schema with indexes is step 9 of 8.1 (6267).
- **8.6 (list A)** Later: parallel tpch, SF 10, tpch on Greengage.
  Status: open; lines 6384–6385. Explains: bench. EXTRACT and substring
  came with 4.21 е).
- **8.4 (list B)** Strings: char(n), varchar, text kernels. Status:
  moved; lines 6417–6424. Explains: type-support. Into 4.21 д) (2803,
  6593).
- **8.5 (list B)** Expression forms: date against timestamp, OR/NOT, IN,
  CASE, extract. Status: moved; lines 6425–6428. Explains:
  batch-expressions, type-support. Into 4.21 в), е) (2803, 6593).
- **8.6 (list B)** Aggregates and group keys over numeric, date,
  strings. Status: moved; lines 6429–6430. Explains: aggregate,
  type-support. Into 5.13 and 4.21 ж) (2803, 6593).
- **8.7** Sorting: top-N with LIMIT over batches. Status: moved; lines
  6431–6433. Explains: sort. Into 5.8 (6594).
- **8.8** Comparison with pg_duckdb. Status: open; lines 6434–6456.
  Explains: bench. Only the research and the protocol are recorded; the
  item also takes PostgreSQL 19 as the lower version (2563, 6442).
- **8.9** Backward scan and mark/restore. Status: open; lines 6457–6475.
  Explains: node-contract, sort, scan-index.
- **8.10** Own join time model replacing the core cost share. Status:
  done; lines 6476–6587. Explains: cost-model, join-hash, aggregate.
  6583: docs/costs.md.

Other paragraphs:

- 6146–6175: Decision of 2026-09-22 (TPC-H as the criterion) and what
  blocks the 22 queries, by code.
- 6387–6416: Minimal set for TPC-H (2026-09-26): types, expressions,
  aggregates, keys and nodes that the 22 queries need.
- 6588–6603: Order of the items, check of the section, refinements of
  2026-09-30 and 2026-10-02 (8.2–8.7 absorbed; §9 goes before steps
  7–11).

## 9. Verifiability and code structure (lines 6605–7442)

Group А (gates of checks) is 9.1–9.10, group Б (reorganisation of C) is
9.11–9.14, group В (pure logic moved to Rust) is 9.15–9.18; 9.19–9.21
were added on 2026-10-03.

- **9.1** SQL suites with the debug Rust library in CI. Status: done;
  lines 6675–6687. Explains: ci.
- **9.2** Restrictive clippy lints; infallible iterator of mask words.
  Status: done; lines 6689–6724. Explains: ci, kernel-abi. 6698: adds
  tessera-bench --disasm.
- **9.3** Layout check at module load. Status: done; lines 6726–6737.
  Explains: kernel-abi.
- **9.4** -Werror for our C in CI. Status: done; lines 6739–6745.
  Explains: ci.
- **9.5** make rust-loom (loom models of the table) in CI. Status: done;
  lines 6747–6753. Explains: ci, hash-table.
- **9.6** Miri in CI. Status: done; lines 6755–6766. Explains: ci.
- **9.7** Sanitizers (ASan, UBSan) in CI. Status: done; lines 6768–6787.
  Explains: ci. Former 4.36.
- **9.8** Random queries against the core: tessera-crosscheck,
  tessera-pgtool. Status: done; lines 6789–6852. Explains: tools, ci.
  Former 4.37; findings and their fixes (PR 18–25) at 6824–6848.
- **9.9** Mutation testing with cargo-mutants. Status: done; lines
  6854–6869. Explains: ci.
- **9.10** Uncontained places: sizes from C, numeric sum bound,
  unreachable. Status: done; lines 6871–6892. Explains: kernel-abi,
  type-support, sort.
- **9.11** Node skeleton in runtime/ over the unary helper. Status:
  done, leftovers; lines 6896–6927. Explains: runtime-helpers,
  node-contract. 6919: four planned parts are not done, each with its
  reason.
- **9.12** Cuts of large node files by concern. Status: done, leftovers;
  lines 6929–6967. Explains: aggregate, sort, join-hash. 6942: done in
  part; 6966: hashjoin_bloom.h left to 9.15.
- **9.13** Kernel operations table generated from one list. Status:
  done; lines 6969–6982. Explains: kernel-abi.
- **9.14** Planner helpers: sort keys, plan child columns, tess_enabled.
  Status: done; lines 6984–6999. Explains: runtime-helpers, sort.
- **9.15** Layout formulas with one owner each. Status: done; lines
  7003–7050. Explains: spill-format, hash-table, kernel-abi.
- **9.16** numeric fast path of agg.c moved onto decimal.rs. Status:
  done; lines 7052–7118. Explains: aggregate, type-support. 7110:
  sum(int2) by groups is 2–3 % slower, kept.
- **9.17** Pure executor pieces in kernels; one weighted spill rule.
  Status: done, leftovers; lines 7120–7221. Explains: join-hash,
  spill-format, hash-table. PR #42 (7389); side fixes PR #40, #41
  (7211–7215); 7135: keydict put off; 7220: rows_write of the aggregate
  spill.
- **9.18** Duplicates inside Rust. Status: open; lines 7223–7232.
  Explains: kernel-abi, batch-functions.
- **9.19** Change-check tools: tessera-ab, tessera-bench --disasm
  --module. Status: done; lines 7234–7271. Explains: tools, bench.
- **9.20** Bit-walk step in one place (tessera_core::ones). Status:
  done; lines 7273–7286. Explains: batch-format. Follows the mutants
  check of PR #38; 7285: the PMU run is left to the user.
- **9.21** Faster mutants of changed lines. Status: done, leftovers;
  lines 7287–7353. Explains: ci. Follows the CI of PR #42; 7352: the
  full manual run on the new runners was not started.

Other paragraphs:

- 6607–6673: Decision of 2026-10-02, what the section delivers, facts
  about where errors were, conclusion; heading of group А.
- 7355–7365: What the section gives, item by item.
- 7367–7376: What is not done and why (Kani, cbindgen, Rust under ASan,
  parallel automata in Rust, checks in the release profile).
- 7378–7390: Order of the items (decision of 2026-10-02, extended on
  2026-10-03).
- 7397–7441: Refinement of 2026-10-02 after a code review before the
  work: corrected facts and decisions.

## Standing rules and other parts

Parts of the log that are not items. The ordering and summary blocks of
the sections are listed here again, together with the short ones.

- Lines 1–7, Banner: The log is frozen on 2026-10-04; where the specs,
  the roadmap and this index are.
- Line 9, Title: «Создание Tessera и поэтапный перенос из pg_batch».
- Lines 11–38, «Кратко» (summary): The C API as the main contract; the
  rule of one change per commit; work in series with a review after
  each.
- Lines 40–69, «Начальная структура» (initial layout): Directory tree,
  the mixed build (Cargo, Meson, PGXS, Makefile), hand-written C
  headers.
- Lines 71–85, «Логические типы и физические представления»: A logical
  type and its physical representation are independent; the Datum path
  is the mandatory fallback.
- Line 87, «Последовательность коммитов»: Heading above sections 1–9.
- Lines 114–117, §2: Order inside the section (2.8 is not needed before
  2.9–2.11) and the rendezvous name tessera.api.v0.
- Lines 280–302, §3: Standing rules for Rust: unsafe only in
  tessera-capi and tessera-pmu, no PostgreSQL calls from Rust, panics
  and errors.
- Lines 767–772, §4: The first end-to-end chain that must work after
  4.17.
- Lines 2770–2839, §4: Ordering decisions («Порядок») from 2026-09-20 to
  2026-10-01; 2801–2803 maps 8.2–8.6 onto the steps of 4.21, 2819–2823
  defers 4.29.
- Lines 2843–2860, §5: Decision of 2026-09-23: the hash table comes
  first and lives in a borrowed region; old and new numbers of the
  items.
- Lines 3810–3847, §5: Common basis of 5.7–5.10: normalised sort keys,
  collation providers, choice of the sort algorithm.
- Line 5178, §5: Policy of partitioning and recursion stays in the node;
  tessera-spill owns blocks and I/O.
- Line 6011, §6: New source callbacks enter the ABI only with their
  first real consumer.
- Lines 6015–6115, §7: Design of 2026-09-22, "one tree, two cores":
  facts about the fork, what differs, AOCO columns, the seams
  (compat.h), the AOCO source.
- Lines 6138–6141, §7: Check of the section as a whole.
- Lines 6146–6175, §8: Decision of 2026-09-22 (TPC-H as the criterion)
  and what blocks the 22 queries, by code.
- Lines 6387–6416, §8: Minimal set for TPC-H (2026-09-26): types,
  expressions, aggregates, keys and nodes that the 22 queries need.
- Lines 6588–6603, §8: Order of the items, check of the section,
  refinements of 2026-09-30 and 2026-10-02 (8.2–8.7 absorbed; §9 goes
  before steps 7–11).
- Lines 6607–6673, §9: Decision of 2026-10-02, what the section
  delivers, facts about where errors were, conclusion; heading of group
  А.
- Line 6894, §9: Heading of group Б: reorganisation of C without moving
  logic.
- Line 7001, §9: Heading of group В: pure logic moved to Rust.
- Lines 7355–7365, §9: What the section gives, item by item.
- Lines 7367–7376, §9: What is not done and why (Kani, cbindgen, Rust
  under ASan, parallel automata in Rust, checks in the release profile).
- Lines 7378–7390, §9: Order of the items (decision of 2026-10-02,
  extended on 2026-10-03).
- Lines 7392–7395, §9: Check of the section.
- Lines 7397–7441, §9: Refinement of 2026-10-02 after a code review
  before the work: corrected facts and decisions.
- Lines 7443–7474, «Документация и проверки» (documentation and checks):
  What every commit must contain, the list of checks, the benchmark
  thresholds, the form of a commit message.
- Lines 7476–7487, «Принятые ограничения» (accepted limits): pg_batch
  only as a model, supported cores, int4-first scope, no fixed
  type-to-format link in the ABI, counters on macOS only.

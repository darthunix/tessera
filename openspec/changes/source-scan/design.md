## Context

See proposal.md, "Why", for the motive. The state this design starts
from:

- A batch node is a `CustomScan` registered in the node registry. A
  parent tells a batch child from a row child by the path:
  `tess_path_node` names the node of a path that `tess_path_create`
  built. Any module can register such a node, and the batch parents read
  its batches.
- A batch (`tessera/batch.h`) gives its columns through one required
  callback, `get_datum_column`, for the rows a reader asks for. A column
  can be made on request, so a source can decode a column only when a
  reader asks for it and only for the rows still selected.
- The runtime's native scan, `tess_batch_scan_path`, takes a sequential
  scan path and calls `scan_rows` of the one node registered as
  `TESS_HEAP_SCAN_NODE_NAME`. `tess_batch_input_path`, which every batch
  parent calls on the path the core chose for its child, uses it for a
  relation without clauses, and otherwise puts the pack node over the
  path.
- The scan planner (`nodes/scan_planner.c`, its `set_rel_pathlist` hook)
  admits only `RELKIND_RELATION` relations, in a `SELECT` without row
  marks and lateral references (`relation_supported`). Over the core's
  sequential scan it adds `TessFilter` over the native scan, or over a
  pack of the core's scan, and the native scan alone for a relation
  without clauses, each serial and partial. The child of the filter
  reads the relation's targets and the clauses' columns
  (`filter_input_target`). It ranks the paths against the index and
  bitmap scans by its scan model, and gathers the cheapest partial path
  with `TessGather`.
- `TessFilter`'s plan takes the clauses from the sequential scan under a
  pack, and expects none under the heap scan node.
- The source registry (`tessera/source.h`) holds names only.
- For a foreign table the core asks its wrapper for paths
  (`GetForeignPaths`), then calls the `set_rel_pathlist` hook, then
  gathers the partial paths. A wrapper offers partial paths only when it
  says its scans are safe in parallel and adds them itself. A wrapper
  may already check clauses in its own rows: it leaves out of its plan's
  qualifiers the clauses it enforces.
- A relation's clauses (`baserestrictinfo`) carry a security level. The
  core orders them by level and lets a clause move ahead of a lower
  level only when it is leakproof (`RestrictInfo.leakproof`, set for a
  clause above level zero when no function that may reveal its arguments
  takes a value of the relation: `restriction_is_securely_promotable`).
  `TessFilter`'s plan keeps the order in which the core hands it the
  clauses, and the scan planner's copy of the core's rule
  (`tess_order_clauses`) gives that order at planning, which is the
  order a source is handed. A foreign table takes no row security policy
  of its own: only tables and partitioned tables do. In a `SELECT`,
  clauses of more than one level still reach a foreign table's scan:
  from the policy of a partitioned table or of a parent of inheritance,
  which the core hands down to each partition or child, foreign ones
  among them, and from a join with a table that has a policy, which
  raises the level of the query's own clauses while a clause derived
  from the policy's (through an equivalence of columns) keeps the
  lowest. A view with `security_barrier` over a foreign table is planned
  as a subquery of its own: its clause is an ordinary clause of the
  table, and a leaky clause of the outer query is never pushed into it,
  so it stays above, on the subquery's scan.

## Goals / Non-Goals

**Goals:**

- A module reads a foreign table in batches for Tessera's filter and
  parents, with no change to Tessera when another such module comes.
- Heap plans, their costs and their results stay exactly as they are.
- The contract is the one the Parquet source needs: a scan in batches,
  serial and parallel, with the source's own costs, its own skipping of
  rows and its own checking of clauses.
- Each clause is checked once, by the source or by the filter, and no
  clause runs ahead of the policy clauses that guard it.

**Non-Goals:**

- Anything that reads Parquet; that is `parquet-fdw`.
- Faster columns: the batch contract does not change.

## The whole

```
 planning a foreign table t that a source reads

 core      GetForeignPaths ─► ForeignPath       ForeignPath
                              (serial)          (partial)
                                  │                 │
 Tessera   each source in order: scan_rows(root, a copy, clauses)
 (scan                            │                 │
 planner)                         ▼                 ▼
                           the source's path  its partial path
                                  │                 │
           clauses left:      TessFilter        TessFilter
           none left:         the path alone    the path alone
                                  │                 │
                                  │                 ▼
                                  │            TessGather
                                  ▼                 ▼
 parents   TessAgg, TessSort, TessHashJoin, TessLimit read the
           cheapest path as a batch child, with no TessPack
```

"Clauses left" are the clauses the source does not check exactly. The
core's paths stay in the lists unless a path of Tessera dominates them,
as `add_path` decides; the planner chooses by cost. A parent given the
core's `ForeignScan` packs it, as today. With `tessera.enable` off, or
when no source returns a path, nothing above is added and the plan is
the core's.

## Decisions

### The callback belongs to the source

`TessSource` gains `scan_rows(PlannerInfo *, Path *, TessScanClauses
*)`: the signature of the heap scan node's callback, and the clauses
with the answers. The returned path is of an ordinary node of the node
registry, which keeps its execution, its EXPLAIN and its parallel
callbacks. A source is "who reads this relation"; a node is "a kind of
plan node". The answers for the clauses are a source's too.

Refused:

- **`scan_rows` of every node kind.** No new field, since `TessNode` has
  the callback already; but the node registry would then decide which
  module reads which relation, and the source registry would stay
  without any use.
- **The source named in the wrapper's options.** Tessera would read
  options of a wrapper it does not own, and every wrapper's validator
  would have to accept Tessera's option.
- **The wrapper adds its batch path itself, in `GetForeignPaths`.** A
  path that leaves clauses to a filter is not a path of a relation with
  clauses, and Tessera would have to find it among the relation's paths
  and learn which clauses it leaves.

### The scan planner alone asks the sources

The scan planner walks the registry with `next` for a foreign table,
offers each source the path and the clauses, and keeps the answers of
the first that returns a path. It builds `TessFilter` with the clauses
left and records them in the filter's path; at `PlanCustomPath`, which
receives all the relation's clauses, the filter keeps only those it
recorded. The relation's own list of clauses does not change.

`tess_batch_scan_path` and `tess_batch_input_path` do not change, and no
batch parent asks a source: the source's path, alone or under the
filter, is already among the relation's paths when the parents are
planned, and a parent reads it when the core chose it. A parent given
the core's `ForeignScan` packs it, so a source that costs more than its
wrapper stays unused under every parent too. The conditions of the
relations offered (a `SELECT` without row marks, no lateral references,
a simple relation) stay in one place, the scan planner's
`scan_supported`, and no public call of the runtime changes.

Refused:

- **`tess_batch_scan_path` asks the sources.** The parents call it on a
  path the core already chose, so a source that lost by cost would come
  back under them; the conditions of the relations offered would have to
  be repeated in it; and a public call that returns a path would also
  have to return the clauses left.

The walk is in the order of registration, so the first source loaded
wins where two would read the same relation. `next` is a required field
of the registry's table (`TESS_SOURCE_REGISTRY_OPS_MIN_SIZE`): a module
built with these headers refuses an older bridge, as for any table of
the API that is too small. The scan planner checks the table's version
and size before the walk with `tess_source_registry_check`, a call of
the runtime that takes the table as its argument, as the runtime checks
the kernels' registry; taking the table lets a test hand it an older
one, which no test could do through the bridge.

### Foreign tables only; heap stays with the heap scan node

A source is offered a foreign scan path, never a sequential scan of a
heap table. The heap paths of the scan planner do not change, so no heap
plan can move, and the sources are asked only for foreign tables. A
table of another access method waits for a source of that kind: it would
be offered a sequential scan, and none exists to test it.

### The path offered and the path returned

The source gets a copy of the whole `ForeignPath`, with the wrapper's
private data (`fdw_private`), so a source that is also the wrapper finds
what its `GetForeignPaths` planned. The copy's target is the filter's
input, the relation's targets and every column the clauses read, as the
heap scan's child gets: the filter needs the columns of the clauses
left, and Tessera does not know them before the answers. The returned
path supports projection (`CUSTOMPATH_SUPPORT_PROJECTION`), and Tessera
sets its target itself: the offered one under the filter, the relation's
own when the path stands alone. Of several foreign paths without
parameters and path keys, Tessera offers the one of the least total
cost, and the serial and the partial offer are each a walk over the
sources of its own, with answers of its own.

The returned path becomes Tessera's: Tessera sets its target and, when
it stands alone, its rows, and `add_path` may free it when another path
dominates it, so a source builds a new path at each call and keeps no
pointer to one it returned.

Tessera checks the returned path before it uses it: a batch path of a
registered node (`tess_path_node`), of the same relation, without
parameters and path keys, with projection, parallel-aware and
parallel-safe with the offered path's number of workers for a partial
offer, and serial for a serial one. Any other path is a fault of the
source, `XX000`: a serial path in the list of partial paths would return
each row once for each participant.

### The source answers for each clause

With the path, Tessera hands the source the relation's clauses in the
core's order of evaluation, and the source answers for each: it does not
use it, it uses it only to skip rows that cannot pass, or it checks it
exactly. The filter then checks every clause but the exact ones, and
when the source checks them all, no filter is planned. The answers are
given at planning, because the plan must know which clauses the filter
keeps: its cost, its `EXPLAIN` and its expressions come from them.

The source puts the expressions of the clauses it uses
(`RestrictInfo.clause`) into its path's expressions, its
`PlanCustomPath` passes them to `TessPlanConfig.expressions`, which
become its plan's `custom_exprs`, and it plans its node with the
relation as its scan tuple (`scan_tuple_is_relation`), as the heap scan
does. Being among the plan's expressions also makes the planner list the
executor's parameters of the clauses for the node (`finalize_plan`,
which lists `PARAM_EXEC` alone), so that a new value of an outer row's
or a subquery's parameter starts the scan again. A prepared statement's
parameter (`PARAM_EXTERN`) is fixed for one execution of a cached plan,
and the source reads it, as any such value, when its node is first asked
for rows. An answer holds for every scan and every execution, and the
source evaluates a clause with a parameter at each scan with its current
value. After the call Tessera checks that every clause answered for is
among the returned path's expressions, which shows only that the source
carried it to its `PlanCustomPath`. What starts the scan again is the
plan's `custom_exprs`, which `finalize_plan` reads, and which a source
fills from its path's expressions: a source that left a clause out there
would not be scanned again when its parameter changes, under parents
that keep their result while their child's parameters stay the same, and
its workers would not get an initplan's value under `TessGather`. So
`TessFilter`, which holds its child's plan, checks there too that every
clause answered for is among the plan's expressions; for a source's path
alone no code of Tessera sees the plan, and the contract's review covers
it. The planner's last pass (setrefs) then resolves their columns
against the relation, whatever the node's targets: against the targets,
as a plan's custom scan target list would have it, a clause on a column
the query does not select could not resolve. The plan's qualifiers stay
empty: `TessFilter` takes a native scan child without qualifiers, as it
takes the heap scan, and a clause there would show in `EXPLAIN` as the
source node's `Filter`, with its rows counted as "Rows Removed by
Filter", the line that belongs to `TessFilter`.

The Parquet source is the first to answer: it can check a comparison of
a column with a constant on the form in which it keeps the column, the
values of a dictionary once instead of each row, and skip groups of rows
by their least and greatest values. The test source answers too, so the
contract is tested apart from Parquet.

Refused:

- **The filter checks every clause, and the source tells nothing.** A
  source that filters would have the same clauses checked again on every
  row it returns, which costs what the source saved.
- **The source checks every clause it is given, and nothing is
  rechecked.** A source that cannot check a regular expression, or a
  clause over two columns, would have to refuse the whole relation.
- **The answers at execution.** The plan, its cost and its `EXPLAIN`
  would not say which clauses the filter checks.

### The clauses a source may use

A source evaluates the clauses it uses before the filter evaluates the
rest, so using a clause moves it ahead of the clauses the filter keeps.
Tessera marks a clause usable when it calls no volatile function, holds
no `SubPlan`, and is leakproof or of the lowest security level among the
relation's clauses: the core's own rule for moving a clause
(`restriction_is_securely_promotable`). This holds for skipping rows
too, since the least and greatest values of a group come from the rows a
policy may hide, and a function that leaks them would show those rows. A
volatile clause, one that calls a function such as `random()` or
`nextval()`, is never usable: a source that skipped rows by it would run
it apart from the filter's own run, on other values, and the core keeps
such a clause in one place and runs it on each row that reaches it, as
Tessera's batch nodes do. How many rows reach it may still differ from
the core's plan, when the source checks another clause exactly that the
core would have run after it, as `docs/limitations.md` says of batch
nodes already. A clause that holds a `SubPlan`, a subquery PostgreSQL
runs from within the clause, is not usable either: a clause a source
uses to skip rows stays in `TessFilter` too, and its subquery would then
sit in two plan nodes at once, a shape the core never builds. A clause
on an uncorrelated subquery's value holds a parameter instead, and stays
usable. A source that answers for a clause it may not use raises an
error: it is a fault of the source, which a test can catch, not a choice
the planner can make for it.

The case that tests the rule is a policy of a partitioned table whose
partition is a foreign table, read by a role the policy binds: the
policy's clause is of the lowest level, a leakproof clause of the query
is usable, and a leaky one is not. A view with `security_barrier` is no
such case for the mark, since its leaky outer clause never reaches the
table's scan; its test checks that the clause stays on the subquery's
scan and that the source is not handed it, then compares results. A
leakproof outer clause, though, is pushed into such a view, where it
comes at the lowest level, beside the view's own clause that
`TessFilter` may check after it; so the rule that a source's own check
reveals nothing of the rows cannot depend on the level.

### No pack over a foreign scan in the scan planner

For a foreign table the scan planner builds its paths only over a
source's path. A pack over the core's `ForeignScan`, as the scan planner
builds over a sequential scan, would have to take the clauses the
wrapper evaluates out of the wrapper's plan, which Tessera does not own.
A batch parent still packs the core's `ForeignScan` through
`tess_batch_input_path`, as today.

### A source's path costs what the source says

The heap scan's paths start at the core's scan times
`tessera.scan_cost_factor`, which calibrates the heap scan against the
core's sequential scan of the same pages, and the scan planner's ranking
then prices them again by its model of a scan's time against the index
and bitmap scans. A source's paths take neither. A source and its
wrapper read the same data in two ways that only the source can price,
so the source's path keeps the cost the source gave it. When it stands
alone, its rows are those of the path offered, the rows the wrapper gave
that path, per participant for a partial one, since the core assumes
that the paths of one relation return the same rows
(`add_partial_path`); the source's estimate serves only as the filter's
input. `TessFilter` over it adds the cost of the clauses left as
`cost_qual_eval` gives it for that list: its start cost to the start and
to the total, since a hashed subquery in a clause is built before the
first row, and its cost for one row times each row the source's path
returns to the total; and has the rows of the path offered too, as the
heap scan's filter has its core path's. The cost of the clauses the
source checks is in the source's own cost, and is not counted twice.
Refused: the heap's rule, which would price a columnar source by its
wrapper's rows.

### Parallel from the core's partial path

`TessGather` gathers the source's partial path as it gathers a partial
heap scan, and refuses, as it does there, a path that returns no column:
under `count(*)` the partial grouping reads the source's path in each
participant and the gather stands above the grouping.

As with the heap scan, a partial path of the source comes from a
parallel-aware partial path of the core, whose number of workers it
keeps. Under `TessFilter` its rows per participant are the source's, the
rows it returns, as the filter's input; alone it takes the rows the
wrapper gave the offered partial path. Its parallel safety is the
source's promise and never more than the offered path's, and Tessera's
paths over it, alone or under `TessFilter`, take it from the source's
path, not from the core's template, so that a source that may not run
inside a worker never does. The wrapper must offer a partial foreign
scan path that it can execute in rows, since the planner may choose it.
A source whose wrapper offers none gets no parallel batch read. Refused:
a source that makes a partial path from a serial one, which would put
the choice of workers in the source and away from the core's rules.

### A test source, built in the tree and outside it

A test module holds a foreign data wrapper and its batch node, which
serve rows generated from the table's options, built with the runtime's
batch builder; the options and the sources beside it, which read
nothing, answer and then decline, are of the least size, or return a
path or answers that are not allowed, are listed in task 3.1, so that
every scenario of the spec has the case it needs. The same sources build
from copies against the installed headers and runtime library in
`test/installed/`, as `TessLimit`'s do, which is the check of the
contract's first rule.

### Decisions of the maintainer

On 2026-11-20, after the analysis of ClickBench:

- Tessera enters ClickBench reading the benchmark's Parquet files in
  place, through a Parquet foreign data wrapper.
- The planning contract is built in Tessera, so that the source's
  batches go under `TessFilter` and the batch parents, rather than the
  wrapper evaluating its clauses alone.
- `parquet-fdw` no longer depends on `compressed-table-source`: the
  Parquet wrapper is the contract's first real source.
- The Parquet wrapper lives in this repository, as a module of its own.
- The source filters: it answers for each clause, and the filter checks
  only what the source does not check exactly. The first draft of this
  change, in which the filter checked every clause and a source only
  skipped rows, was refused the same day.

## Was and will be

- **The source registry.** Was: `add`, `remove`, `find`, and a name.
  Will be: also `next`, required; an optional `scan_rows` of a source;
  the clauses offered with it and the answers.
- **The runtime's native scan and batch input.** Was and will be the
  same: the heap scan node for a sequential scan of a plain heap table;
  a pack over any other path.
- **The runtime's API.** Was: no check of the source registry's table.
  Will be: `tess_source_registry_check`, which the scan planner calls
  before the walk.
- **The scan planner.** Was: tables (`RELKIND_RELATION`) only, never a
  foreign table; the heap scan node then reads only those whose access
  method is heap, and a pack over the core's scan reads the others. Will
  be: also a foreign table that a source reads: the walk over the
  sources, `TessFilter` over the source's path with the clauses the
  source leaves, or the path alone with the relation's target when it
  leaves none, serial and partial, and `TessGather`; the index and
  bitmap scans and the ranking by the scan model stay the heap's.
- **`TessFilter`'s plan.** Was: a child of the heap scan node carries no
  clauses; a pack's child carries them and the filter takes them. Will
  be: the same, and over a source's child the filter keeps the clauses
  its path recorded and leaves the source's own expressions as they are.
- **Costs.** Heap: unchanged. A source: its own, and the cost of the
  clauses left under the filter.
- **The guides.** `docs/source.md` becomes the guide to writing a
  source, with the test source as its example; `docs/runtime.md`
  ("Building paths"), `docs/nodes.md` (`TessFilter`),
  `docs/limitations.md` ("Tables other than heap tables"),
  `docs/bridge.md` (the source registry), `docs/node.md` and the comment
  of `TessNode.scan_rows` in `include/tessera/node.h`, which point to
  the source's callback of the same name, follow.

## What we do not do

- **Join clauses for a source.** A batch path is never parameterized, so
  a source is offered only the relation's own clauses.
- **Columns in other forms.** The Parquet source measures first what
  `Datum` arrays cost it.
- **Tables of another access method.** No source of that kind and no
  test of one.
- **Index, bitmap and parameterized scans of a source.** The Parquet
  files have no index, and a batch path is never parameterized: the
  inner side of a nested loop parameterized by the outer row stays the
  core's, while an unparameterized inner side is read through the source
  and scanned again for each outer row.
- **Statements that write or lock rows.** Their row marks need the
  wrapper's own tuples. The rule is of the query level that the scan
  planner plans, as for heap: a subquery planned on its own inside a
  statement that writes is a `SELECT` level, and reads through a source.
- **The compressed test table access method.** It is no longer the first
  source; its entry keeps the port itself.

## Risks / Trade-offs

- [A source estimates its rows or costs wrongly] → plans move between
  its batches and its wrapper's rows. The test source's cost option
  checks the choice; each source calibrates its own costs.
- [A source skips rows that pass a clause, or returns rows that fail a
  clause it checks exactly] → wrong results without an error. The duty
  is in the spec; the test source is compared with the same source
  checking nothing and with heap, and each source is compared so in its
  own tests.
- [A source uses a clause that a policy guards] → Tessera marks such a
  clause not usable and refuses an answer for it; the case of a policy
  over a foreign partition tests it.
- [A source checks a clause with other results than the core's, for a
  collation, a time zone or an overflow] → its exact answer is a promise
  of the core's result. The source answers only for the operators and
  types it checks as the core does, and leaves the rest.
- [A source checks exactly a clause that the core would have evaluated
  after another, which fails on some row] → the error of that row can
  come or not as the order changes, as it can with any plan of the core;
  `docs/limitations.md` says so of batch nodes already.
- [A module built with these headers meets an older bridge] → it refuses
  the registry's table, as `next` is required, and the scan planner
  checks it before the walk.
- [A source checks a clause it uses, at any level, by code of its own,
  which fails or reports on some row's value] → the row may be one a
  policy hides, and the message would show it. A source checks such a
  clause without errors or messages that depend on the values, or leaves
  it to `TessFilter`; the spec says so.
- [A source plans its node with the clauses `PlanCustomPath` hands it as
  qualifiers, as custom scans often do] → it would run the clauses it
  leaves, the unusable ones among them. Its plan keeps no qualifiers,
  and `TessFilter` refuses such a child.
- [A wrapper that reads its data with the rights of a user mapping] → a
  source's node gets no user to check as, which a `ForeignScan` gets
  from the planner; such a source takes the relation's user at planning
  itself. The Parquet source reads local files and has no such mapping.
- [`EXPLAIN` differs from the core's] → rows a source skipped or removed
  count in no line of `TessFilter`, where the core's `ForeignScan` would
  count them as removed by its filter. The spec says so.

## Measurement plan

None: no hot path changes. The sources are asked at planning, for
foreign tables only. That heap plans do not move is shown by every
existing suite keeping its expected output, and by a case of the new
suite that plans heap tables with a source registered.

## Open Questions

None that would change the specs or the tasks.

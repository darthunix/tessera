# Spec Delta

## Purpose

The way a module that keeps a relation in a format of its own hands
Tessera batches of the relation's rows. A source is such a module's
registered entry: Tessera asks it for a batch scan of a relation, and
plans its filter and its batch parents over the scan the source returns.
The relations are foreign tables: tables whose rows a foreign data
wrapper, a module of PostgreSQL's interface for outside data, reads from
elsewhere, such as files. A heap table, PostgreSQL's own kind, is read
by Tessera's heap scan node. A batch parent is a node of Tessera that
reads batches from its child, such as `TessAgg`, `TessSort`,
`TessHashJoin` or `TessLimit`; over a child that returns rows it reads
through `TessPack`, the node that packs rows into batches. `TessAppend`
reads the children of an append, such as the partitions of a table, in
batches, and `TessGather` gathers the batches of a parallel plan's
processes. A path is one way to read a relation that PostgreSQL's
planner weighs against the others by its cost; its target is the list of
columns it returns; a clause is one condition on the relation's rows, of
the query's `WHERE` or one that PostgreSQL adds, such as a row security
policy's. The bridge is Tessera's module `tessera`, which holds the
registries and gives them through its table of operations, `TessApi`.
The source may check clauses of the relation itself, and Tessera's
filter, `TessFilter`, then checks only the rest. The requirements go
from the registry to the call, the source's answers for the clauses, the
relations offered, and the plans built over a source's scan. The batch
itself and the obligations of a batch node are those of every node of
Tessera; [design.md](design.md) explains the whole and the reasons.

## ADDED Requirements

### Requirement: Registering, finding and listing sources
A module SHALL register a source as a `TessSource` of
`tessera/source.h`, through the operations of the registry,
`TessSourceRegistryOps`, which `TessApi.sources` gives. The provider
SHALL own the structure and its name, and keep both valid and unchanged
from `add` until `remove` returns; the registry SHALL keep the
provider's pointer and copy nothing.

```
 TessSource                                 owned by the provider
 ┌─────────────┬─────────────┬──────────────┬───────────┐
 │ abi_version │ struct_size │ name         │ scan_rows │
 │ uint32      │ Size        │ const char * │ function  │
 └─────────────┴─────────────┴──────────────┴───────────┘
   required:  abi_version, struct_size, name (TESS_SOURCE_MIN_SIZE)
   optional:  scan_rows, read only when struct_size includes it;
              NULL there means the source reads no relation

 TessSourceRegistryOps                      owned by the bridge
 ┌─────────────┬─────────────┬──────────┬──────────┬──────────┬──────────┐
 │ abi_version │ struct_size │ add      │ remove   │ find     │ next     │
 │ uint32      │ Size        │ function │ function │ function │ function │
 └─────────────┴─────────────┴──────────┴──────────┴──────────┴──────────┘
   required:  all; TESS_SOURCE_REGISTRY_OPS_MIN_SIZE includes next

 The fields lie in this order, at the offsets the C compiler gives
 them; struct_size tells which are present, as for every structure
 of the API (TESS_ABI_SIZE_INCLUDING_FIELD of tessera/abi.h).
```

```c
CustomPath *(*scan_rows) (PlannerInfo *root, Path *path,
                          TessScanClauses *clauses);
const TessSource *(*next) (const TessSource *previous);
const TessSourceRegistryOps *
tess_source_registry_check(const TessSourceRegistryOps *ops);
```

`add` SHALL register the source; adding the same pointer again SHALL
change nothing. `find` SHALL return the source registered under a
case-sensitive name, or `NULL`, also for a `NULL` or empty name.
`remove` SHALL unregister that pointer alone; `NULL` and a pointer not
registered SHALL change nothing. `next` SHALL list the sources in the
order they were registered: `next(NULL)` SHALL return the first,
`next(source)` the one registered after it, and `NULL` after the last or
when none is registered. A pointer returned by `find` or `next` SHALL be
borrowed: the caller SHALL NOT modify it or use it after the source is
removed.

Before it lists the sources, Tessera SHALL check the registry's table
with `tess_source_registry_check` of `tessera/runtime_api.h`, which
returns the table when its version is
`TESS_SOURCE_REGISTRY_OPS_ABI_VERSION` and its size is at least
`TESS_SOURCE_REGISTRY_OPS_MIN_SIZE`, and otherwise, also for a `NULL`
table, raises an error, SQLSTATE `0A000`, without reading `next`; a
module that calls `next` itself SHALL check the table the same way.

The registry SHALL refuse with an error: a `NULL` source, SQLSTATE
`XX000`; a version other than `TESS_SOURCE_ABI_VERSION` or a size below
`TESS_SOURCE_MIN_SIZE`, SQLSTATE `0A000`; a `NULL` or empty name,
SQLSTATE `XX000`; another source with a name already registered,
SQLSTATE `42710`; and a pointer given to `next` that is not registered,
SQLSTATE `XX000`.

#### Scenario: Sources found and listed
- **WHEN** the registry is listed while it is empty, two sources are
  registered, the first is added again, the first is removed, and `NULL`
  and a pointer never registered are removed
- **THEN** `next(NULL)` of the empty registry returns `NULL`; while both
  are registered `find` returns each by its name, `NULL` for a `NULL`
  and an empty name, and `next` lists the first and then the second;
  once the first is removed, `find` returns `NULL` for it and `next`
  lists the second alone; the last two removals change nothing
- **Verified by:**
  `test/sql/source.sql::-- spec: source-scan/Sources found and listed`

#### Scenario: Sources refused
- **WHEN** a `NULL` source, a source of another version, one below the
  least size, one without a name, one with an empty name, a second
  source of a registered name, and a pointer never registered given to
  `next` are each tried in turn
- **THEN** each raises its error, with the SQLSTATE listed above, and
  the registry stays as it was
- **Verified by:**
  `test/sql/source.sql::-- spec: source-scan/Sources refused`

#### Scenario: A registry table too old
- **WHEN** `tess_source_registry_check` is given the bridge's table, a
  table that ends at `find`, a table of another version, and `NULL`
- **THEN** it returns the first, and raises an error, SQLSTATE `0A000`,
  for each of the others without reading their `next`
- **Verified by:**
  `test/sql/source.sql::-- spec: source-scan/A registry table too old`

#### Scenario: A source of the least size
- **WHEN** a source whose `struct_size` is `TESS_SOURCE_MIN_SIZE`,
  without `scan_rows`, one whose `scan_rows` is `NULL`, and one of a
  size past the current structure are registered beside a source that
  reads a foreign table
- **THEN** all are found and listed, the first two are offered no
  relation, the third, whose `scan_rows` returns `NULL`, is asked and
  declines, and the plan reads the table through the source that reads
  it
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: source-scan/A source of the least size`

### Requirement: A source's scan callback
Tessera SHALL ask a source to read a relation by calling its `scan_rows`
with the planner's state, a copy of the core's whole scan path of the
relation (for a foreign table, the `ForeignPath` with the wrapper's
private data), and the relation's clauses with the source's answers to
fill (the requirement "A source's answers for the clauses"). The copy's
target SHALL hold the relation's targets and every column the clauses
read. The copy is Tessera's: it lives as long as the planning of the
statement and shares the wrapper's private data with the wrapper's own
path; the source SHALL NOT change it, MAY read it after the call, and
SHALL NOT make it a child of its path. The path the source returns
becomes Tessera's: Tessera may change its target and rows and may free
it, so the source SHALL build a new path at each call and keep no
pointer to it. The source SHALL return `NULL` when it does not read the
relation, or a path built by `tess_path_create` (`tessera/planner.h`)
for a batch node registered in the node registry (Tessera's registry of
the kinds of its plan nodes, of `tessera/node.h`), with these
properties:

- it reads the relation of the given path, with
  `CUSTOMPATH_SUPPORT_PROJECTION` (the flag that says its node computes
  the expressions of its target list itself); Tessera SHALL set its
  target: the offered one under `TessFilter`, the relation's own when it
  stands alone;
- every row it returns passes each clause it answered it checks exactly;
  a row of the relation is left out only when such a clause rejects it,
  or when the source proves that a clause it uses to skip rows rejects
  it; each row returned is returned once, over all the processes that
  read a partial path (below);
- it carries no parameters: it is never the inner side of a nested loop
  that passes it the values of an outer row of the same query;
- it is partial when the given path is: a partial path is one of a
  parallel plan, read by several processes, its participants, each of
  which reads its share; Tessera offers only a parallel-aware one, whose
  participants divide the rows among them, and the returned path SHALL
  be parallel-aware and parallel-safe, with the given path's number of
  workers, and serial for a serial one;
- it is parallel-safe, that is, its node may run whole inside a worker
  of a parallel plan, only where the given path is, and that flag is the
  source's own promise; Tessera's paths over it, alone or under
  `TessFilter`, are parallel-safe only when the source's path is;
- its rows and costs are the source's estimate of returning those rows,
  per participant for a partial path; Tessera uses the rows as the input
  of `TessFilter` only.

The node of the path SHALL show in `EXPLAIN` the clauses it checks
exactly.

A returned path that is a core path or a custom path of another
provider, or one whose relation, parameters, path keys, projection,
parallel awareness, parallel safety or number of workers differ from the
above, SHALL raise an error, SQLSTATE `XX000`: the path keys of the
returned path, a promise of order, SHALL be none, as those of the path
offered. Each path offered, the serial and the partial, SHALL be a call
of its own with answers of its own. For each, Tessera SHALL ask the
sources in the order they were registered, give each the answers set to
`TESS_CLAUSE_UNSUPPORTED`, take the path of the first source that
returns one and ask no later source. When a source returns `NULL`,
Tessera SHALL ignore what it wrote into the answers; when none returns a
path, the relation SHALL keep the core's paths alone.

#### Scenario: The first source that reads the relation
- **WHEN** three sources are registered: the first answers that it
  checks a clause exactly and returns `NULL`, the second returns its
  path and leaves the answers as it got them, and the third would return
  a cheaper path
- **THEN** the plan reads the table through the second source's node,
  with `TessFilter` checking the clause, the third is not asked, and the
  result equals the result over a heap table of the same rows
- **Verified by:**
  `test/sql/source_scan.sql::The first source that reads the relation`

#### Scenario: The path offered
- **WHEN** a query with a `LIMIT`, under which the core keeps a path
  that starts cheaper beside one that costs less in total, reads a table
  whose wrapper offers two such paths, each with private data of its own
- **THEN** the source is given a copy of the one of the least total cost
  with its private data, and its node, built from it, says so in
  `EXPLAIN`
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: source-scan/The path offered`

#### Scenario: A path refused
- **WHEN** a source returns, in turn, a core path, a custom path that
  `tess_path_create` did not build, a batch path of another relation, a
  path with parameters, a path with path keys, a path without
  projection, a serial path where it was given a partial one, a partial
  path where it was given a serial one, a partial path that is not
  parallel-aware, one that is not parallel-safe, one with another number
  of workers, and a serial path marked parallel-safe where the given one
  is not
- **THEN** each raises an error at planning, SQLSTATE `XX000`
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: source-scan/A path refused`

#### Scenario: A source not safe in parallel
- **WHEN** a source marks its serial path not parallel-safe, and a
  parallel plan joins a heap table with the foreign table it reads
- **THEN** neither the source's node nor `TessFilter` over it runs
  inside a worker, and the result equals the result over heap tables of
  the same rows
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: source-scan/A source not safe in parallel`

#### Scenario: No source reads the relation
- **WHEN** every source returns `NULL` for a foreign table
- **THEN** the plan has the core's `ForeignScan`, under `TessPack` where
  a batch parent reads it, as without sources
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: source-scan/No source reads the relation`

### Requirement: A source's answers for the clauses
Tessera SHALL pass a source, with the path, a `TessScanClauses` of
`tessera/source.h`: the relation's clauses in the order the core
evaluates them, whether the source may use each, and an answer for each,
which the source sets. A relation without clauses SHALL be offered with
`nclauses` 0.

```
 TessScanClauses                    Tessera's, valid during the call
 ┌─────────────┬──────────┬───────────────────────┬──────────────┬─────────┐
 │ struct_size │ nclauses │ clauses               │ usable       │ answers │
 │ Size        │ int      │ RestrictInfo *const * │ const bool * │ int32 * │
 └─────────────┴──────────┴───────────────────────┴──────────────┴─────────┘
   clauses[i]:  the clause i, in the core's order; read only
   usable[i]:   whether the source may use clause i; read only
   answers[i]:  TESS_CLAUSE_UNSUPPORTED on entry; the source writes it
   TESS_SCAN_CLAUSES_MIN_SIZE includes answers; a source reads a
   later field only when struct_size includes it
```

An answer SHALL be one of:

- `TESS_CLAUSE_UNSUPPORTED` (0): the source does not use the clause;
  `TessFilter` checks it.
- `TESS_CLAUSE_PRUNE_ONLY` (1): the source may skip rows the clause
  rejects; the clause stays in `TessFilter`, which checks it in the
  core's order on the rows that reach it.
- `TESS_CLAUSE_EXACT` (2): every row the source returns passes the
  clause; `TessFilter` does not check it.

The source SHALL use only the clauses marked usable (the requirement
"The clauses a source may use"). The structure and its arrays SHALL be
valid only during the call. For the clauses it uses, the source SHALL:

- put the expression of each (`RestrictInfo.clause`) into its path's
  expressions (`TessPathConfig.expressions`);
- pass them on, in its `PlanCustomPath`, the callback by which
  PostgreSQL turns a custom path into a plan node, as its plan's
  expressions (`TessPlanConfig.expressions`);
- leave its plan's qualifiers, the conditions a plan node checks on
  each row it returns, empty, whatever clauses `PlanCustomPath` is
  handed: PostgreSQL hands it every clause of the relation, the clauses
  the source leaves and the ones it may not use among them;
- plan its node with the relation as its scan tuple
  (`scan_tuple_is_relation`), the row against which PostgreSQL resolves
  the columns of the node's expressions, so that their columns resolve
  whatever the node's targets.

An answer SHALL hold for every scan and every execution of the plan. A
clause may compare a column with a value that is not a constant of the
plan: a parameter, given from outside the scan, or an expression such as
`now()` or `$1 + 1`, which PostgreSQL does not fold at planning. A
parameter is a value of an outer query's row (a correlated subquery),
which the executor sets and changes between scans; an uncorrelated
subquery's, which PostgreSQL computes once, at the first read of the
parameter (both `PARAM_EXEC`); or a prepared statement's, fixed for one
execution (`PARAM_EXTERN`). A source that uses such a clause SHALL
compute the clause's side without columns when its node is first asked
for rows and again after each rescan (each time the executor starts the
node's scan anew), never when the node is begun. It SHALL compute it
through PostgreSQL's evaluation of expressions in its node's context,
which computes a subquery's value when it is first read or, in a worker
of a parallel plan, reads the value the gathering node sent, and never
by reading the executor's array of parameters itself. It MAY then
compare that value with the form in which it keeps the column. Since the
clause is among the plan's expressions, a change of an executor
parameter starts the node's scan again.

#### Scenario: A source that checks every clause
- **WHEN** a source answers that it checks exactly every clause of a
  foreign table, one of them on a column the query does not select
- **THEN** the plan reads the table through the source's node alone,
  with no `TessFilter` and with the rows of the path offered, and the
  result equals the result over a heap table of the same rows
- **Verified by:**
  `test/sql/source_scan.sql::A source that checks every clause`

#### Scenario: A source that checks some clauses
- **WHEN** a source checks exactly one of two clauses and leaves the
  other
- **THEN** `EXPLAIN` shows the first under the source's node and the
  other under `TessFilter` alone, and the result equals the result over
  a heap table of the same rows
- **Verified by:**
  `test/sql/source_scan.sql::A source that checks some clauses`

#### Scenario: A source that only skips rows
- **WHEN** a source answers that it uses a clause only to skip groups of
  rows whose least and greatest values the clause rejects
- **THEN** `TessFilter` checks the clause and counts as removed only the
  rows it rejects itself, and the result equals the result of the same
  source that skips nothing and the result over a heap table of the same
  rows
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: source-scan/A source that only skips rows`

#### Scenario: A clause on an outer row's value
- **WHEN** a correlated subquery scans a foreign table that a source
  reads, once by a clause on the outer row's value that the source
  checks exactly and once by one that it uses only to skip groups of
  rows
- **THEN** each scan returns the rows of the current outer value, with
  `TessFilter` checking the second clause, and each result equals the
  result over a heap table of the same rows
- **Verified by:**
  `test/sql/source_scan.sql::A clause on an outer row's value`

#### Scenario: A value fixed for the statement
- **WHEN** a statement prepared with a generic plan, one planned once
  for every value of its parameters (`plan_cache_mode =
  force_generic_plan`), reads a foreign table that a source reads by a
  clause on its parameter that the source checks exactly, and is
  executed with two values, once by a clause on the parameter itself and
  once by one on `$1 + 1`. A query reads the table by a clause on
  `now()`, and one by a clause on an uncorrelated scalar subquery's
  value, once checked exactly and once used only to skip groups of rows.
  A plain `EXPLAIN` shows the last query
- **THEN** each execution returns the rows of its own value, each query
  the rows of the subquery's value, the `EXPLAIN` runs no subquery, and
  each result equals the result over a heap table of the same rows
- **Verified by:**
  `test/sql/source_scan.sql::A value fixed for the statement`

### Requirement: The clauses a source may use
A clause has a security level: the clauses of a row security policy (a
condition a table's owner attaches to it, which PostgreSQL adds to every
query of the roles it binds) come at a lower level than the query's own
clauses, and a clause of a higher level may run before one of a lower
level only when it is leakproof as the core marks it
(`RestrictInfo.leakproof`): no function that may reveal its arguments
takes a value of the relation's columns. A clause is volatile when it
calls a function whose result may change from one call to the next, such
as `random()`; PostgreSQL evaluates such a clause in one place, on each
row that reaches it in the order of the clauses.

Tessera SHALL mark a clause usable when, and only when, it calls no
volatile function, holds no subquery that PostgreSQL runs from within
the clause (a `SubPlan`), and is leakproof or of the lowest level among
the relation's clauses: the core's own test of moving a clause ahead
(`restriction_is_securely_promotable`), and its rule of keeping a
volatile clause where it is. A clause on an uncorrelated subquery's
value holds a parameter, not a `SubPlan`, and stays usable. So no source
runs a clause, on rows or on data derived from them, such as their least
and greatest values, ahead of the policy clauses that guard it, and no
source evaluates a volatile clause. The source SHALL answer
`TESS_CLAUSE_UNSUPPORTED` for a clause it may not use. Another answer
for such a clause, a value that is none of the three answers, or a
clause answered `TESS_CLAUSE_PRUNE_ONLY` or `TESS_CLAUSE_EXACT` whose
expression is not among the returned path's expressions
(`tess_path_get_info`), SHALL raise an error, SQLSTATE `XX000`. When the
plan of a source's node under `TessFilter` has qualifiers, or lacks
among its expressions (`custom_exprs`) a clause the source answered
`TESS_CLAUSE_PRUNE_ONLY` or `TESS_CLAUSE_EXACT`, `TessFilter` SHALL
raise an error at planning, SQLSTATE `XX000`. Under no filter, a
source's path alone, no code of Tessera sees the source's plan, and
those duties are the source's own.

A source that checks a clause it uses, at any level, by code of its own
rather than by PostgreSQL's functions, SHALL check it without an error,
a notice or any other message that depends on the values of the rows, or
answer `TESS_CLAUSE_UNSUPPORTED`. The rows it checks may be ones that a
policy or a view's clause left to `TessFilter` hides. A view with
`security_barrier` is one whose own clauses run before any clause of the
query that is not leakproof, and a leakproof clause of the query that
the core pushes into such a view comes to the source at the lowest
level.

#### Scenario: Clauses under a row security policy
- **WHEN** a role that neither owns nor bypasses row security filters a
  partitioned table with a policy, whose partition is a foreign table,
  by a clause that is not leakproof and by one that is
- **THEN** the policy's clause and the leakproof clause are usable, the
  other is not and `TessFilter` checks it after the policy's, and the
  result equals the result over heap partitions of the same rows; over a
  view with `security_barrier` instead, the clause that is not leakproof
  stays above the view's scan and never reaches the source
- **Verified by:**
  `test/sql/source_scan.sql::Clauses under a row security policy`

#### Scenario: A source's own check reveals nothing
- **WHEN** the source checks by code of its own, exactly and to skip
  rows, a leakproof clause over the rows a policy hides, and a leakproof
  clause that the core pushes into a view with `security_barrier` over
  the rows the view's clause hides
- **THEN** no error and no message comes from the source's check, and
  each result equals the result over heap tables of the same rows
- **Verified by:**
  `test/sql/source_scan.sql::A source's own check reveals nothing`

#### Scenario: A volatile clause and a subquery in a clause
- **WHEN** a foreign table that a source reads is filtered, in a full
  scan without a limit, by one clause, which calls a volatile function;
  and, in another query, by a `NOT IN` over a subquery whose column may
  be NULL, so that the subquery stays in the clause
- **THEN** neither clause is usable, `TessFilter` checks each, the
  volatile function is called as many times as over a heap table of the
  same rows, and each result equals the result over a heap table
- **Verified by:**
  `test/sql/source_scan.sql::A volatile clause and a subquery in a clause`

#### Scenario: Answers that are not allowed
- **WHEN** a source answers that it checks exactly, or uses to skip
  rows, a clause marked not usable; gives a value that is none of the
  three answers; answers for a clause whose expression its path does not
  carry; or, under `TessFilter`, plans its node with qualifiers or
  without a clause it answered for among its plan's expressions
- **THEN** planning raises an error, SQLSTATE `XX000`
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: source-scan/Answers that are not allowed`

### Requirement: The relations offered to a source
With the setting `tessera.enable`, the switch of all of Tessera's
planning, on, Tessera's scan planner SHALL offer the sources the core's
scan path of a foreign table when:

- the table is a base relation, a partition, a child of inheritance or a
  branch of a `UNION ALL`;
- the query level is a `SELECT` that locks no rows; a subquery planned
  on its own inside a statement that writes is such a level;
- the relation has no lateral references, references to the columns of
  another relation of the same `FROM`;
- the relation has no clause that holds for every row or for none (a
  pseudoconstant clause).

The path offered SHALL be the foreign scan path of the relation of the
least total cost among those without parameters and without path keys (a
promised order of its rows), from its serial paths and, when the
relation may be read in parallel, from its parallel-aware partial paths.

A batch parent SHALL NOT ask the sources: over the core's foreign scan
path it reads through `TessPack`, and it reads a source's path only when
the planner chose that path for the relation. Tessera SHALL NOT offer a
source a sequential scan of a plain heap table: the heap scan node alone
reads one, with any source registered. Every other relation, statement
and path SHALL keep the plan it has without sources.

#### Scenario: Tessera off
- **WHEN** `tessera.enable` is off and a source reads a foreign table
- **THEN** no source is asked, and the plan has the core's `ForeignScan`
  alone
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: source-scan/Tessera off`

#### Scenario: A statement that locks or writes rows
- **WHEN** a `SELECT … FOR UPDATE`, also one with an `ORDER BY` that a
  batch sort could take; an `INSERT` into a heap table of the rows that
  a plain `SELECT`, which the planner merges into the statement, takes
  from the foreign table; an `UPDATE` of a heap table `FROM` it; and an
  `INSERT … SELECT … GROUP BY`, whose grouping is planned as a query of
  its own, read a foreign table that a source reads
- **THEN** the first three read the table through the core's
  `ForeignScan`, under `LockRows` in the first, with no source's node,
  and the grouping of the last reads it through the source's node
- **Verified by:**
  `test/sql/source_scan.sql::A statement that locks or writes rows`

#### Scenario: Relations and paths not offered
- **WHEN** a foreign table that a source reads is scanned with a lateral
  reference, with a pseudoconstant clause, only through a foreign path
  with path keys, on the inner side of a nested loop through the
  wrapper's parameterized path, or in parallel only through a partial
  path that is not parallel-aware
- **THEN** the paths not offered keep the core's `ForeignScan`: the plan
  is the one without sources in the first three, the inner side is the
  wrapper's in the fourth, and no partial path of a source is in the
  plan in the last
- **Verified by:**
  `test/sql/source_scan.sql::Relations and paths not offered`

#### Scenario: A foreign partition
- **WHEN** a grouping reads a partitioned table that has a heap
  partition and a foreign one that a source reads, a grouping reads a
  heap table whose child of inheritance is that foreign table, and a
  grouping reads a `UNION ALL` of a heap table and that foreign table
- **THEN** each plan reads the heap side through the heap scan node and
  the foreign side through the source's node, under `TessAppend`, and
  each result equals the result over heap tables of the same rows
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: source-scan/A foreign partition`

#### Scenario: Heap tables beside a source
- **WHEN** a source that would take any path is registered and a query
  reads heap tables
- **THEN** the plans and results are those without the source
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: source-scan/Heap tables beside a source`

### Requirement: A source's path alone
When a relation offered has no clauses, or the source checks every one
of them exactly, Tessera SHALL add the source's path to the relation's
paths, serial and partial, with the relation's own target and the rows
of the path offered: the rows the wrapper gave that path, per
participant for a partial one, as every path of the relation returns the
same rows. A batch parent SHALL read it as a batch child, without
`TessPack`, when the planner chose it. A rescan SHALL produce the
relation's rows again.

#### Scenario: Batch parents over a source
- **WHEN** a grouping, a sort with a limit and a hash join read a
  foreign table without clauses, which is offered to the source with no
  clauses, and the source's path costs less than the wrapper's
- **THEN** each plan reads the table through the source's node with no
  `TessPack` between, and each result equals the result over a heap
  table of the same rows
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: source-scan/Batch parents over a source`

#### Scenario: A source read again
- **WHEN** a nested loop scans a foreign table without clauses through
  its source once for each row of an outer relation
- **THEN** every scan produces all the table's rows, and the result
  equals the result over a heap table of the same rows
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: source-scan/A source read again`

### Requirement: The filter over a source
When the source does not check every clause of a relation offered
exactly, Tessera SHALL put `TessFilter` over the source's path, serial
and partial, with the clauses the source does not check exactly. The
filter SHALL evaluate each of them, in the order the core evaluates
them, in batches when Tessera's expression compiler (which turns a
clause into calls of the batch kernels) takes the clause and row by row
otherwise, as it does over the heap scan, and SHALL evaluate no clause
the source checks exactly. The filter SHALL show its clauses in
`EXPLAIN` as over the heap scan; rows a source skipped or removed SHALL
count in no line of the filter.

#### Scenario: Clauses in batches and by rows
- **WHEN** a foreign table that a source reads is filtered by a clause
  the compiler takes and by one it does not, such as a regular
  expression, and the source checks neither
- **THEN** the plan has `TessFilter` over the source's node, the first
  clause as a batch filter and the second as a row filter, and the
  result equals the result over a heap table of the same rows
- **Verified by:**
  `test/sql/source_scan.sql::Clauses in batches and by rows`

### Requirement: A parallel read through a source
Tessera SHALL gather a source's partial path, alone or under
`TessFilter` (the requirements "The relations offered to a source", "A
source's path alone" and "The filter over a source"), with `TessGather`
as it gathers a partial heap scan: with the setting
`tessera.batch_gather`, which lets `TessGather` stand where the core's
`Gather` would, on, when the path is the cheapest partial path of a base
relation and returns at least one column. A partial grouping SHALL read
it as it reads a partial heap scan, and a path that returns no column,
as under `count(*)`, SHALL be read so and gathered above the grouping. A
worker of the plan SHALL get the value of an uncorrelated subquery that
a clause the source uses compares with, from the gathering node.

#### Scenario: Parallel rows through a source
- **WHEN** a parallel plan returns columns of the rows of a foreign
  table that a source reads, filtered by a clause that the source checks
  exactly when offered the serial path and leaves when offered the
  partial one, with `tessera.batch_gather` on and off
- **THEN** the plan has `TessGather` over `TessFilter` over the source's
  partial node, with the rows of the partial path offered, when it is on
  and the core's `Gather` there when it is off, and the rows equal those
  over a heap table of the same rows
- **Verified by:**
  `test/sql/source_scan.sql::Parallel rows through a source`

#### Scenario: A parallel count
- **WHEN** a parallel plan counts the rows of a foreign table that a
  source reads, once with a clause the source leaves and once with every
  clause checked by the source
- **THEN** a partial `TessAgg` reads, in each participant, `TessFilter`
  over the source's partial node in the first and the source's partial
  node alone in the second, `TessGather` stands above it, and each count
  equals the count over a heap table of the same rows
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: source-scan/A parallel count`

#### Scenario: A parallel read by a subquery's value
- **WHEN** a parallel plan reads a foreign table that a source reads by
  a clause on an uncorrelated scalar subquery's value, once checked
  exactly by the source's partial node alone and once used only to skip
  rows under `TessFilter`, with `tessera.batch_gather` on and off
- **THEN** every worker uses the subquery's value, and the rows equal
  those over a heap table of the same rows
- **Verified by:**
  `test/sql/source_scan.sql::A parallel read by a subquery's value`

#### Scenario: A parallel grouping
- **WHEN** a parallel plan groups the rows of a foreign table that a
  source reads
- **THEN** a partial `TessAgg` reads the source's partial node in each
  participant, and the groups equal those over a heap table of the same
  rows
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: source-scan/A parallel grouping`

### Requirement: The cost of a source's paths
Tessera SHALL cost the source's path, added alone, at the cost the
source gave it, with the rows of the requirement "A source's path
alone". It SHALL cost `TessFilter` over it as the core counts the cost
of the clauses the filter checks, a cost at the start and a cost for
each row: the filter starts at the source path's start cost plus the
clauses' start cost, and costs in total the source path's total cost
plus the clauses' start cost plus their cost for one row times the rows
the source's path returns. It SHALL give the filter the rows of the path
offered, as for a path alone. The setting `tessera.scan_cost_factor`,
the share of the core scan's cost at which a heap scan's path starts,
SHALL NOT apply to a source. The planner SHALL choose between these
paths and the core's by cost, as it chooses between any paths.

#### Scenario: A source that costs less than its wrapper
- **WHEN** the source's path, and `TessFilter` over it with the cost of
  the clauses it leaves, cost less than the core's foreign scan at the
  start and in total, with `tessera.scan_cost_factor` at its least and
  at its greatest
- **THEN** the plan reads the table through the source in both, and
  `EXPLAIN` gives `TessFilter` the rows of the path offered
- **Verified by:**
  `test/sql/source_scan.sql::A source that costs less than its wrapper`

#### Scenario: A source that costs more than its wrapper
- **WHEN** the source's path costs more than the core's foreign scan at
  the start and in total, with clauses and without, and with a limit and
  a grouping above
- **THEN** the plan keeps the core's `ForeignScan`, under `TessPack`
  where a batch parent reads it
- **Verified by:**
  `test/sql/source_scan.sql::A source that costs more than its wrapper`

#### Scenario: The clauses left decide
- **WHEN** the source's path alone costs less than the core's foreign
  scan, but the cost of a clause it leaves, for each row it returns,
  makes `TessFilter` over it cost more; and, under a `LIMIT`, the start
  cost of a clause it leaves, such as a `NOT IN` over a hashed subquery
  whose column may be NULL, makes `TessFilter` start later than the
  core's foreign scan
- **THEN** each plan keeps the core's `ForeignScan`
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: source-scan/The clauses left decide`

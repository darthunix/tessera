# source-scan: how it is built

Tessera reads a plain heap table in batches through its heap scan node.
A foreign table, whose rows a foreign data wrapper reads from elsewhere,
such as files, comes out of the wrapper row by row, and a batch parent
packs those rows into batches again. A wrapper that keeps a columnar
format could hand out batches directly and check some conditions on the
form in which it keeps a column, but Tessera's planner had no way to ask
it. A batch source is that way: a registered module that Tessera asks,
while it plans a foreign table, for a batch scan of it, and that says
for each condition whether it checks it.

This page explains the idea and how it is built. The exact rules, the
errors and what EXPLAIN shows are in the capability's spec,
[spec.md](spec.md), next to this page; the guide for an author of a
source is [docs/source.md](../../../docs/source.md).

## Background

A **batch** is a group of rows of a node, kept by column, with a mask
of the rows still taken. A **batch parent** is a node of Tessera that
reads batches from its child, such as `TessAgg`, `TessSort` or
`TessHashJoin`; over a child that returns rows it reads through
`TessPack`, which packs rows into batches. `TessFilter` checks a
relation's clauses on the batches of the scan below it, in batches
where the expression compiler, which turns a clause into calls of the
batch kernels, takes the clause, and row by row otherwise.
`TessGather` gathers the batches of the processes of a parallel plan.

A **path** is one way to read a relation, which PostgreSQL's planner
weighs against the others by its cost. Its **target** is the list of
columns it returns, its **path keys** a promised order of its rows, and
its **parameters** the values of an outer row it needs, as the inner
side of a nested loop. A **partial** path belongs to a parallel plan:
several processes, its **participants**, read it, a **parallel-aware**
one dividing the rows among them, and a path is **parallel-safe** when
its node may run inside a worker. A plan node's **qualifiers** are
conditions the node checks on each row it returns, which `EXPLAIN`
shows as its `Filter`; a **rescan** starts a node's scan anew, as under
a nested loop.

A **clause** is one condition on the relation's rows. A clause has a
**security level**: the clauses of a row security policy come at a
lower level than the query's own, and the core lets a clause run before
one of a lower level only when it is **leakproof**, when no function
that may reveal its arguments takes a value of the relation. A clause
is **volatile** when it calls a function whose result may change from
one call to the next, such as `random()`.

The **bridge** is Tessera's module `tessera`, which holds the
registries. The **node registry** holds the kinds of Tessera's plan
nodes; the **source registry** holds the sources, each a `TessSource`
with a name and a callback, `scan_rows`. A **hashed subquery** is a
subquery of a clause, such as `NOT IN (SELECT …)`, whose rows the core
puts into a hash table once, before the first row, and probes for each
row.

## The problem

1. Which module reads a foreign table in batches, and how does the
   planner find it without knowing the wrapper?
2. A source may check some clauses itself, on the values of a
   dictionary, the distinct values a columnar file stores once for a
   column, or on a group's least and greatest values, and not others. How does
   `TessFilter` learn which ones are left to it, so that no clause is
   checked twice and none is lost?
3. A source runs the clauses it uses before `TessFilter` runs the rest.
   How does that never run a clause ahead of a policy that guards it?
4. A clause may compare a column with a value known only at execution:
   an outer row's, a prepared statement's parameter, a subquery's
   result. How does a source's plan get those values and scan again
   when they change?
5. How are the source's paths priced against the wrapper's, which only
   the source can price?

## Goals and what they cost

- **One way in, for every source** (question 1). The scan planner walks
  the registry in the order of registration and takes the first path
  returned. The price: two sources that read one table cannot share it,
  and the first loaded wins.
- **The answers at planning** (question 2). The source answers for each
  clause when it builds its path, so that the plan, its cost and its
  `EXPLAIN` say which clauses the filter checks. The price: an answer
  holds for every scan and every execution of the plan.
- **The core's own rule of order** (question 3). A clause is usable
  where the core itself would move it ahead of another. The price: a
  source cannot use a clause that is not leakproof above a policy, even
  one it could check safely.
- **The executor's own evaluation** (question 4). The clauses a source
  uses go into its plan's expressions, and the source computes their
  sides through PostgreSQL's evaluation, at the first rows of each scan.
  The price: a source cannot fold such a side at planning.
- **The source's own prices** (question 5). Its path costs what it says;
  Tessera adds only the clauses `TessFilter` keeps.

## The whole in one picture

```
 planning a foreign table t that a source reads

 core      GetForeignPaths ─► ForeignPath       ForeignPath
                              (serial)          (partial)
                                  │                 │
 scan      each source in order: scan_rows(root, a copy, clauses)
 planner                          │                 │
                                  ▼                 ▼
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
and the planner chooses by cost. A parent given the core's
`ForeignScan` packs it. With `tessera.enable` off the plan is the
core's; when no source returns a path, the wrapper's scan stays, packed
under a batch parent as before.

## The walk over the sources

The scan planner's hook on a relation's paths runs after the wrapper
has made its own. For a foreign table of a `SELECT` that locks no rows,
the same conditions the heap scan's relations meet, kept in one
function of the scan planner, it takes the wrapper's foreign path of
least total cost without parameters or path keys, serial and, apart,
partial, and offers each to the sources in turn. It checks the
registry's table first: an older bridge's table has no `next`, and
calling it would read past the table.

A heap table is never offered: the heap scan node keeps reading it,
so no plan of a heap table can move when a source is loaded. No batch
parent asks a source either. The parents call the runtime's helpers
on a path the planner already chose, and a source that lost by cost
would come back under them; the source's path, alone or under the
filter, is already among the relation's paths when the parents are
planned.

## The call and the path returned

The source gets a copy of the wrapper's whole path, its private data
included, so a source that is the wrapper too finds what its own
planning made. The copy's target holds the relation's targets and every
column the clauses read: `TessFilter` needs the columns of the clauses
it keeps, which are not known before the answers. With the path come
the relation's clauses in the core's order of evaluation, whether the
source may use each, and an answer for each to fill.

The path the source returns is a path of an ordinary node of the node
registry, which keeps its own execution, `EXPLAIN` and parallel
callbacks, built by the runtime's path helper so that every node of
Tessera recognizes it as a batch child. Tessera checks its shape before
it uses it; a wrong shape is a fault of the source, never a plan the
planner could choose. The path then becomes Tessera's: Tessera sets its
target and, when it stands alone, its rows, and the planner may free
it.

## The answers and the filter above

A source answers for each clause that it does not use it, that it uses
it only to skip rows that cannot pass, or that it checks it exactly.
`TessFilter` checks every clause but the exact ones, in the core's
order; when the source checks them all, no filter is planned. The
filter's path records the clauses it keeps and those the source
answered for, since its plan is handed every clause of the relation
and keeps only its own.

A source carries the clauses it uses in its path's and its plan's
expressions. That is what makes the planner list the executor's
parameters of the clauses for the source's node, so that a new value
starts its scan again, and lets the planner's last pass resolve their
columns against the relation, whatever the node's targets. The source's
plan keeps no qualifiers: PostgreSQL hands a custom scan every clause
of its relation, those the source leaves and those it may not use
among them, and a node that checked its qualifiers would run them all,
ahead of `TessFilter` and twice. Under
`TessFilter`, which holds its child's plan, Tessera checks both; for a
source's path alone no code of Tessera sees the plan, and the
architecture contract's review covers it.

## The clauses a source may use

Using a clause moves it ahead of the clauses the filter keeps, so
Tessera marks which clauses a source may use by the core's own test of
moving a clause ahead (`restriction_is_securely_promotable`), and the
spec's requirement "The clauses a source may use" holds the rule. It
binds skipping rows too, since the least and greatest values of a group
come from rows a policy may hide. A volatile clause stays out, since
the core runs it in one place on each row that reaches it, and so does
a subquery run from within a clause, which a clause used to skip rows
would put into two plan nodes at once.

The case that tests the rule is a partitioned table with a policy whose
partition is a foreign table: the policy's clause is of the lowest
level, a leakproof clause of the query is usable, and one that is not
leakproof waits for the policy in `TessFilter`. A source's own checks,
by code of its own rather than PostgreSQL's functions, raise no error
and no message that depends on the rows, so that a row a policy hides
is never shown by the source that skipped it.

## Values known at execution

A clause may compare a column with a parameter or with an expression
the core does not fold at planning, such as `now()`. The source
computes that side through PostgreSQL's evaluation of expressions in
its node's context, when the node is first asked for rows and again
after each rescan, never when the node is begun: an uncorrelated
subquery's value is computed when first read, so a plain `EXPLAIN`
runs no subquery, and in a worker of a parallel plan the evaluation
reads the value the gathering node sent. A correlated subquery's
parameter changes between scans, and the executor rescans the node.

## The costs

A source and its wrapper read the same data in two ways that only the
source can price, so the source's path keeps the cost the source gave
it; the share of the core's cost at which the heap scan starts,
`tessera.scan_cost_factor`, calibrates the heap scan alone. The path
alone and `TessFilter` over a source have the rows of the path
offered, since the core assumes that every path of a relation returns
the same rows; under the filter the source's path keeps its own
estimate, the rows the filter reads. `TessFilter` adds the cost of the
clauses it keeps as the core counts them, so that a hashed subquery,
built before the first row, makes the filter start later; the spec's
requirement "The cost of a source's paths" holds the rule.

## Parallel reads

A source's partial path comes from the wrapper's parallel-aware partial
path, whose workers it keeps, so the choice of workers stays the core's.
`TessGather` gathers it as it gathers a partial heap scan, and a partial
grouping reads it in each participant. Its parallel safety is the
source's promise and never more than the offered path's, and Tessera's
paths over it take it from the source's path, so a source that may not
run inside a worker never does.

## What we decided not to do

- **The callback on every node kind.** The node registry would then
  decide which module reads which relation, and the source registry
  would have no use.
- **The source named in the wrapper's options.** Tessera would read
  options of a wrapper it does not own, and every validator would have
  to accept Tessera's option.
- **The wrapper adds its batch path itself.** A path that leaves clauses
  to a filter is not a path of a relation with clauses, and Tessera
  would have to find it among the relation's paths and learn which
  clauses it leaves.
- **The runtime's helpers ask the sources.** A source that lost by cost
  would come back under the parents, and a call that returns a path
  would have to return the clauses left too.
- **The filter checks every clause, the source tells nothing.** A
  source that filters would have its clauses checked again on every
  row it returns.
- **The source checks every clause it is given.** A source that cannot
  check a regular expression would have to refuse the whole relation.
- **The answers at execution.** The plan, its cost and its `EXPLAIN`
  would not say which clauses the filter checks.
- **A pack over the wrapper's scan in the scan planner.** It would have
  to take the clauses the wrapper evaluates out of the wrapper's plan,
  which Tessera does not own; a batch parent still packs the wrapper's
  rows.
- **The heap's rule of costs for a source.** It would price a columnar
  source by its wrapper's rows.
- **A partial path made by the source from a serial one.** The choice
  of workers would move into the source and away from the core's rules.

## Files

- `include/tessera/source.h`: the source, its callback, the clauses
  offered and the answers, and the registry's table with its listing
- `include/tessera/runtime_api.h`, `runtime/api.c`: the check of the
  registry's table
- `bridge/source.c`: the registry and its listing
- `nodes/scan_planner.c`: the walk over the sources, the checks of the
  path and the answers, the path alone, the filter over a source and
  its costs, and the checks of the source's plan
- `docs/source.md`: the guide for an author of a source

## Tests

- `test/sql/source.sql` with `test/tessera_source_test.c`: the
  registry's listing, its refusals and their SQLSTATEs, sources of the
  least size and past it, and the check of the registry's table.
- `test/sql/source_scan.sql` with `test/tessera_scan_source_test.c`:
  the test source, a foreign data wrapper with a batch node and sources
  that read, decline or fail; every plan over a source against the
  same query over heap tables of the same rows, with the answers, the
  security rule, values known at execution, parallel reads, the costs
  and the relations offered.
- `test/installed/Makefile`: the test source built from a copy against
  the installed headers and runtime library.

## Context

Tessera's modules are compiled against one core's headers and run
only in that core. A module built for master does not load in 19, and
the other way round. So supporting 19 means that the same sources
compile against either core's headers, and that the result behaves
the same in each.

A build of the sources against REL_19_STABLE, with the check in
`abi.h` lowered, fails in five places, all where master changed an
interface after 19 branched. Everything else compiles with warnings as
errors, the test modules too.

## Goals / Non-Goals

**Goals:** the modules build with `-Werror` and pass every suite on 19
and on master; master's code stays exactly as it is.

**Non-Goals:** PostgreSQL 18 and older, whose differences reach the
deforming of tuples and the planner; Greengage 7; a shared layer of
version shims for them.

## Decisions

### A branch where the difference is

Each of the five places reads `#if PG_VERSION_NUM >= 200000` with
master's code first and 19's after it, in the file that uses it. There
is no `compat.h` yet. Each difference has one user, so a header would
only move the branch away from the code that needs it. A header of
aliases, master's names defined over 19's, was refused: a reader of
`sort.c` would see master's names and get 19's functions, and two of
the five places are not names at all (a missing comparator, a missing
function). A shared header becomes worth it when one difference has
two users, which the next core or Greengage will bring.

### The index-only scan of 19

Master reads the next row of an index scan through
`table_index_getnext_slot`, which also decides whether an index-only
scan must visit the table. 19 has no such call: its `IndexOnlyNext`
walks the index's entries, checks the visibility map, fetches the
table's row where a page is not all visible, and fills the slot from
the index tuple. On 19 the node keeps that loop, about 60 lines of
19's code, in one function beside the batch builder. Two details
follow master rather than 19:

- The page's predicate lock is taken before the node rechecks a lossy
  index's condition, as master's table access method takes it.
- The memory of name columns, which 19 copies out of btree's cstrings
  for each row, is reset before each row, as `ExecScan` resets it; the
  batch builder has copied the values by then.

A lookup, which reads a plain index scan, uses 19's
`index_getnext_slot` in place of `table_index_getnext_slot`.

The loop has branches that only 19 runs, so the suite `index` gains
the scans that reach them, on both cores: an index-only scan of a GiST
index, whose rows come as heap tuples (`xs_hitup`); one of a btree
index over a `name` column, which btree stores as a C string; and a
parallel index-only scan over pages the visibility map does not show
all visible.

The other way, calling the child's own `ExecProcNode` row by row on
19, was refused: it runs the child's quals and projection, which the
node's batch path does not want, and it would make 19's path differ
from master's in behavior and not only in code.

### Expected outputs

On 19, 44 suites of 45 give master's output. The one that does not,
`union`, gives the same rows but other plans in one section: the
grouping of a UNION by the workers under `TessGather`. Master
estimates a UNION's rows as the sum of its branches' distinct groups;
19 estimates them as all the rows of its branches. The node takes the
core's estimate for its groups, so on 19 a participant's grouping
seems to save nothing, and the serial plan is cheaper. The node is
right by its inputs; the inputs differ.

The section moves to a suite of its own, `union_parallel`, and that
suite gets `test/expected/union_parallel_1.out`, which pg_regress
accepts in place of the first. pg_regress accepts either file on
either core, so the suite's first query prints whether the core is
master: each file then matches one core only, and a master that fell
back to the serial plan fails. A second file of the whole `union`
suite, 2300 lines, was refused: every later edit of the suite would
need a run on 19 to renew it, where only edits of the section need it
now. Estimating a UNION's groups on 19 as master does, in the node's own
planner, was refused: it would copy a fix of the core's planner for one
core, and the node's path would count other rows than the core's
relation it belongs to. On 19 the node then groups a UNION serially
where master's workers would, with the same rows; `docs/limitations.md`
says so.

Bending the section's settings until 19 also chooses the parallel
plan was refused too: the estimate, not a cost, decides it. The one
setting tried, `parallel_tuple_cost = 0`, made a plan of the core's
win on 19 rather than the node's.

A difference in Tessera's own results, or in a choice that the core's
inputs do not explain, is a defect, not a second file.

### TOAST pointers in the Rust check

The check of a chunk of columns read back, in Rust, sizes a TOAST
pointer by master's two tags, 18 and 4. On 19 tag 4 is no pointer. A
damaged chunk that holds one passes the check, and 19's core reads it
as 2 bytes, inside the 22 the check found in the chunk: no read leaves
the chunk, which is what the check promises. The comment of the Rust
function says so; the check itself does not change, so the kernels'
entry point stays as it is.

### CI

`.github/postgres.env` pins a revision of REL_19_STABLE beside
master's, and the PostgreSQL job gains one row for it: Linux x86-64,
the target platform, with the release kernels. The random queries of
`tessera-crosscheck` run on that row too, and their findings go up
under a name of each core, `crosscheck-findings-<core>`, as two
uploads of one name fail. macOS and the debug
kernels stay on master: they check the code paths, which are the same
on both cores but for the five places. Moving the pinned revision is a
commit of its own, as for master.

## Risks / Trade-offs

- 19's minor releases may fix `IndexOnlyNext`; the copy does not
  follow by itself. The comment above the copy names its source, so a
  reader of a minor release's notes can find it.
- Every new use of an interface master changed after 19 branched now
  breaks the 19 row of CI. That is the point of the row, and its cost.
- The sanitizers run against master only, so the loop of 19 never runs
  under them. It is 19's own code with three changes, and the suites
  run it under assertions.

## Measurement

None on speed: master's code does not change. Shown instead by the
object code: the four changed object files (`spill.o`, `heapscan.o`,
`sort.o`, `tessera_spill_test.o`) built against master before and
after the change are compared with `objdump -d`. Their instructions
are the same; only the line numbers that `elog` and `Assert` pass
differ.

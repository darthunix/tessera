## Why

Under a `Gather`, `TessAgg` groups the rows of each participant in a
table of its own (the partial stage), and a second `TessAgg` above
`TessGather` merges their groups (the final stage). The planner offers
the partial stage only when every key of the grouping fits a 64-bit
word: integers, dates, timestamps, booleans. A key of text, `char(n)`,
numeric or any other type that goes through a dictionary (a table that
gives each value a number, by the type's hash and equality) leaves the
partial stage to the core's `Partial HashAggregate`, or the whole
grouping serial.

The rule's own comment gives its reason: a partial table empties early,
and the dictionaries of its keys would go with it. The executor has
settled that since sum states went up early. A key through a dictionary
marks the grouping as one whose records cannot spill, and in the partial
stage such a table goes up whole every time it fills; its dictionary
starts anew with the next table, after the batch that carried the groups
up was released. The planner refuses a path the executor runs, but for
two faults that the reviews of this change found (below).

The rule costs most where grouping is always two-stage. On Greengage 7,
whose segments group their own rows and a motion brings the groups
together, TPC-H Q1 groups by two `char(1)` keys: with the rule the
core's partial stage takes 550 of its 700 ms, and Tessera saves
nothing (0.98 of the core's time); without it 0.41. On PostgreSQL
master the rule keeps parallel `GROUP BY`, `DISTINCT` and `UNION` by
text or numeric off the node's partial stack.

## What Changes

- The planner offers the node's partial grouping for keys of any type
  the serial node takes, in the three places that build it: the stack
  with aggregates under a `Gather`, the stack without them (`GROUP BY`
  without aggregates and `DISTINCT`), and `UNION`.
- Plans change, results do not: a parallel grouping by text or numeric
  shows `TessAgg` in `Partial Mode: Partial` under `TessGather` where
  the core's `Partial HashAggregate` or a serial plan stood.
- Two faults of the executor that the new plans reach, which two
  outside reviews found: each dictionary took room for the groups
  expected up to a quarter of `hash_mem`, so three or four keys and
  the empty table could pass the limit before a row was read, and a
  partial table then went up empty, again and again, forever. The
  dictionaries of a node's keys now start with one quarter together,
  and a partial table goes up only with a group in it.
- Tests in `test/sql/parallel.sql` and `test/sql/union.sql`, compared
  with Tessera off: text with NULL, numeric of two scales, `char(n)`
  and `bpchar` of other lengths, four keys at the least memory, three
  dictionaries sharing their room, tables that hold many batches and
  that fold almost nothing, a rescan, the core's `Gather` and Finalize,
  a nondeterministic collation, `UNION` sent up early.
- `docs/nodes.md` and `docs/spill.md` say how a dictionary key behaves
  in the partial stage.

Not in this change: a comparison of `char(n)` by bytes in place of the
type's functions, which would make Q1's keys cheaper on both cores; it
was not tried.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None. `TessAgg` has no capability yet; `docs/nodes.md` describes it.
The change declares `skip_specs: true`.

## Impact

- Code: `nodes/agg_planner.c`, a function and three conditions removed;
  `nodes/agg_group.c` and `nodes/agg_keydict.c`, the room of the
  dictionaries and when a table is too full.
- Tests: `test/sql/parallel.sql`, `test/sql/union.sql`.
- Documents: `docs/nodes.md`, `docs/spill.md`.
- Runs: the family `anykey` and TPC-H with two workers against
  `main`.
- No format, setting, header or public API changes.

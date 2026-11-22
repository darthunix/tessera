## Context

See proposal.md, "Why", for the motivation. This section gives the
state of the code that shapes the design.

```
  a batch of rows
        │
        ▼
  the table of groups (nodes/agg_group.c)       every row finds or
    keys → the group's record, its states       makes its group
        │ the record's reference, a row each
        ▼
  the DISTINCT set (nodes/agg_keydict.c)        one per aggregate
    (group keys, value) → no payload            a row passes if it
        │ the rows that added a pair            added a pair
        ▼
  the aggregate's step on those rows: count, sum, avg, min, max
```

- **The set.** Each DISTINCT aggregate of one argument has a table of
  its own without payload, a set. It is keyed by the group's keys and
  the argument, the argument alone without `GROUP BY`. A row goes into
  the aggregate only where it inserted its pair; a NULL argument drops
  out at the hash. An argument a word holds (`smallint`, `integer`,
  `bigint`, `date`, `boolean`, `timestamp`, `timestamptz`) is its word.
  Another goes by its number in a dictionary of the set's own values.
- **No spill with DISTINCT.** A grouping spills in two ways. The
  node's own aggregates send whole partitions of groups to disk and
  merge them back. Generic aggregates (states through PostgreSQL's
  functions) freeze the table and write the rows of new groups to disk,
  to be replayed into tables of their own. Both are off when the query
  has a DISTINCT aggregate (`has_distinct`): the groups stay, and the
  set grows past `hash_mem` if it must.
- **The gates.** So the planner checks two estimates before it offers
  the node's path. `distinct_fits` asks that the pairs of every
  DISTINCT aggregate fit `hash_mem`. `generic_fits`, beside a generic
  aggregate, asks that the groups with their states fit. It takes the
  count of groups from the core's hashed grouping paths, and the core
  makes none when an aggregate has DISTINCT (its `can_hash` needs no
  ordered aggregate), so it returns false. The design text of the node
  in `docs/nodes.md` says the planner's estimate of the groups.
- **The machine of rows.** `relspill`'s `Rows` decides the frozen
  table's spill for generic aggregates: after each batch it weighs the
  table (`Grown`), and past `fill` eighths of the limit, with `least`
  records at least, it freezes the table and plans a level of fixed
  partitions (`Freeze`). Once the input is read, it hands the host the
  level's partitions one by one (`Next` → `Read`), and each partition
  read back gets a machine of its own that may freeze it into a level
  below, while hash bits are left. A table of the last level never
  freezes, and neither does one of fewer than `least` records. The
  shape of a level says what each partition keeps, and `share` how
  many takers divide the limit among their chunks. A property test
  checks that the counts through frozen tables equal a grouping in
  memory.
- **A record's reference.** The table of groups gives each row the
  reference of its group's record, 32 bits: the chunk and the byte in
  it. A chunk never moves while the table lives, and the groups of such
  a query are never sent to disk, so a reference names its group until
  the groups go out.

## Goals / Non-Goals

**Goals:**

- A grouping with DISTINCT aggregates of word arguments runs in the
  node whatever its pairs, while its groups and their states fit.
- Each pair counts once in its group, at any limit, and the rows are
  the core's.
- A grouping whose pairs fit by the estimate, and whose memory stays
  within seven eighths of `hash_mem`, costs no more than today.

**Non-Goals:**

- The sets of arguments that a dictionary numbers, and the set of an
  aggregate without `GROUP BY`. Both keep the gate of the pairs.
- The groups of such a grouping going to disk; a parallel form.

## Decisions

### The set keyed by the group's reference

- **Was:** the set's key is the group's keys followed by the argument,
  up to `TESS_TABLE_MAX_KEYS` - 1 keys and the argument.
- **Will be:** with `GROUP BY`, the set's key is the reference of the
  group's record, as an `integer` word, and the argument. The pair's
  hash stays the group's hash folded with the argument's.
- **Why:** a pair written to disk then carries two words and its hash,
  whatever the keys, and goes back to its group by its reference, with
  no probe of the table of groups and no key dictionary. The set's
  records shrink for a grouping of several keys. The table of groups
  already gives the reference of each row.
- **Refused:** writing the group's keys with the pair and probing the
  table of groups when it is read back. A key a dictionary numbers
  would need its number again, and a probe a row costs what the
  reference saves.

### The frozen set and its pairs on disk

```
  input phase                       after the input
  ───────────                       ───────────────
  row → group reference r           for each set with a level:
      → pair (r, v), hash h           for each partition (Next → Read):
        in the set?  ─ yes ─ drop       an empty set, its own machine
             │ no                       a pair (r, v, h) read back
        set frozen? ─ no ─ insert,        new in the set → the step of
             │              step          the aggregate at r
             yes                          in it → drop
             │                          past the limit → frozen, the
        write (r, v, h) to                rest to a level below
        partition of h's bits         then the groups go out
```

- **Was:** the set grows past `hash_mem`.
- **Will be:** each set has a machine of `Rows`, with the node's limit,
  `fill` (seven eighths) and `least` (64). After each batch it is told
  the node's memory and the set's pairs (`Grown`). The node's memory is
  what Memory Usage counts: every set, the writers and readers, and the
  groups by their bytes. Where the planner gave the node the grouping
  although its pairs do not fit by the estimate (below), the machine
  weighs the groups otherwise while the input is read: by the most of
  their bytes and of the bytes the planner estimated for them, so that a
  set that freezes early keeps room for the groups still to come. Once
  the input is read no group is to come, and they weigh their bytes
  alone. Where the pairs fit by the estimate, the planner passes no
  estimate of the groups, and a set freezes only when the memory held
  passes seven eighths: a grouping whose groups the planner thinks more
  than they are runs as today. On `Freeze` the set takes no new pair: a
  row is looked up without insert, a row found drops as today, and a row
  not found writes its reference, its value and its pair's hash to the
  partition its hash names. The writers are chunks of columns, as the
  rows' spill writes, of three words a pair and no values. The level's
  shape has the count of the node's sets as its `share`, so the writers
  of sets that freeze together divide the room between them.
- **The room of the sets.** The groups never leave memory, so the sets
  have what the groups leave. The planner gives the node a grouping
  whose pairs may not fit only when its groups take at most half of
  `hash_mem` by the estimate (below), so a set has three eighths of it
  at least, less the writers and the reader, before it freezes.
- **Reading back.** Once the input is read, every set is emptied: its
  pairs in memory have counted already. Then, before the groups go out,
  the levels are read back one set at a time, the last level written
  first, as the machine says: for each partition the set is emptied,
  the pairs are inserted under their stored hash, and the rows that add
  a pair go through the aggregate's step at their reference. A
  partition past the room freezes into the level below.
- **Where the bound ends.** The bound holds where the planner gave the
  node a grouping whose pairs do not fit by the estimate, and its groups
  are no more than the estimate, so at most half of `hash_mem`. A batch
  is weighed after it is in, so one batch can pass seven eighths. A set
  of fewer than 64 pairs and a set of the last level, with no bit of
  the hash left, never freeze and may pass `hash_mem`, as a frozen
  table's rows do. Where the pairs fit by the estimate, the groups may
  take nearly all of `hash_mem`, a set a dictionary numbers never
  freezes, and groups that come after a set froze still grow: such a
  grouping may pass `hash_mem`, as it does today. So may the groups
  where there are more than the estimate said. Then the sets have no
  room: if at the end of the input the groups' bytes alone pass seven
  eighths of `hash_mem`, the sets read back are read as of the last
  level (the host sets `last` of the machine's `Table`), and grow past
  `hash_mem` rather than freeze at 64 pairs and rewrite every pair a
  level at a time.
- **The end.** A level's files are temporary files of the node's
  spill: a rescan closes every level of every set with the groups, as
  it closes the rows' levels; the node's end closes them; on an error
  PostgreSQL closes its temporary files with the transaction. A cursor
  that stops early has read the pairs back already, since they are read
  before the first group goes out.
- **Why:** each distinct pair is in memory or in the one partition its
  hash names, never both. Once the set is frozen no pair enters it, so a
  pair the set lacks was never counted in memory. Its copies on disk
  share its hash, so they meet in one partition, and the set there
  counts the first. `count`, `sum`, `avg`, `min` and `max` of integers
  give the same result whatever the order of the pairs, which is why
  only they take DISTINCT in the node. The machine is the one the
  frozen table already uses, with its test; the property it needs is
  the same, with pairs for groups.
- **Refused:** a set split into partitions with the largest sent to
  disk, as the node's own groups do. It writes fewer copies of a pair
  seen often, but it sends records, not rows, and needs a merge of
  sets; the freeze needs neither. If the copies cost, the measurement
  will show it.

### The gates of the planner

- **Was:** the pairs must fit (`distinct_fits`), and beside a generic
  aggregate the groups with their states must fit (`generic_fits`, by
  the core's hashed paths, so never).
- **Will be:** the groups' estimate is the most `numGroups` of the
  core's grouped paths in the relation, the groups before HAVING. The
  relation's own rows are set only after the hook, and the core's
  sorted paths are there beside a DISTINCT aggregate. A grouping with a
  DISTINCT aggregate is offered when its groups with their states fit
  `hash_mem`, and either its pairs fit too, as today, or it has `GROUP
  BY`, every DISTINCT argument is a word, and its groups take at most
  half of `hash_mem`. On that second way alone the node gets the
  estimate of the groups' bytes, for the machines of its sets. The cost
  of a grouping adds the pairs' spill as it adds the rows' spill: the
  rows whose pairs pass the room, seven eighths of the limit less the
  groups, written and read once a level, copies of a pair included.
- **Why:** the groups are what still cannot spill, and the half keeps
  room for the sets. The estimate before HAVING is what the table
  holds: HAVING drops groups only as they go out. A grouping whose
  pairs fit keeps its gate of the pairs. The gate of the groups is new
  for a grouping without a generic aggregate: one whose groups with
  their states do not fit, or that the planner thinks do not fit,
  though its pairs do, goes to the core, where today it runs in the
  node, past `hash_mem` if its groups are as many as thought, since
  they cannot spill.
- **Refused:** half of `hash_mem` for the groups of every grouping with
  DISTINCT. A grouping by a key nearly unique, whose pairs fit, would go
  to the core where the node takes it today.

### What EXPLAIN shows

- `Batches` and `Disk Usage` count the partitions and the bytes of the
  pairs with the others. VERBOSE shows `Spilled Pairs`, the pairs
  written, beside `Spilled Rows`. Memory Usage counts the sets, the
  writers of their level and the readers, as the spec of table-memory
  says.

## What we do not do

- The sets a dictionary numbers. The dictionary grows with each new
  value, also of a pair written to disk, so a frozen set would need a
  frozen dictionary too: a look-up of each value without insert, and
  values carried to disk. That is the next step if a query needs it.
- The set without `GROUP BY`. Its pairs are values with no group, and
  the step after reading back goes into the one result. It is the same
  machine with another host; it is left until a query needs it, as
  `count(DISTINCT)` over all rows runs in the node already.
- A grouping with DISTINCT whose groups spill, and a partial form of it
  for parallel plans: each needs the sets to follow their groups to
  disk.

## Risks / Trade-offs

- [The copies of a pair seen often after the freeze are written each
  time] → the measurement counts the disk of each case; the refused
  split is the remedy.
- [The set keyed by reference touches the path that fits] → the counts
  of the in-memory case against `main`, within 1 % of instructions.
- [A reference outlives its group] → the groups of such a query never
  leave memory and the sets are emptied with the table; an assertion
  checks that a reference read back falls in a chunk of the table.
- [The planner thinks the groups more than they are, as it often
  does for several keys] → where the pairs fit by the estimate, the
  machines of the sets are not told the estimate, so a set freezes as
  today; an estimate past `hash_mem` sends the grouping to the core,
  where the node takes it today, the price of the gate of the groups.
  Where the pairs do not fit, the sets have less room and freeze
  earlier, in a grouping that goes to the core today. The measurement
  has a case of it.
- [The planner thinks the groups fewer than they are] → groups past
  the estimate stay in memory past `hash_mem`, as today; the sets then
  read back without a freeze, past it too, rather than level after
  level of 64 pairs.

## Measurement plan

Rules of docs/measuring.md: a release build, an idle machine on the
maintainer's word, five minutes a run at most.

1. **The family `spill` of `bench/pg`**, new cases, Tessera on against
   the core at `work_mem` 4, 16 and 64 MB:
   - `count(DISTINCT v)` of a `bigint` by an `integer` of 1000 groups,
     whose pairs pass `hash_mem` at 4 and 16 MB and fit at 64 MB;
   - its twin with `sum` and `avg` of a `smallint` beside it;
   - two DISTINCT sets of one grouping, which freeze together;
   - groups near half of `hash_mem` at 16 MB, the least room the gate
     leaves the sets, their rows in the order of the key, so that most
     groups come after the sets froze;
   - the first case by two keys, whose pairs shrink most when the set
     is keyed by the group's reference;
   - a grouping by two keys that depend on each other, whose groups the
     planner thinks 12 MB while they are few, and whose pairs fit, at
     16 and 64 MB alone, since at 4 MB its plan is the core's: it
     writes no pair.
   Threshold: a ratio below 1 in every case at every `work_mem`. The
   disk each case writes is kept with its plans, the copies of pairs
   seen often among them.
2. **The path that fits**, by `cargo ab measure --counters` against
   `main`, with task 4.1 and again at the end: the new cases at 64 MB,
   where their pairs fit, and the case of groups thought more than they
   are at 16 MB. Threshold: instructions within 1 %, or fewer.
3. **ClickBench Q9 and Q10** by `cargo clickbench`, on a local build of
   the branch of `parquet-fdw` merged with this one, never pushed: the
   ten files and `hits.parquet`, at 0 and 8 workers, Tessera on, off
   and pg_duckdb. Thresholds: Tessera on beats off on Q9 and Q10 over
   both at 0 and 8 workers, Q10 over the ten files at 8 workers among
   them, the criterion that stage 2 missed. Expected over
   `hits.parquet`: on at least twice as fast as stage 2's 20.3 and
   22.6 s serially, since the core sorted every row there; DuckDB
   still faster.

## Decisions of the maintainer

- 2026-11-22: fix both causes of Q10 going to the core, the gate of
  the groups and the set that does not spill, in a change of its own
  off `main`.
- 2026-11-22: the estimate of the groups weighs in the freeze of a set
  only where the planner gave the node a grouping whose pairs do not
  fit by the estimate; where they fit, a set freezes by the memory the
  node holds, so that a grouping whose groups the planner thinks more
  than they are runs as today.

## Open Questions

- The number of partitions of a level of pairs: the rows' spill fixes
  it (`ROWS_PARTS`). Whether pairs, which are smaller than rows, want
  more partitions is a question for the measurement, not the design.

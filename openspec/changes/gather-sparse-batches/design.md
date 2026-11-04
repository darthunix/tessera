## Context

A batch node reads its child through `TessInput` (`runtime/input.c`):
`tess_input_next` returns the batch the child published, and
`tess_input_finish` marks it consumed, which lets the child release it
(a heap batch drops its pins) and reuse its storage. A batch selects its
rows with a mask; a filter forwards its child's batch with fewer rows
selected, so after a selective filter a batch of a page's 50 or 60 rows
may select one.

Columns are read lazily. A consumer asks a batch for a column on a mask
of rows, and the producer computes or deforms it for those rows only:
the heap batch deforms the rows asked for, a projection's batch
evaluates its expressions on them (`runtime/project.c`). The hash join
reads its outer side's output columns on the rows that found a pair,
and the aggregate reads an argument after its `FILTER`.

A consumer pays a fixed cost a batch besides its cost a row. Measured
on TPC-H Q14 with its range of dates widened, so that the same nodes
get batches from 1.8 rows to 53:

| range | rows | batches | join, ms | aggregate, ms |
|---|---|---|---|---|
| 1 month | 76 000 | 41 000 | 21 | 25 |
| 1 year | 756 000 | 98 000 | 37 | 85 |
| 7 years | 6 000 000 | 113 000 | 66 | 237 |

The aggregate costs about 27 ns a row and 0.6 µs a batch, the join
about 0.5 µs a batch; at Q14's own range nearly all their 45 ms is the
cost of the batches. The index lookup of TessHashJoin pays a scan and
its descents a batch, so with one or two keys a batch its cost model
gives the join to the core's nested loop (`docs/costs.md`, "The index
join").

The maintainer widened the work on 2026-11-04, after this measurement,
from gathering inside the index lookup to gathering by a consumer's
input, and asked for the design to be checked by two reviews before
his approval. On the same day, after the second review, he decided
the two questions no rule settled: reading ahead past an early stop,
and volatile functions (both in "Reading ahead, and volatile
functions" below).

## Goals / Non-Goals

**Goals:**

- A consumer that asks gets full batches from a sparse child, where
  reading the child's columns evaluates nothing.
- A batch full enough already goes as it is, without a copy.
- The same rows, in the same order; reading a column for a copy
  evaluates no expression.
- A join right under a limit, and a consumer that stops after its
  first batch, read no further than now.
- A numeric column keeps its decimals.

**Non-Goals:**

- Gathering for a consumer that forwards its child's batch to its own
  parent, such as TessFilter: a gathered batch has no slot to forward.
- Gathering in TessSort, TessAppend or TessGather: later, each by its
  own measurement.
- Gathering a batch whose columns are computed on request.
- A node of its own that gathers; SEMI and ANTI lookups
  (`semi-anti-lookups`).

## Decisions

### The input gathers, at its consumer's request

**Was:** `tess_input_next` returns the child's batch as it comes.

**Will be:** a consumer calls `tess_input_set_gather(input, true)`
after it sets its request and before a scan's first batch. From the
scan's second batch on,
`tess_input_next` returns the child's batch as before, or a batch the
input owns, filled with copies of the selected rows of several child
batches:

```
child:  [2]    [1]   [3]   [20]  [30]  [25]         [40]   [1]   end
        first  copy  copy  copy  copy  10 copied,   as is  copy
        as is                          15 wait
input:  [2]    [1 + 3 + 20 + 30 + 10 = 64, full]  [15]  [40]  [1]
```

A number is a batch's selected rows. The rules, with a capacity of 64
rows, or the `max_batch_rows` of the consumer's request to its child
where that is smaller:

- The first batch after the start or a rescan goes as it is: a
  consumer that stops after its first batch reads no further than
  now.
- A child batch with at least half the capacity selected goes as it
  is, and the rows gathered before it go first. Such a batch is never
  copied.
- Any other child batch has its selected rows copied, in order, up to
  the capacity. A batch that does not fit gives the rows that fit; its
  copied rows leave its mask, and its rest waits, unfinished, to fill
  the next gathered batch.
- A child batch whose columns are not stored values (below) goes as it
  is, and the rows gathered before it go first.
- At the child's end the gathered rows go.

A gathered batch is therefore full, unless a batch that goes as it is,
or the child's end, comes after it.

**Refused:**

- A node of its own between child and consumer. It would need a place
  chosen by the planner, a cost for every kind of parent, and the
  machinery of a node (EXPLAIN, workers, rescans), for what one
  function of the input does at its consumer's request.
- Gathering in each producer: a filter, a join and a scan would each
  copy, each with its own code.
- Gathered batches that end where a child batch does not fit: batches
  of 22 rows would gather 44 a batch, and the cost model would not know
  the count.
- Copying a dense batch too while rows are gathered, so as to fill the
  gathered batch: batches of 31 and 33 rows in turn would give one
  batch a pair in place of two, but the copy would reach dense batches,
  and the count of batches would hang on the order of dense and sparse
  ones, which the planner does not know.

### What may be gathered: stored values only

**Was:** a batch's columns are read lazily, whatever computes them.

**Will be:** `TessBatch` gains `values_stored`, a field at its end:
true when reading a column of the batch evaluates no expression, so
that it can fail only on a damaged value and has no side effect. A
batch of a size without the field counts as false, and the input
gathers only batches with the field true. A producer sets the field on
each batch it publishes, by what that batch's getter reads:

- True: the heap batch; the builder's batches (TessPack) and the
  index-only scan's batches over them; TessAgg's output and the groups
  read back from its spill; TessSort's output; the batches TessGather
  receives from workers, and its merge's output.
- The hash join. A round's batch reads its outer columns from the
  outer batch on request and takes the outer batch's flag; its inner
  columns are records of the join's table. The compact output and the
  final compact output hold copies, the tails of an outer join have no
  outer columns, and the batches read back from the spill are stored:
  all true. The flag goes with the batch, so a change from rounds to
  compact output changes it.
- The child's flag, batch by batch: the leader's own rows in
  TessGather, which it hands on as its child's batch, and TessAppend's
  batches. TessFilter, TessLimit and TessPack's mode of passing on
  hand on the child's batch itself, flag and all.
- False: a projection's batch, whatever its child gives.

**Refused:**

- Copying any batch. An eager copy would evaluate a projection's
  expressions on rows the consumer drops: a hash join's rows without a
  pair, an aggregate's rows its `FILTER` rejects. An expression that
  raises an error, or calls a volatile function, would then do so where
  it does not now.
- Gathering heap batches only. It would leave Q14's aggregate, which
  reads the join's output, and the outer sides of Q7 and Q21, which are
  joins' outputs.

### What a gathered batch holds

**Was:** nothing; a consumer reads the child's columns.

**Will be:** the columns of the consumer's request, filter and
projection columns both, for the rows gathered: a Datum and a NULL flag
each, read through the child batch's `get_datum_column` for its
selected rows. Copying a stored value evaluates nothing; deforming it
eagerly costs a heap batch's work for rows the consumer may drop, which
the measurement weighs. The input keeps a copy of the request's sets of
columns, which the request only lends. A value by reference is copied
into a memory context of the gathered batch, reset when the next one
starts; its length comes from the child's result type, found through
the layout's map of targets to columns, as the hash join's spill
already finds it. A requested column that is no target of the child is
an error of `tess_input_set_gather`. A column outside the request is an
error of the gathered batch.

A numeric column is copied as numerics. Asked for decimals
(`accept_decimals`), the gathered batch reads them from its numerics as
the heap batch does, once a batch and for the rows asked: the heap
batch's `decimal_column` becomes a runtime function over a column of
Datums that both call. Without the kernels' module it gives no
decimals, as now.

A gathered batch selects all its rows and sets `values_stored`. Its
`table_oid` is its child batches' if they share one, `InvalidOid`
otherwise. It has no release: the input owns its storage.

**Refused:** copying a child's decimals. A child batch fixes its
decimals' scale at its first decimal, and the batches gathered would
bring several scales into one column.

### The contract

- `tess_input_slot` returns NULL while gathering is on, so a consumer
  that forwards its child's slot cannot gather.
- `tess_input_set_gather(input, on)` turns gathering on or off for the
  scans to come, a rescan keeping the choice. It is called before a
  scan's first batch, at the start or right after a rescan; inside a
  scan it is an error.
- `tess_input_finish` and `tess_input_finished` act on the gathered
  batch as on a child's.
- `tess_input_rescan` drops the gathered rows and forgets the waiting
  child batch, which the child's rescan voids; the input drops what it
  holds itself, where a node otherwise finishes its input first.
- A key filter the consumer hands to the child through the input goes
  to the child as before; a batch that waits keeps the rows it had,
  which the consumer's table still probes exactly.
- A tuple bound handed to the consumer turns its gathering off:
  TessHashJoin gains `set_tuple_bound`, which keeps the bound for
  itself, for its outer side may need more rows than its output. At
  the start of each scan the join sets its input's gathering from the
  bound it then holds: on without one (-1), off with one. A limit by a
  parameter, which a rescan computes again, so turns it off and on in
  turn.
- `tess_input_gathered_rows` gives the rows the input copied over its
  life, rescans included.
- Each participant of a parallel plan gathers its own rows.

### Reading ahead, and volatile functions

**Was:** a consumer reads its child a batch at a time. A batch node
already evaluates a batch's 64 rows at once, past a limit too
(`docs/limitations.md`, "Errors of rows the core's plan does not
evaluate"), and the index lookup looks a batch's keys up in one scan
of its inner side.

**Will be:** a gathering input reads its child until a gathered batch
fills, which after a selective filter may take many child batches. The
maintainer decided on 2026-11-04 that its two effects are limitations
of the same kind as the batch's own, written into
`docs/limitations.md`:

- Reading ahead past an early stop. A join under `EXISTS` may drop
  several batches before its first pair, and its input then reads
  further than now. A limit reaches only the node right under it, so a
  join or a filter below that one does not learn of it. A data error
  (an overflow, a division by zero) in a row the core's plan never
  reaches can then stop the query. The guards stay where they cost
  nothing: the first batch after a start or a rescan goes as it is,
  and a tuple bound turns TessHashJoin's gathering off.
- The count and the order of a volatile function's calls. A gathered
  batch's keys are looked up together, so a volatile clause of the
  inner side runs once for an inner row that the rows of several child
  batches look up. A gathering consumer evaluates its own expressions
  after its child has run its filters over more rows. Both differ from
  the core's plans and from Tessera without gathering. PostgreSQL
  promises that a volatile function is evaluated at every row where
  its value is needed, and this holds; the count of calls over a join
  and their order between nodes are a plan's, and the core's own hash
  join and parallel plans differ in them too.

**Refused:**

- A mode of no reading ahead, handed down from a limit, an `EXISTS`
  and every consumer that stops early through each streaming node: a
  mechanism in every node, against a class of errors the core does not
  avoid either.
- No gathering, and no index lookup, where a volatile function is in
  the inner side's clauses or columns or in the consumer's
  expressions, as the core refuses its cache over a nested loop
  (Memoize): the maintainer kept these plans, since PostgreSQL's
  promise holds without the refusal.

### Consumers

- TessHashJoin, its outer side in both modes; the index lookup then
  looks up to 64 keys at once.
- TessAgg, its input, and for INTERSECT and EXCEPT both of its inputs'
  counts read apart.

### The cost

**Was:** `lookup_shape` counts the child's batches; the lookup's
descents and the nested loop's both come from them.

**Will be:** only the index lookup's model changes, the one choice that
hangs on the count of batches. An outer side is gathered where the
planner finds stored values under it, by the rules of the flag above.
Its batches are counted from the rules. Between two batches that go as
they are, the sparse batches make a run, whose rows are copied. A run
gives its rows over the capacity and about half a batch for its last,
short one, and at least one batch. The model's shape of the child
gains three counts: the batches that go as they are, the rows copied,
and the runs with a row that such a batch ends. With the run the
child's end closes, and the first batch counted once:

```
gathered = 1 + passing
             + max(runs + 1, copied / capacity + (runs + 1) / 2)
```

at most the child's batches. Simulated batch by batch on binomial
batches of 30 to 64 rows, by the rules, it comes within 2.4 % of their
count. The branches of `lookup_shape` give the three counts:

- A scan under its filter, and the joins above that keep its batches:
  the model spreads the rows at random over the run of the table the
  clauses select, so a batch's rows follow a binomial law. A batch
  passes with the chance q that it holds half the capacity or more;
  the copied rows are the others' rows; and a passing batch ends a run
  with a row with the chance r / (q + r), where r is the chance of a
  sparse batch with a row.
- A node the model gives full batches (a pack that builds them, an
  index or bitmap scan, an aggregate, a sort): all pass.
- A join whose rounds may each be a batch, and a child where no scan
  is found, which the model counts a batch a row: all rows copied, no
  run ended.
- An append: the sums of its parts' counts. An append hands its parts
  on with no end between them, so a part's last run goes on into the
  next part, and only the whole's last run is added.
- A pack that passes its subquery's batches on: full batches, as now.
  The model does not look under the subquery; the gap is the model's
  already, and goes to the roadmap's "Not placed" at the close.
- A child without stored values gathers nothing and keeps its count.

For a partial path the counts are a participant's, as now. A gathered
batch spans the child batches it took: its span is the child's span
times the child's batches over the gathered ones, both counts a
participant's, so that the span keeps the units of the whole table's
run. The lookup's descents of keys in the table's order are counted
over that span. The nested loop's descents stay those of the child's
batches: its keys come one by one, gathered or not.

The lookup's time gains `join_lookup_copy_cost` a copied row and
column, the columns of the join's request to its outer side, as the
node's work in the table's part of the cost. The price is fitted by
`lookupcost` with the lookup's others, from cases with outer sides
through a selective filter and through a join, child batches of 22, 24,
31, 32 and 33 rows and of 31 and 33 in turn, and the same key
sequences in batches of different shapes, so that the two descent
counts are checked apart; its fit is revised too, for it reads the
batches the aggregate above it receives.

The hash join's own cost stays: it prices its outer batches as full
ones already (`probe_rows / 64`). The aggregate's cost has no term a
batch.

### EXPLAIN

`Gathered Rows` under `VERBOSE`, on a node whose input copied rows,
summed over participants and rescans. The aggregate's `Input Batches`
counts the batches it receives, a gathered one as one.

## Risks / Trade-offs

- A stored projection column is deformed and copied for every gathered
  row, though the consumer may drop the row first: a join whose probe
  finds few pairs over a sparse outer side with wide rows.
- The child is read ahead until a gathered batch fills, and a volatile
  function's calls change in count and order: limitations the
  maintainer accepted, written into `docs/limitations.md`.
- The count of gathered batches assumes rows spread at random over the
  scan's run. Rows in clumps, as under a filter on a column in the
  table's order, give fewer sparse batches than it counts.
- A child batch that waits keeps its buffer pins while the consumer
  works on the gathered batch before it.
- A dense input pays one comparison a batch.
- The copy's price is fitted on one machine.

## Measurement plan

The tools of `docs/measuring.md`, on Apple M5 Pro, an idle machine,
five minutes a run at most.

- TPC-H at SF 1, `cargo tpch`, base and change in turn, base, change,
  base, change, serial and with two workers; every query's plan with
  its lookups and `Gathered Rows`; Q14's batches and nodes' times as in
  the table above.
- `cargo ab measure` against the base: the families `win` (aggregates
  above filters; its cases over text and numeric columns), `join`,
  `indexjoin`, `tax`, and new cases of `join`: a sparse outer side with
  wide text columns and few pairs, a numeric argument summed above a
  filter, a projection's computed column above a filter (not gathered).
- `lookupcost` twice, for the fit.

The thresholds that decide:

- the geometric mean of TPC-H, serial and with two workers, no worse
  than the base's beyond the control, and no query slower by more than
  5 % beyond the control in both pairs;
- Q14 at most 0.85 of the base's time, serial;
- no case of the families flagged slower by `cargo ab` at its 2 %,
  once measured again alone, but one: the new case of wide rows and
  few pairs, which pays the copy by design, may be up to 10 % slower.
  Past that, a gathering that leaves out the request's projection
  columns is weighed before the close.

## Files

- `include/tessera/batch.h`: `values_stored`.
- `include/tessera/runtime_input.h`, `runtime/input.c`, a file of the
  gathered batch: the gathering.
- `runtime/heap_batch.c`, `runtime/builder.c` and the shared function:
  the decimals and the flag.
- `nodes/hashjoin_begin.c`, `nodes/hashjoin.c`,
  `nodes/hashjoin_pairs.c`, `nodes/hashjoin_spill.c`, `nodes/agg.c`,
  `nodes/agg_spill.c`, `nodes/sort.c`, `nodes/gather.c`,
  `nodes/append.c`, `nodes/heapscan.c`: the flag, the consumers, the
  bound and `Gathered Rows`.
- `nodes/join_planner.c`, `nodes/module.c`: the lookup's count and
  price.
- `docs/runtime.md`, `docs/costs.md`, `docs/nodes.md`,
  `docs/limitations.md`.

## Tests

- `test/tessera_input_test.c` (suite `input`): sparse, dense and mixed
  child batches, a dense batch after gathered rows, batches of 32 rows
  and of 31 and 33 in turn with the counts the rules give, a batch
  split and its rest, the first batch as it is, a batch without
  `values_stored`, a request's `max_batch_rows` below 64, by-reference
  values, NULLs, decimals, a rescan, the end of the input, gathering
  turned off and on again between scans, the call inside a scan, a
  column outside the request, the count over rescans. Its fake child
  grows: masks that select some rows, a numeric
  column, the flag; the suite loads the kernels' module for the
  decimals.
- `test/sql/index_join.sql`, `test/sql/join.sql`, `test/sql/agg.sql`:
  sparse inputs through a filter and through a join, joined, looked up
  and aggregated against the core's rows, `Gathered Rows` shown; the
  lookup's estimate of batches (`Lookup Estimate`) beside the batches
  it receives, for a scan's filter at densities around half a batch, a
  join's output and an append of partitions of a row each; an
  expression above a filter
  that raises an error on a row the join drops, and a volatile one that
  counts its calls, both as without gathering; the same expression
  under TessGather in the leader, with no worker launched; aggregates
  over TessAppend, an index-only scan, an outer join's tails and a
  join's compact and spilled output, gathered; a limit right above a
  join that reads no further than now; a limit by a parameter above a
  join, rescanned without a bound, with one and without again, its
  outer side's first batches without a pair; INTERSECT and EXCEPT;
  workers.

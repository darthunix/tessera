# The hash table in borrowed memory

Joins and grouping keep their state in a hash table whose memory the C
node owns. The table itself, where its memory lives, what it holds, how
a batch goes in and comes out, how it outgrows its index and what
several processes may do at once, is the capability
[hash-table](../openspec/specs/hash-table/design.md), and so are the
partitions of a table that spills and the Bloom filter of its keys.
This guide keeps what other parts build on it: the aggregate states of
a grouping, sorting records, and the phases of a shared build.

## One writer

A grouping keeps the states of its aggregates in the payloads of its
records, which only the one writer changes:

- `tess_table_accumulate(&table, offsets, &rows, op, column, prepared,
  value_at, flags_at, flag_bit, &status)` folds each selected row into
  the aggregate state of its record, the references
  `tess_table_find_or_insert` gave: `count(*)` and `count(x)` add one to
  the int8 at byte `value_at`; `sum(int4)`, the int8 sum of int8 values
  (`TESS_TABLE_SUM_INT8`: a parallel grouping's partial counts and sums,
  merged), `min` and `max` of int4 or int8 take the row's non-NULL
  value, the first one also setting bit
  `flag_bit` of the word at byte `flags_at`, so a state without the bit
  has seen no value and stands for NULL. The rows of a batch go in row
  order, several of one group in turn, so a sum past the int8 range
  fails with 22003 "bigint out of range" where the row-wise transition
  would; one check of the header per batch, as for `tess_table_gather`;
- `tess_table_accumulate_sums(&table, offsets, &rows, nsums, sums,
  &status)` folds each selected row into the sum or average states of up
  to `TESS_TABLE_MAX_SUMS` (32) aggregates of its record, each
  `TESS_TABLE_SUM_WORDS` (4) words at its `value_at`: the int128 sum of
  the values below 10^36 in magnitude at the largest display scale met
  (two words, low half first), their count, and a word of that scale
  (bits 0 to 7) and whether NaN, +Infinity and -Infinity were met (bits
  8, 9 and 10), as `tessera_kernels::decimal::SumState` keeps it; all
  zeros is the empty state a new record has. A sum's column is numeric
  (read in place, or its decimals), int4 or int8 words (an integer is a
  decimal at scale 0). A row a state does not take, a numeric not read
  in place or of more than 18 digits or a display scale past 18, or one
  the sum would carry to its bound, is set in that sum's `rest` mask for
  the caller. Within a batch the rows of up to 16 groups are added up by
  group first, the decimals a source hands over in bulk by a loop over
  their values, and each group's record is found once and changed once a
  sum; the rows of further groups go to their records one by one. Exact
  sums add in any order; a group's batch sum the state refuses at its
  bound is taken again row by row, so the rows it refuses are the ones a
  row-by-row fold would. When every sum's column holds partial states,
  the same call merges them into the records' states instead, as a final
  grouping merges the partial groupings' groups
  (`tessera_kernels::decimal::SumState::merge`, `sum_partials`): the
  sums added at the larger of their scales, the count added, the flags
  kept, NULL skipped, each row's record found once for all its sums (the
  rows of a batch are one participant's groups, so they are not added up
  by group first). A partial state is `TESS_TABLE_SUM_OF_STATE`, the
  node's own bytea: after the varlena header (4 bytes or 1) the tag
  `TESS_TABLE_SUM_STATE_TAG`, the state's four words in the machine's
  order, then, when the state has one, its numeric rest whole with its
  header (a plain aggregate's, which TessAgg merges itself, counts every
  value taken in word 2 and keeps NaN and the infinities in its rest, word
  3 its scale alone); or `TESS_TABLE_SUM_OF_PAIR`, the core's int8[] of the count and
  the sum of `avg(int4)` and `avg(int2)` (one dimension of 2, no NULL
  bitmap), a sum at scale 0. A state with a rest, one not read in place,
  or one the record's sum refuses at its bound is set in the sum's
  `rest` for the caller to merge, the record's state unchanged; a value
  of another tag, words out of range (a scale past 18, a sum at 10^36) or
  another array fails the call, as do partial states and numeric or int4
  sums in one call; an int8 column among partial states
  (`TESS_TABLE_SUM_OF_INT8`, the core's partial value of `sum(int2)`) is
  a state of one value each;
- `tess_table_accumulate_extremes(&table, offsets, &rows, column,
  value_at, max, &rest, &status)` offers each selected row's numeric to
  the `min` or `max` state of its record, `TESS_TABLE_EXTREME_WORDS` (3)
  words at `value_at`, as `tessera_kernels::decimal::ExtremeState` keeps
  it: a decimal's value, a word of its scale (bits 0 to 7), the kind of
  the extreme (bits 8 to 10: empty, decimal, NaN, +Infinity, -Infinity,
  or a numeric of the caller's) and pending (bit 11), and the address of
  the caller's copy of a numeric extreme, which the kernels never write;
  all zeros is the empty state. The rows go in their order, and a value
  equal to the extreme takes its place, as `numeric_larger` and
  `numeric_smaller` keep theirs; decimals, NaN and the infinities compare
  in `numeric_cmp`'s order. A row only the core can order (a numeric that
  is not a decimal, NaN or an infinity, or a decimal against the caller's
  numeric) is set in `rest` and makes the state pending: the
  group's later rows of the batch go to `rest` too, and the caller takes
  them in order and clears pending.

## Sorting records

A node that sorts keeps its rows as records whose keys are the sort keys,
in key order (`TessRows` in `tessera/runtime_rows.h`), and orders them without
linking them (`tessera/sort.h`, module `sort` of the kernels). Each
record becomes an item of whole 64-bit words: every key's value as bits
whose unsigned order is the key's order (an int4 or int8 with its sign
bit flipped, every bit inverted for a descending key), after one bit
that puts NULL first or last when the key may be NULL, and the record's
reference in the last word's low 32 bits. One int4 key, or two that are
never NULL, make one word; an int8 or up to three int4 keys, two; sixteen
int8 keys that may be NULL, the most, seventeen.

```c
kernels->sort_item_words(nkeys, keys, &words, &status);
kernels->sort_items(&table, nkeys, keys, items, nrecords * words, &count, &status);
kernels->sort(items, count, words, refs, &status);   /* refs in order */
```

`sort_items` walks every record of every chunk in the order appended,
linked or not, and `sort` sorts the items as arrays of words, the first
word most significant, with the standard library's unstable sort, then
reads the references out of them. The reference makes equal keys
distinct, so the order of equal keys is the order of their references.
The items are the caller's memory; the kernels allocate nothing. A new
type of key needs only its transform into bits with the order kept: the
sort itself does not change.

## Several participants

The participants of a shared build go through phases that the core's
`Barrier` separates, in its own numbering (`TESS_BUILD_*`):

- `BUILD`: every participant appends its share of the inner side to
  chunks of its own, each numbered by the build's shared counters
  (`tess_build_take_chunk`), then reports the records it appended and
  the payload words it saw a NULL in (`tess_build_report`);
- `FLUSH`: when the table spilled, every participant writes its chunks
  of the partitions that went to disk and finishes its files;
- `SIZE`: the elected participant makes the index for exactly the
  records appended (`tess_build_totals`) and the directory of the
  chunks by number, from which every participant maps their bases;
- `LINK`: every participant links its own chunks into the index,
  counting the duplicates unless the planner knows the inner side
  unique, and adds them to the counters (`tess_build_add_duplicates`);
- `OUTER`: when the table spilled, every participant writes its share of
  the outer side to the partitions' files, before any row goes out;
- `PROBE`: every participant probes, then leaves; the last to leave
  frees the table.

No phase copies records, and the table never grows: the index is made
once, for the rows that are there. A participant is a state machine
(`tess_build_step`, `TessBuildParticipant`) that never waits itself:
each step returns an action, the node performs it, and what a barrier
operation returned (the phase `BarrierAttach` gives, whether
`BarrierArriveAndWait` elected it, whether `BarrierArriveAndDetach`
found it the last) goes into the next step. The waits stay in the node,
since they may raise an error that must not unwind Rust frames. A
participant that attaches late joins the phase the others are in: while
they build, it appends what is left of the inner side, which a parallel
scan hands out page by page; from `SIZE` on it has nothing to link and
waits for the probe; after the last one left, it leaves at once.

A table that spills keeps what its participants decide in words every
one maps (`tess_table_spill_*`, `shared_spill.rs`): the first whose
chunks pass the budget splits the table into partitions, a
compare-and-swap that the others take the number from; while the chunks
still take more, the largest partition in memory goes to disk, marked by
the one participant whose fetch-or set its flag, and every participant
writes its own chunks of it. The `FLUSH` barrier orders every write
before the partitions are read, and `OUTER` comes before `PROBE` because
the core forbids waiting at a barrier once a participant returns rows:
a participant that returns rows may wait on the leader, which may wait
at the barrier. Each partition on disk is then a round of its own, with
a barrier of its own and the phases of the core's batches (`ELECT`,
`ALLOCATE`, `LOAD`, `PROBE`, `FREE`, `tess_round_step`): the elected one
makes the partition's index, all load its files, taken one at a time
from a counter, and link them, all probe and leave without waiting, the
last frees it. A partition too large for one participant is taken whole
by one of them (`tess_table_spill_take_alone`). A round's chunks are
linked as they are loaded, without counting duplicates: a link reads
only its own chunk and the buckets, so a participant sees the chunks
others load only once the round probes. A filter of every inner row is filled by all at once,
word by word atomically (`tess_bloom_shared_add`).

`make rust-loom` runs the table's own code over a model index and model
chunks of loom cells (`crates/tessera-kernels/src/table/loom.rs`) with
the orderings the real memory uses: two and three participants linking
their chunks into one bucket, a probe that finds a record another
participant is publishing and reads it whole, two participants racing
to build the shared filter and a reader that sees it ready and then
every bit, and a whole shared build of two participants, and of three
that attach at any phase, over a model of the core's barrier; two
participants past the budget agreeing on one split, two sending the
largest partition to disk and marking it once, and rounds of two and
three participants that attach at any phase, load every file once, probe
every key and free the partition once. Every
access to a record's bytes is announced to loom first, so a read not
ordered after the writing is reported. Four negative tests check that
the model catches what it should: a round that probes before every file
is loaded misses keys; relaxed bucket heads let a probe read
an unwritten record, a relaxed filter state lets a reader see an
unfilled filter, and linking before the index is made breaks the table.
The model found that counting the records after publishing them let
such a probe call a chain corrupt; they are counted first.

## The atomics in order

A shared build, by the phases of the barrier:

```
 BUILD ─ every participant:
          tess_build_take_chunk: counters.chunks.fetch_add(1)   Relaxed → a chunk number
          dsa_allocate, tess_table_chunk_init
          under the node's spinlock: the chunk into the table's list
          tess_table_append of its rows into its own chunks (plain stores)
          tess_build_report: counters.records, null_columns fetch_add   Relaxed
 ═══ BarrierArriveAndWait ═══  ← orders everything above
 SIZE  ─ the elected one: the totals (Relaxed) → the index for exactly N records (plain
          stores of the header, the buckets cleared) → the directory of dsa_pointers by
          number, from the list → the Bloom filter, cleared, which one
          participant builds later
 ═══ BarrierArriveAndWait ═══
 LINK  ─ every participant: the chunks' bases from the directory → tess_table_link of its
          own chunks (fetch_add and CAS, as above), counting duplicates →
          tess_build_add_duplicates: counters.duplicates fetch_add   Relaxed
 ═══ BarrierArriveAndWait ═══
 PROBE ─ every participant: probes (Acquire loads of the heads)
 ═══ BarrierArriveAndDetach ═══ → the last one frees the index, directory, chunks and values
```

The build counters need no ordering: the barrier shows the others everything a participant
did before it (the model of the core's barrier in `loom.rs`); other participants' used
marks are only read after the barrier.

## Tests and measurements

`crates/tessera-kernels/tests/table.rs` covers the format and every
operation, including corrupt headers, chunks, chains and references,
over `LocalTable`, a table that owns its index and chunks, and runs
under Miri; `crates/tessera-capi/tests/table.rs` compares Datum and
dense key columns and calls the entry points as C would;
`test/tessera_table_test.c` is the C test module, run by `make
installcheck` as the `table` suite, which runs the load-time layout
check and runs batches through appending over
several chunks, linking, probing, grouping, a new index and the error
statuses. The benchmark `table_int32`
(`crates/tessera-capi/benches/README.md`) measures insertion, probes and
find-or-insert on PMU counters against a chained table with plain
stores. The benchmark `table_large` adds a table past the cache, where
the groups `hit`, `miss` and `occupied` also time the Bloom filter check
alone (`bloom_probe`) and with the probe of the rows it passes
(`bloom_then_probe`).

# The hash table in borrowed memory

Joins and grouping keep their state in a hash table whose memory the C
node owns. The table is implemented in Rust (`crates/tessera-kernels`,
module `table`) and reaches C through `include/tessera/table.h`, in the
static library of the kernels. This guide is the C-side contract: where
the memory lives, what it holds, how a batch goes in and comes out, how
the table outgrows its index, and what several processes may do at once.

## Why chunks and references

A serial plan keeps the table in the memory of its query context; a
parallel plan keeps the build side of a join in dynamic shared memory,
which every process maps at an address of its own. One table serves
both because it never holds an address: it is an index and the chunks
its records lie in, all allocated by the caller, who hands them to every
call as a `TessTableRef` (the index's address and length, and the
addresses and lengths of the chunks in this process, by number). Rust
keeps nothing between calls and allocates nothing, and a record is
addressed by its chunk number and its place in the chunk, so the bytes
mean the same in every process, whatever address a chunk has there.

Records never move. A chunk fills and the caller adds another; when the
records outgrow the buckets, only the index is made anew, over the same
chunks. The first design kept the records and the buckets in one region
that grew by `repalloc` or by a copy: a shared table that the planner
underestimated was copied whole, with the old and the new region held at
once (plan item 5.1b). With no table stored anywhere yet, the format kept
its version, 1.

## The index and the chunks

Every block is aligned to 8 (`palloc` and DSA allocations are).

The index holds a header of 96 bytes with a magic value and the format
version (`TESS_TABLE_FORMAT_VERSION`, 1), the index length, the key
kinds, the payload size, the record size, the bucket count and the
record count; then the buckets, a power of two of 32-bit slots, at least
1024 and at least twice the capacity the index was made for, each
holding the reference of the first record of its chain. A bucket is the
high bits of the hash.

A chunk holds at most `TESS_TABLE_MAX_CHUNK_LEN` (1 MiB) bytes, a
multiple of 8: its first 8 bytes (`TESS_TABLE_CHUNK_HEADER`) count the
bytes it uses, and the records follow one after another. A table has at
most `TESS_TABLE_MAX_CHUNKS` (32768) chunks, 32 GiB.

A reference is a `uint32`: the chunk number in the high 15 bits and the
record's place in the chunk, in units of 8 bytes, in the low 17. 0 means
none, since the used mark of chunk 0 lies there. A record holds 16 bytes
of header (its hash, the reference of the next record of its bucket, a
bit per key that is NULL, its length in 8-byte units), then one 8-byte
slot per key, then the payload, rounded up to 8. Keys are `int4`
(sign-extended into the slot) or `int8` (`TessTableKeyKind`), up to
`TESS_TABLE_MAX_KEYS` of them; a NULL key holds 0 in its slot and sets
its bit. The payload is opaque: a join keeps the Datums of its build row
there, grouping its aggregate states.

## The layout in pictures

```
 TessTableRef (one per process: the addresses differ between processes)
 ┌────────────────────────────────────┐
 │ index ──────────────► INDEX        │   one allocation; only a regrow replaces it
 │ index_len                          │
 │ chunks[] ─┬─► chunk 0              │   64 kB in the nodes
 │           ├─► chunk 1              │   1 MB
 │           ├─► chunk 2              │   1 MB
 │           └─► …                    │   up to 32768 chunks
 │ chunk_lens[]                       │
 │ nchunks                            │
 └────────────────────────────────────┘
```

The index:

```
 byte
   0 ┌──────────────────────────── header, 96 bytes ───────────────────────────────┐
     │ 0   magic "TESSTABL"   8 version = 1   12 header_size                       │
     │ 16  region_len (index length)        24 buckets_offset = 96                 │
     │ 32  reserved_used = 0                40 nrecords  ◄── the one field that     │
     │                                                       changes while a       │
     │                                                       build runs            │
     │ 48  nbuckets   52 bucket_shift   56 record_size   60 payload_size           │
     │ 64  nkeys      68 flags          72 kinds[16]     88 reserved               │
  96 ├──────────────────────────────── buckets ────────────────────────────────────┤
     │ [0] u32 reference │ [1] u32 │ [2] u32 │ … │ [nbuckets - 1] u32               │
     └───────────────────────────────────────────────────────────────────────────────┘
       nbuckets: a power of two, at least max(1024, 2 × capacity)
       bucket = hash >> bucket_shift (the high bits of the hash); 0 is an empty bucket
```

A chunk and a record:

```
 chunk k (at most 1 MiB, aligned to 8)
 ┌────────┬──────────┬──────────┬──────────┬─────────────┬───────────────┐
 │ used   │ record 0 │ record 1 │ record 2 │    …        │ free          │
 │ 8 bytes│          │          │          │             │               │
 └────────┴──────────┴──────────┴──────────┴─────────────┴───────────────┘
  used: the bytes taken, these 8 included; only the chunk's writer stores it

 record (record_size bytes, a multiple of 8)
 ┌──────┬──────┬───────────┬──────┬────────┬────────┬─────┬──────────────────┐
 │ hash │ next │ null_bits │ len  │ key 0  │ key 1  │  …  │ payload, padding │
 │ u32  │ u32  │   u32     │ u32  │  i64   │  i64   │     │                  │
 └──────┴──────┴───────────┴──────┴────────┴────────┴─────┴──────────────────┘
  0      4      8           12     16 (the record header is 16 bytes)
  next: the reference of the next record of the bucket, 0 at the end of the chain
```

A reference, the same in every process:

```
  31            17 16                    0
 ┌────────────────┬────────────────────────┐
 │ chunk (15 bits)│ place / 8 (17 bits)    │   address = chunks[chunk] + place · 8
 └────────────────┴────────────────────────┘
  reference 0 is chunk 0, byte 0: the used mark, never a record, so 0 means none
```

Before a record is read, its reference is checked: the chunk below `nchunks`, the place at
least 8, the whole record within `chunk_lens[chunk]`, its `len` equal to `record_size`.

A chain may run through several chunks:

```
 index                               chunk 0              chunk 1              chunk 2
 ┌──────────┐                        ┌──────────┐         ┌──────────┐         ┌──────────┐
 │ bucket 0 │ 0                      │ used     │         │ used     │         │ used     │
 │ bucket 1 │────────────────────────┼──────────┼─────────┼─►┌─────┐ │         │          │
 │ bucket 2 │ 0                      │          │         │  │ A   │─┼─────────┼─►┌─────┐ │
 │ bucket 3 │──────►┌─────┐          │          │         │  │next │ │         │  │ B   │ │
 │   …      │       │ C   │ next = 0 │          │         │  └─────┘ │         │  │next=0│
 └──────────┘       └─────┘(chunk 0) └──────────┘         └──────────┘         └──┴─────┴─┘
```

`tess_table_link` puts each record at the head of its bucket, so a key's records lie among
others and the next one is found by a walk (`tess_table_next_match`);
`tess_table_link_grouped` puts a record right after one with the same keys, so the next
record of a key is the next one in the chain (`tess_table_next_in_group`):

```
 link:          bucket → D(k=5) → B(k=7) → C(k=5) → A(k=5)
 link_grouped:  bucket → D(k=9) → A(k=5) → C(k=5) → E(k=5) → B(k=7)
                                  └── key 5 together ──┘
```

## Creating and attaching

`tess_table_size(nkeys, kinds, payload_size, capacity, &size, &status)`
says how many bytes the index for `capacity` records needs, a multiple
of 8. `tess_table_create(index, len, nkeys, kinds, payload_size,
capacity, &status)` lays the index out; a table holds more records than
its capacity, in more chunks, but its chains grow longer past it.
`tess_table_chunk_init(base, len, &status)` makes a block an empty
chunk. `tess_table_attach(&table, &status)` only checks that the
reference holds a table of this format, and `tess_table_stats` reports
the record count, the bucket count, the bytes of the index in use and
its length, for planning and `EXPLAIN`.

Every call attaches anew and checks the whole header: the magic and the
version, the sizes, that the buckets are a power of two inside the
index; and the chunks: aligned, at most 1 MiB, a multiple of 8. Every
reference a call follows is checked against its chunk's number, length
and used mark and the record length, and a chain is walked at most as
many steps as there are records. A corrupt table is therefore a status,
never a crash or a hang. `tess_table_format_version` and
`tess_table_layout` let a C test compare the format and the structures
with what the library was built with.

## A batch in and out

A batch brings three things: its hashes, one `uint32` per physical row,
from `tess_int4_hash`, `tess_int8_hash` and their `_next` forms, which
also apply the NULL policy (`TESS_NULL_KEYS_REJECT` drops NULL keys from
the mask, for joins; `TESS_NULL_KEYS_GROUP` keeps them as a key of their
own, for grouping); its keys, one `TessTableKey` per key of the table, a
Datum column read by its kind with the readiness mask of the batch
contract; and a row mask. An int8 inside the int4 range hashes as the
int4 and both are stored as 8-byte slots, so an int4 key column may
probe a table whose records came from int8 keys, as a join of an int4
column with an int8 one does.

A build has two steps. `tess_table_append(&table, chunk, payload_size,
hashes, nkeys, keys, payload, &pending, offsets, &status)` writes the
rows of `pending` as records into chunk `chunk`, in row order, as long
as whole records fit: each row appended leaves `pending` and gets the
reference of its record in `offsets`; the rows still pending need
another chunk. `payload` is the payload of every physical row one after
another, or `NULL` for zeros. `tess_table_append_columns` takes the
payload from columns instead, a `TessDatumColumn` each: a record's payload
is then a word of the row's NULL bits per 64 columns (column `c` takes bit
`c % 64` of word `c / 64`) and a word per column, 0 for a NULL,
written straight from the columns with no array in between (`TessRows`
passes a by-value column as it is and a by-reference one as the
references of its copies). Append does not read the index, which a
build may not have yet. `tess_table_link(&table, chunk, &from, &linked,
&duplicates, &status)` then puts the chunk's records from byte `from` on
(starting at `TESS_TABLE_CHUNK_HEADER`) into the buckets, and moves `from`
past them; equal keys make separate records that chain in their bucket.
With `duplicates` (not `NULL`), each record, once published, walks the
rest of its chain for a record with its hash, NULL bits and keys, and
`duplicates` receives how many found one: the records whose keys the
table held already. The compare-and-swap orders a bucket's records, so
of two records of one key exactly the one linked later finds the other,
whatever participants link at once, and the sum over the participants
is exact, the count `tess_table_link_grouped` gives.

`tess_table_probe(&table, hashes, nkeys, keys, &rows, matches, &found,
&status)` finds, for each row of `rows`, the first record of its chain
with its hash, NULL bits and keys: `matches[row]` gets the reference and
`found`, a mask the call fills whole, the rows that have one. Keys are
compared whole: the hash alone cannot decide, since under the group
policy a NULL key hashes like the value `0x9e3779b9`, and int8 keys have
no bijection. Equal keys have separate records, so
`tess_table_next_match(&table, offsets, &rows, &found, &status)`
replaces each row's reference in place by the next record of its chain
with the same keys, until `found` is empty: a join walks the chains of a
whole batch of probe rows at a time. A table linked by
`tess_table_link_grouped` (below) keeps a key's records next to each
other in the chain, and `tess_table_next_in_group(&table, offsets,
&rows, &found, &status)` steps to the next one by looking at the record
right after a row's own only: one step, where `tess_table_next_match`
walks the rest of the chain to find that no other record of the key is
there.

`tess_table_gather(&table, offsets, &rows, at, values, &status)` reads,
for each row of `rows`, the 8 bytes at byte `at` of the payload of the
record at `offsets[row]` into `values[row]`; other rows keep their
values, and `at + 8` must lie within the payload. A join keeps the
Datums of its build row as payload words and fetches one column of a
batch of matches per call, with one check of the header, where a call
per row would check it per row. `tess_table_gather_scattered` reads the
same for records in no order, as a sort reads its rows back: it locates
every record of a word of rows and prefetches its header and word before
reading any, so that the cache misses overlap. A join's matches were
just read by the probe and are in the cache, where the extra pass only
costs (2–5 % of the join cases that read inner columns), so the join
keeps `tess_table_gather`.

`tess_table_gather_key(&table, offsets, &rows, key, values, isnull,
&status)` reads key `key` of each row's record the same way, as its
Datum (an int4 key sign-extended, as `Int32GetDatum` makes it) and its
NULL flag from the record's NULL bits: a grouped aggregate returns its
groups' keys with it.

`tess_table_record(&table, offset, &record, &status)` exposes a record's
hash, NULL bits, key slots and payload as pointers into its chunk,
valid as long as the chunk.

## One writer

Grouping, output and a new index need the table to themselves, with no
other call over it at the same time:

- `tess_table_find_or_insert(&table, chunk, hashes, nkeys, keys,
  &pending, offsets, &inserted, &status)` gives each pending row the
  record of its keys, creating one with a zero payload in chunk `chunk`
  where none exists, in row order, until the chunk is full or the
  records reach half the buckets; `inserted` receives the rows whose
  record the call created, so the caller initializes their aggregate
  states. Rows left pending need another chunk or, when the records are
  at half the buckets, a larger index;
- `tess_table_link_grouped(&table, chunk, &from, &linked, &duplicates,
  &status)` links a chunk's records as `tess_table_link` does, but each
  right after a record with the same keys when the table holds one, so
  that a key's records lie together in their chain; `duplicates`
  receives how many records had keys the table held already. It looks
  every record up, which is why it belongs to one writer: a serial join
  links its whole table with it once the inner side is read, and a
  table without duplicates needs no second round at all;
- `tess_table_payload(&table, offset, &payload, &status)` hands out a
  payload to change in place;
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
- `tess_table_scan(&table, &cursor, offsets, capacity, &count, &status)`
  visits the records chunk by chunk in the order they were appended, up
  to `capacity` per call, from a cursor the caller starts at 0 and keeps
  between calls; a count of 0 ends the walk;
- `tess_table_regrow(&table, index, len, capacity, &status)` moves the
  table to a new index of `len` bytes for `capacity` records: the
  buckets are filled anew from the records, which stay where they are
  with their references, a record right after an earlier one with the
  same keys; the old index is no longer the table's, and the caller
  frees it. An index with fewer buckets is refused.

A walk reads every record below a chunk's used mark, which an append in
flight moves; that is why it belongs to the one writer.

## Partitions for spilling

A node whose table outgrows its memory keeps the records in partitions
and writes whole chunks of some of them to disk (see
[spill.md](spill.md)). The partition of a hash is
`(hash >> shift) & (npartitions - 1)`, a power of two of partitions: the
buckets take the hash's high bits, so the first level takes its low ones
and a partition split further takes the bits above them. Two calls work
on chunks alone, without the index, as `tess_table_append` does:

- `tess_table_append_partitioned` appends a batch's rows each to the
  current chunk of its partition, given as a chunk number per partition.
  A row whose partition's chunk is full stays pending while the rows
  after it go on, so the node gives every such partition a new chunk and
  calls again. `tess_table_append_partitioned_columns` does the same with
  the payload taken from columns, as `tess_table_append_columns` takes
  it, and also adds each appended row to its partition's count and ORs
  the rows' NULL bits into a word, so it takes 64 columns at most: a
  spilling join appends a batch
  without a pass of its own over the rows (plan item 5.12, a fifth off a
  spilled join).
- `tess_table_split` copies the records of one chunk, whole and in order,
  each to the chunk of its partition, and stops before a record whose
  partition's chunk is full, naming that partition; it returns the new
  references and the hashes of the records copied. A node uses it once,
  when its table first overflows, to sort the chunks it built into
  partitions, and again when a partition read back from disk is still too
  large and splits by the next bits. The copies are not linked: a
  partition gets an index when it is processed.

A grouping that spills (TessAgg) keeps one index over every partition's
chunks, since its rows must find their groups as they come, and has two
calls of its own:

- `tess_table_find_or_insert_partitioned` resolves a batch's rows to the
  records of their keys as `tess_table_find_or_insert` does, but a new
  group goes to the current chunk of its partition; a row whose
  partition's chunk is full stays pending while the rows after it go on,
  and every row stops once the records reach half the buckets.
- `tess_table_combine` merges a chunk of groups' states read back from
  disk into the table: the record of the same keys takes each state in
  as the caller says per aggregate (counts and sums add, with 22003 past
  the int8 range, a sum only where it has a value; minima and maxima keep
  the extreme; the flags join), and a group the table lacks is copied
  whole to a chunk the caller names and linked. It stops where a new
  group needs another chunk or a larger index, and goes on from there.

## A Bloom filter of the keys

A probe that finds no record still reads a bucket, and a record too when
the bucket holds another key; on a table past the cache these are cache
misses. A join whose rows mostly find no pair can check them first
against a Bloom filter of the table's keys, which rejects most of those
rows without touching the table. The filter is a blocked one: one 64-bit
word per key, four bits in it, both taken from the row hash (the same
hash the table uses, with every key and the NULL policy) multiplied by
`0x9E3779B97F4A7C15`, the word from the high bits of the product and the
bits from its low 24, so the word does not repeat the bucket index. A
check reads one word and compares it with a four-bit mask. The size is a
power of two words, 16 bits per record, which lets about 1 % of absent
keys through; a key of the table always passes.

Like the table, the filter lives in a borrowed buffer of words with no
process addresses in it, so it may sit in shared memory next to a shared
table:

- `tess_table_bloom_words(records, &nwords, &status)` gives the size for
  a number of records;
- `tess_table_bloom(&table, words, nwords, &status)` clears the words
  and sets the bits of every record of the table's chunks; it walks the
  records, so no append may run meanwhile, as for a scan;
- `tess_bloom_probe(words, nwords, hashes, &rows, &found, &status)`
  fills `found` whole with the rows of `rows` whose bits are all set;
  the two masks must not share words.

A shared filter (`tess_bloom_shared_words`, `tess_bloom_shared_init`)
is a state word (none, building, ready) and then the filter's words,
cleared by one participant before the others use it. Each participant
decides by its own batches whether it wants the filter;
`tess_table_try_build_bloom` lets the first that does claim it with a
compare-and-swap of the state, fill it alone and mark it ready with
release, and tells the others it did not, which wait for nothing:
`tess_bloom_shared_probe` checks a batch only once the state reads
ready with acquire (`tess_bloom_shared_ready`), and until then the
participant probes the table without the filter.

## Sorting records

A node that sorts keeps its rows as records whose keys are the sort keys,
in key order (`TessRows` in `tessera/runtime.h`), and orders them without
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

Over shared memory, several processes may append to chunks of their own
and link them at once, and several may probe, but not both at a time: a
join builds, passes a barrier, then probes. An append writes only its
own chunk and moves its used mark. A link publishes each record with a
compare-and-swap of its bucket's head (release), which a probe reads
with acquire, after counting the chunk's records into the index's
record count, so that a probe that finds a record also sees a count that
covers its chain, whose length it checks against the count. A published
record never changes, except its payload under the one writer. The same
code runs over local memory, where the compare-and-swaps never fail.

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

## How the table grows

Records never move; only the index is made anew.

- A serial `TessHashJoin` appends the inner side to chunks (64 kB, then 1 MB, another when
  the last is full) with no index at all (`index` is NULL); once the inner side is read,
  it makes the index for exactly the rows appended and links every chunk grouped:
  `build_rows = N → index for N → link_grouped(chunk 0), link_grouped(chunk 1), …`.
- `TessAgg` creates the index for the planner's estimate of the groups;
  `tess_table_find_or_insert` appends new groups to the last chunk and puts them into the
  buckets at once, and stops when the chunk is full (the node adds a chunk) or the groups
  reach half the buckets (the node makes an index for twice the groups with
  `tess_table_regrow`: the header copied, the buckets cleared, every record of every chunk
  put into its bucket again, grouped; the old index is freed). Both indexes live for the
  moment of the regrow, 4 bytes per bucket each; no record is copied.
- A shared table does not grow at all: its index is made once, for every record appended
  (below).

## The atomics in order

Most header fields are written once, when the index is made, before anyone else sees it,
and are read without ordering. Only `nrecords` and the buckets change during a build.

Linking a chunk (`tess_table_link`, several processes at once in a shared build):

```
 1. nrecords.fetch_add(records of the chunk)        AcqRel   ← counted first
 2. for each record of the chunk, in order:
      hash = record.hash                            plain read (the linker's own record)
      head = bucket.load()                          Acquire
      loop:
        record.next = head                          plain store (the record is not yet visible)
        CAS(bucket, head → the record's reference)  AcqRel; on failure Acquire and again
      with duplicates: walk from record.next         plain reads (published records never change)
        a record with the same keys → count it
```

The count comes first so that a probe that finds a new record also sees a record count
that covers its chain; otherwise it could take a long chain for a cycle and report a
corrupt table (the loom model found this).

Probing, once linking is over:

```
 head = bucket.load()                               Acquire  ← sees the whole record the CAS published
 each step:
   check the reference (chunk < nchunks, the chunk's length)
   read hash, keys, next                            plain reads (a published record never changes)
   steps ≥ nrecords: read nrecords again            Acquire; still ≥ → a cycle, the table is corrupt
```

Appending (`tess_table_append`) is plain stores into the appender's own chunk and its used
mark; the buckets are not touched. The one writer (`tess_table_find_or_insert`,
`tess_table_link_grouped`, `tess_table_regrow`) uses the same CAS, which never fails for it;
placing a record after another of its keys is plain stores of two `next` fields, which one
writer may do.

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
          number, from the list → the Bloom filter
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

The shared Bloom filter has a state word (0 none, 1 building, 2 ready): the participant
whose CAS 0 → 1 succeeds fills the filter and stores 2 with Release; the others check rows
against it only after they read 2 with Acquire, and probe the table without it until then.

## Ownership and errors

Entry points borrow the index, the chunks and the batch's buffers and
own nothing. The status rules of `tessera/kernels.h` apply: a dimension,
pointer or header error comes before any change; after a failure the
mutable outputs of the call (masks, offsets) hold unspecified values,
and the caller reports the status with `ereport` after the call returns.
Buffers must not alias: a mask a call fills must not be the mask it
reads.

## Tests and measurements

`crates/tessera-kernels/tests/table.rs` covers the format and every
operation, including corrupt headers, chunks, chains and references,
over `LocalTable`, a table that owns its index and chunks, and runs
under Miri; `crates/tessera-capi/tests/table.rs` compares Datum and
dense key columns and calls the entry points as C would;
`test/tessera_table_test.c` is the C test module, run by `make
installcheck` as the `table` suite, which checks the layout probes
against `sizeof` and `offsetof` and runs batches through appending over
several chunks, linking, probing, grouping, a new index and the error
statuses. The benchmark `table_int32`
(`crates/tessera-capi/benches/README.md`) measures insertion, probes and
find-or-insert on PMU counters against a chained table with plain
stores. The benchmark `table_large` adds a table past the cache, where
the groups `hit`, `miss` and `occupied` also time the Bloom filter check
alone (`bloom_probe`) and with the probe of the rows it passes
(`bloom_then_probe`).

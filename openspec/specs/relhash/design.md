# relhash: how it is built

## Background: keys, hashes, buckets, chains

A **key** is the value a row is found by: in a join, the column of the
join condition; in a grouping, the columns of GROUP BY. A key may have
several columns.

A **hash** is a number of 32 bits made from the key. Equal keys always
have equal hashes. Different keys usually have different hashes, but
not always.

The table keeps each column of a key in a word of 8 bytes and compares
keys bit for bit; it gives a word no type. Tessera's key columns fill
the words in two forms: an `int4`, widened to 8 bytes with its sign,
and an `int8`. A key of another SQL type goes in in one of these two
forms, either as its value or as a number that stands for it; "Keys of
each SQL type" below tells which type goes which way and why.

A hash table keeps an array of **buckets**. A row goes to the bucket
that its hash picks. Rows whose hashes pick the same bucket are linked
one after another in a list, called a **chain**. To find a key, take
its bucket and walk its chain, comparing the key of each row.

```
 buckets            chains
 ┌───────┐
 │   0   │ ── none
 ├───────┤
 │   1   │ ──► row (key 7) ──► row (key 12) ──► none
 ├───────┤
 │   2   │ ── none
 ├───────┤
 │   3   │ ──► row (key 5) ──► none
 └───────┘
```

Each row in the table is stored as a **record**: its hash, the link to
the next record of its chain, its key, and a **payload**. The table
does not look into the payload. A join keeps there the other columns of
the row it will return. A grouping keeps there the states of its
aggregates, such as a running sum.

Tessera does not pass rows between nodes one at a time. A node gets a
**batch**: up to 64 rows at once, kept by column. The table works on
batches too: one call puts in, or looks up, all the rows of a batch.

A **mask** says which rows of a batch a call works on: one bit for each
row, 1 for a row the call takes and 0 for a row it leaves alone. The
bits are kept in words of 64 bits, row `r` in bit `r % 64` of word
`r / 64`, so a batch of 64 rows has a mask of one word. In C a mask is
a `TessRowMask` of `tessera/row_mask.h`: the number of rows and the
address of the words.

```
 row    0   1   2   3   4   5   …  63
 bit    1   0   1   1   0   1   …   0     one word of 64 bits
 rows 0, 2, 3 and 5 take part; rows 1 and 4 do not
```

A filter clears the bits of the rows it drops. The calls of the table
read such a mask and write masks of their own: the rows still to place,
the rows found, the rows that got a new record.

## The whole in one picture

A table is one **index** and any number of **chunks**. The caller
allocates all of them and passes them to every call as a
`TessTableRef`: the address and length of the index, and the address
and length of each chunk, by number.

```
 in each process: TessTableRef     in memory, the same for all

 index ──────────────────────────► ┌──────────┬────────────────────┐
 index_len                         │ header   │ buckets            │
                                   └──────────┴──────────┬─────────┘
                                                         │
 chunks[0] ──────────────────────► ┌──────┬────────┬─────▼──┬──────┐
 chunk_lens[0]                     │ used │ record │ record │ free │
                                   └──────┴────────┴──┬─────┴──────┘
                                                      │ next
 chunks[1] ──────────────────────► ┌──────┬────────┬──▼─────┬──────┐
 chunk_lens[1]                     │ used │ record │ record │ free │
                                   └──────┴────────┴────────┴──────┘
 …
```

The index holds the buckets. A bucket holds the reference of the first
record of its chain, and each record holds the reference of the next.
The references are numbers, not addresses, so the right half of the
picture is the same in every process; only the left half differs.
"References" below shows how a reference is made and followed, with a
chain as an example.

## The words used below

- **Index**: one block with a header of 96 bytes and the array of
  buckets.
- **Header**: the description of the table: its format, its keys, the
  size of a record, the number of buckets, and the count of records
  linked into the buckets.
- **Bucket**: 32 bits that hold the reference of the first record of a
  chain, or 0 when the chain is empty. The high bits of a hash choose
  the bucket.
- **Chunk**: a block of at most 1 MiB that holds records one after
  another. Its first 8 bytes are its **used mark**: how many of its
  bytes are taken.
- **Record**: one row in the table: a header of 16 bytes, a slot of 8
  bytes for each key, then the payload.
- **Reference**: 32 bits that name a record: the number of its chunk
  and its place in the chunk. 0 names no record.
- **Capacity**: the number of records an index is made for. A table may
  hold more, but its chains then grow longer.
- **Append**: write rows as records into a chunk. No bucket points to
  them yet, so a probe cannot find them.
- **Link**: put appended records into their buckets. Now they can be
  found.
- **Probe**: look up the rows of a batch by their hashes and keys.
- **One writer**: a caller that has the table to itself while it works.
- **Participant**: one process of a parallel plan that works on a
  shared table.

## The index

The index is made once for a number of records, its capacity:
`tess_table_size` says how many bytes it needs, the caller allocates
them, and `tess_table_create` writes the header and clears the
buckets. The spec draws the header field by field, in
[The index of a table](spec.md#requirement-the-index-of-a-table).

Most fields of the header are written once, when the index is made,
and never change. One field changes while the table is used: the count
of the records linked into the buckets. It is there for two reasons:
`tess_table_stats` reports it to EXPLAIN and to the node's own rules,
and a walk down a chain uses it as its bound (see "Checks and errors").

The bucket of a hash is its high bits. The low bits are left free on
purpose. A node whose table does not fit in memory splits its rows
into partitions by the low bits of the hash. Inside one partition the
low bits are all the same, but the high bits still spread the rows over
every bucket.

The index has at least twice as many buckets as its capacity, and at
least 1024. With at most one record for every two buckets, most chains
hold one record or none. A table may hold more records than its
capacity; it stays correct, only slower.

## Chunks and records

A chunk is a plain block of memory. `tess_table_chunk_init` makes a
block an empty chunk by writing its used mark. Records follow the mark
one after another, all of the same size, and the mark grows by a record
each time one is written. Only the chunk's one writer appends to it.
The links of the table never leave its chunks: a bucket and the field
next of a record name only records of the same table. A payload may
name memory outside the table, as "Values longer than a word" shows
below.

A record has a fixed size for the whole table: a short header, a slot
for each key, then the payload. The spec draws it byte by byte, in
[A record](spec.md#requirement-a-record). Why each piece is there:

- **The hash** is compared before the keys. Two different keys in one
  chain usually have different hashes, so most wrong records are
  skipped after one comparison of 32 bits.
- **A slot of 8 bytes for every key**, also for an `int4`, which is
  widened with its sign. An `int8` that fits in an `int4` hashes like
  that `int4`, so a join of an `int4` column with an `int8` column puts
  both into one table and compares them slot by slot.
- **The NULL bits.** A grouping puts NULL keys into a group of their
  own. A NULL key's slot holds 0, and its bit in the record's header
  says that it is NULL, not the number 0. Where the bits lie is drawn
  in [A record](spec.md#requirement-a-record).
- **The record's own size** gives every call a cheap check that a
  reference points to a record of this table.
- **The payload** is the caller's. The table writes it when a record is
  appended (zeros when the caller gives none), and only the one writer
  changes it later.

## References

A reference names a record in 32 bits. The high 15 bits are the number
of the record's chunk. The low 17 bits are the record's place in the
chunk, in units of 8 bytes: every record starts at a multiple of 8, so
its byte is the place times 8. The spec draws this split in
[Chunks and references](spec.md#requirement-chunks-and-references).
The split sets the limits. 2^17 places of 8 bytes make a chunk of at
most 1 MiB, and 15 bits number at most 32768 chunks, so one table holds
at most 32 GiB of records. `tessera/table.h` has three small functions
that make a reference and split it.

References lie in two places: a bucket holds the reference of the first
record of its chain, and the field next in the header of every record
holds the reference of the record after it. To follow a reference, a
process splits it, takes the address of the chunk from its own
`TessTableRef`, and adds the byte. In the example below a record takes
40 bytes, so the records of a chunk start at bytes 8, 48, 88 and so on.
A chain of two records goes from chunk 1 to chunk 0:

```
                ┌──────────────────┐
 bucket 5       │ 131078           │  = 1 * 2^17 + 6: chunk 1, byte 48
                └──┬───────────────┘
                   ▼
 chunks[1]      ┌──────┬──────────┬───┬─────┬─────────┐
 byte 48        │ hash │ next: 11 │ … │ key │ payload │
                └──────┴────┬─────┴───┴─────┴─────────┘
                            ▼   11 = 0 * 2^17 + 11: chunk 0, byte 88
 chunks[0]      ┌──────┬──────────┬───┬─────┬─────────┐
 byte 88        │ hash │ next: 0  │ … │ key │ payload │
                └──────┴──────────┴───┴─────┴─────────┘
                        next 0: the end of the chain
```

Before a call reads a record, it checks the reference: see "Checks and
errors".

Reference 0 would be the first byte of chunk 0, which is the chunk's
used mark and never a record. So 0 can mean "no record": an empty
bucket, the end of a chain, a row without a match.

PostgreSQL's own parallel hash join links its tuples by `dsa_pointer`,
a number of 64 bits. With 32 bits a bucket and the link in every record
take half the memory, and more of them fit in the cache. The price is
the limit of 32 GiB of records in one table.

## Putting rows in: append, then link

A join builds its table in two steps.

```
 a batch
    │  tess_table_append: writes the chunk only
    ▼
 records in a chunk, which nobody finds yet
    │  tess_table_link: writes the buckets
    ▼
 records in their chains, which probes find
```

**Append** writes the rows of a batch as records into one chunk, in row
order, while whole records fit. A batch comes with three things: a hash
for every row, the key columns, and a mask of the rows to place. Each
row that is written leaves the mask and gets the reference of its
record. The rows still in the mask need another chunk: the caller adds
one and calls again. Append does not need the index, so it works
before there is one. When the table has an index, append checks it as
every call does and refuses keys or a payload size other than the
table's. A node's mistake then fails where it is made, and is not
found later as a damaged table.

The payload of a record comes either from an array that the caller
filled, or, with `tess_table_append_columns`, straight from the
batch's columns, in the form that
[Appending rows](spec.md#requirement-appending-rows) gives. The second
form saves the node a pass over the rows to build the array.

**Link** walks the records of one chunk from a given place to its used
mark and puts each of them into its bucket. First it adds their number
to the count in the header, all at once, with one atomic addition: a
fetch-and-add, which always succeeds, not a compare-and-swap. Then it
makes each record the first of its bucket's chain with a
compare-and-swap, which fails when another process has just changed
the bucket and is then tried again ("Several processes at once" shows
the steps). Records of equal keys stay separate records in one chain.

On request link also counts duplicates: after a record is in its
chain, link walks the rest of the chain for a record with the same
keys. Of two records of one key, only the one linked later finds the
other, so the count is exact even when several processes link at once.
The table does not keep this count; the call returns it. A join in one
process adds it up in its own state. In a parallel join every
participant adds its part to a counter in shared memory that the join
keeps beside the table. When the sum is 0, every key has one record,
and the join's rows need no walk for a second match.

Why two steps and not one insert? Because a parallel join does not know
how many rows will come until all processes have read their share.
Each process appends into its own chunks first. When all are done, the
total is known, one process makes an index of exactly that size, and
then all link their chunks. The index is never too small, and it never
has to grow. A join in one process takes the same two steps.

## Looking rows up

**Probe** takes a batch: hashes, key columns and a mask of rows. For
each row it finds the first record of the chain whose hash, NULL bits
and keys are all equal to the row's. It writes the reference of that
record for the row and sets the row in a mask of rows found.

The keys are always compared, never the hash alone, because different
keys can have equal hashes. Two cases make this certain, not only
likely. An `int8` key has 64 bits and its hash 32, so many `int8` keys
share each hash. And under the rule of a grouping a NULL key hashes
like one particular number, so a NULL and that number share a hash and
differ in the slot and the NULL bit.

A key may have many records in a join. **`tess_table_next_match`**
moves each row of a batch from its record to the next record of its
chain with the same keys, and leaves in the mask only the rows that
have one. The node calls it again and again until the mask is empty. In
this way a whole batch walks its chains together, and each step is one
call, not one call per row.

Then the node reads what it needs from the records it found.
`tess_table_gather` copies one word of the payload of each found record
into an array, one call for a column of the batch.
`tess_table_gather_key` copies a key, with its NULL flag.
`tess_table_record` and `tess_table_payloads` give pointers into a
record, for the few cases that need more.

When a batch has enough rows selected (8 or more of a word of 64), the
probe goes over them together, step by step: first every row's bucket,
then every row's first record, then the next records. Before each step
it asks the processor to start fetching the memory of the step after
it. This is the "lookups by batch" goal above. With fewer rows a probe
goes row by row, which is cheaper for them.

Two more ways to read are for records in no useful order. A sort keeps
the rows it sorts as records of this format, in a table that is never
linked (`runtime/rows.c`), and gives them out in sorted order, which
jumps from record to record across its chunks. It reads them with
`tess_table_gather_scattered` and `tess_table_gather_words`, which
first find and prefetch the records of 64 rows, then read them. For a
join they would cost more than they save, since a probe has just read
its records and they are in the cache.

## A key's records together

Link puts each record at the head of its chain, so the records of one
key end up spread among the records of other keys of the same bucket.
To find the next record of a key, `tess_table_next_match` must walk
until it meets one, and then walk the rest of the chain to learn that
there is no other.

`tess_table_link_grouped` links a chunk differently: it first looks the
record's key up, and when the table holds that key already, it puts the
new record right after the record it found. All records of a key then
stand together in their chain, and `tess_table_next_in_group` reaches
the next one in one step: it looks only at the record right after.

```
 link:          bucket → D(5) → B(7) → C(5) → A(5)
 link_grouped:  bucket → D(9) → A(5) → C(5) → E(5) → B(7)
                                └─ key 5 together ─┘
```

Each new record of a key goes right after the first one, so after the
first the records of a key lie in the reverse of the order they were
linked. The lookup per record makes grouped linking slower than link,
and it changes the link of a record that is already in a chain, which is
safe only for one writer. So a join in one process links its table
grouped, and a parallel join, whose processes link at once, uses plain
link.

## One writer

Some calls need the table to themselves, with no other call over it at
the same time. A grouping, the output of a grouping and a new index are
such cases. They are safe in one process, and in a parallel plan only
over a table that no other process uses.

- **`tess_table_find_or_insert`** gives each row of a batch the record
  of its keys. When the table has no such record, the call makes one in
  the chunk the caller names, with a payload of zeros, and links it at
  once, so that a later row of the same batch finds it. A mask tells the
  caller which rows got a new record, so that it can set up their
  aggregate states. The call stops at the first row that needs a new
  record when the chunk is full, or when the records reach half the
  buckets. That row and the rows after it stay in the mask; the caller
  adds a chunk or makes a larger index and calls again.
- **Payloads change in place.** The grouping finds a row's record and
  adds the row's values to the states in its payload. One writer also
  changes the links between records, when it links grouped or makes a
  new index, and a key, when it clears one; in a shared table a
  published record never changes.
- **`tess_table_scan`** visits the records chunk by chunk, in the order
  they were appended, from a cursor the caller keeps between calls. A
  grouping returns its groups this way.
- **`tess_table_regrow`** moves the table to a new, larger index over
  the same chunks. It clears the new buckets and links every record of
  every chunk it is given again, grouped. No record moves, so every
  reference a node holds stays valid. For a moment both indexes exist,
  4 bytes a bucket each; then the caller frees the old one. An index
  with fewer buckets is refused. Since every chunk given is linked, a
  node that holds a chunk of other records, such as groups read back
  from disk, gives an empty chunk under that number for the call.
- **`tess_table_clear_key`** writes 0 into one key of every record,
  keeping its NULL bit. A sort keeps its rows as records but never
  looks them up; when it gives up a key's short form, it clears it this
  way.

A walk over the records reads every record below each chunk's used
mark. An append in another process moves that mark while it writes, so
a walk is also a call for one writer.

## Several processes at once

In a parallel join the table lies in shared memory and several
processes, the participants, use it. What they may do at the same time:

- **Append** at once, each into chunks of its own. A chunk has one
  writer, so its used mark and its records need no atomic operation.
- **Link** at once, each its own chunks. Two links of one bucket meet
  only at the bucket's head, and the compare-and-swap decides their
  order.
- **Probe** at once, once the linking is over.
- Not the calls of one writer: they assume that nothing else happens.

PostgreSQL's barrier keeps these apart: every participant appends, all
wait, one makes the index, all wait, every participant links, all wait,
and then all probe. Which phase comes next is the table's C API too,
and "A shared build" below explains it.

Only two places of the table are changed by several processes, and
both are changed by atomic operations: the count of records in the
header, by an addition, and the buckets, by a compare-and-swap. The
bytes of a record are plain memory. They are written by one process
before the record is published, and read by the others only after. A
link and a probe go in these steps; the memory order of each atomic
operation is in
[Calls at the same time](spec.md#requirement-calls-at-the-same-time).

```
 link, for a chunk:
   1. add the chunk's records to the count, one atomic addition
   2. for each record of the chunk:
        read the bucket
        write the bucket's first record into the record's next
        swap the bucket from that record to this one, if it still
        holds it; if not, take the new first record and repeat

 probe, for a row:
   1. read the bucket: the first record of the chain
   2. for each step down the chain:
        check the reference against its chunk
        read the record: hash, keys, next
        past the count of steps?  read the count again;
        still past it: the chain has a loop
```

The swap that publishes a record is a release, and the read of the
bucket that finds it is an acquire. Together they make a process that
finds a record see every byte written into it before the swap.

The count goes up before the records are published. A probe that finds
a new record therefore also sees a count that covers the record's
chain. If the order were the other way round, a probe could see a chain
longer than the count and call a sound table corrupt. A model of the
table found exactly this, and the order was fixed.

That model is `make rust-loom`. Loom runs a test many times, in every
order the memory model allows. The test uses the table's own code over
a model of the index and the chunks, with the same memory orders as the
real code. It checks that two or three participants that link into one
bucket lose no record and count duplicates exactly, and that a probe
that finds a record being published reads it whole. Negative tests
check that the model catches mistakes. With relaxed reads of the
buckets a probe reads a record that is not written yet. With the count
raised only after a record is published, the order once found wrong, a
probe takes a sound chain for a loop. The model makes that mistake
itself, holding the link's addition back until the record is in its
bucket, so that the table's code keeps one order only.

## What we decided not to do

- **One block that grows.** The first form of the table kept the header,
  the buckets and the records in one block. To grow, a node in one
  process moved it with `repalloc`, and a shared table was copied into a
  larger block. Under a low estimate the whole table was copied, with
  the old and the new block held at once. Chunks that never move removed
  the copy.
- **Growing a shared table while it is built.** PostgreSQL's own
  parallel hash join does this: when the buckets are too few, all
  processes stop at a barrier, the array of buckets doubles, and every
  tuple is moved to its new bucket, perhaps several times. Tessera's
  join appends first and counts, and makes the index once, of the right
  size.
- **References of 64 bits**, such as `dsa_pointer`: see "References".
- **A filter in each bucket.** A bucket of 64 bits held the head and a
  few bits of the hashes of its chain, so that a lookup could skip a
  chain without the key. It made a miss in a full bucket about twice as
  fast, but a miss in an empty bucket 41 percent slower, a hit 12
  percent slower, and the buckets twice as large. Joins on a foreign key,
  where most rows find their pair, lost. It was taken out; a join whose
  rows mostly find no pair checks them against a Bloom filter instead.
- **SIMD comparisons of the gathered records.** A probe collects the
  fields of the records it reaches one by one, with narrow stores, and
  a wide SIMD load right after such stores has to wait for them. With
  SIMD comparisons a probe of a table in cache took twice the cycles,
  and against the compiler's own loops they saved 1.5 percent.
- **A mode with plain stores for one process.** The compare-and-swap was
  measured in a plan of one process and was not the main cost of an
  insertion, so one code serves both.
- **The hash as the key.** See "Looking rows up".

## Files

- `crates/relhash/src/table/mod.rs`: the table, its calls and
  their rules
- `crates/relhash/src/table/chunks.rs`: the chunks, their checks,
  and the calls over chunks alone: appending before the table has an
  index, appending and splitting by partition
- `crates/relhash/src/table/header.rs`: the header of the index,
  the limits of chunks and references
- `crates/relhash/src/table/record.rs`: a record, the walk down a
  chain and over a chunk's records, publishing
- `crates/relhash/src/table/region.rs`: the memory of a table and
  its atomic operations, with their memory orders
- `crates/relhash/src/table/batch.rs`: a batch of rows; append,
  link, probe and the reads of found records; append and split by
  partition
- `crates/relhash/src/table/lanes.rs`: the probe of a word of rows,
  step by step for all its rows at once, and its prefetch hint
- `crates/relhash/src/table/writer.rs`: the calls of one
  writer, the lookup by partition, the cursor over payloads and the
  merge by a rule
- `crates/relhash/src/table/keys.rs`: the keys of a batch, a
  word of rows at a time
- `crates/relhash/src/table/owned.rs`: a table that owns its
  memory, for tests and benchmarks
- `crates/relhash/src/table/loom/`: the memory of the loom model,
  the model table the models of the phases and the rounds run over,
  and the models of the table, the filter and the marks under several
  participants
- `crates/relhash/src/table/testing.rs`: the keys of the table's own
  tests and loom models

# hash-table: how it is built

A hash join and a grouping both need to find rows by a key fast.
Tessera keeps such rows in a hash table of its own. The table is
written in Rust, in the kernels, and the nodes in C call it through
`tessera/table.h`.

This page explains what the table is made of, how a batch of rows goes
in and how it is looked up, how the table grows, and how several
processes use one table at once. The exact bytes, limits and errors are
written once, in the table's spec, [spec.md](spec.md) next to this
page. Where this page leans on a rule of the spec, it links to the
requirement that states it. What a join or a grouping does with the
table is described with those nodes.

## Background: keys, hashes, buckets, chains

A **key** is the value a row is found by: in a join, the column of the
join condition; in a grouping, the columns of GROUP BY. A key may have
several columns.

A **hash** is a number of 32 bits made from the key. Equal keys always
have equal hashes. Different keys usually have different hashes, but
not always.

The table keeps each column of a key in a word of 8 bytes and compares
keys bit for bit. It knows two kinds of key: `int4`, widened to 8 bytes
with its sign, and `int8`. A node puts a key of another type in as one
of these two kinds:

- `int2`, `bool` and `date` go in as `int4`, and `timestamp` and
  `timestamptz` as `int8`. Their values are whole numbers, and two of
  them are equal exactly when their bits are.
- A key of any other type whose equality has a hash function, such as
  `text` or `numeric`, goes in as a number of 64 bits that stands for
  the value. A grouping gives each distinct value a number of its own,
  from a dictionary that the node keeps. A join puts in a hash of the
  value, 64 bits made by the type's own hash function, as an `int8`.
  Two different values may have the same hash of their type, so when
  the table finds a match, the join still compares the values
  themselves.

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

## The problem

A table in a database engine has to answer some hard questions. These
are the ones Tessera's table answers.

1. A plan that runs in one process keeps a node's memory in a
   PostgreSQL memory context. A parallel hash join keeps the build side
   in dynamic shared memory, which every process maps at an address of
   its own. An address that one process stores means nothing to
   another. How can one table, and one code, serve both?
2. The planner guesses how many rows will come, and the guess can be
   wrong by a hundred times. How does the table grow without copying its
   records, and without holding an old and a new copy at once?
3. In a parallel join several processes put rows into one table at the
   same time. How do they do it without a lock? How does a process that
   finds a record know that the record is whole?
4. The table's memory is bytes that a node hands over. A wrong call or a
   bug elsewhere may leave wrong bytes there. How does the table avoid a
   crash of the server, or a loop that never ends?
5. A join needs every record of a key. A grouping needs exactly one
   record of a key, made the first time the key comes. How does one
   table serve both?
6. A table larger than the processor's caches makes almost every look
   at a record a wait for memory. How can a batch of lookups hide these
   waits?
7. Rust code must not raise a PostgreSQL error, and memory it allocated
   would be memory PostgreSQL does not count. How does a call say "this
   chunk is full" or "the buckets are too few"?

## Goals and what they cost

Each goal answers one of the questions above. Each has a price, and the
price is written next to it. The numbers were measured on the
developer's machine, an Apple M5 Pro; the target platform is Linux on
x86-64, where they have not been measured yet.

- **No address inside the table** (question 1). The caller allocates
  every block of the table and hands the blocks to each call. A record
  is found by a *reference*: the number of its block and its place in
  the block, 32 bits in all. A process turns a reference into an address
  through its own list of the blocks. The same bytes then mean the same
  in every process. The price is one more read per record, of the
  block's address from that list, which is almost always in the
  processor's fastest cache.
- **Records never move** (question 2). Records lie in blocks called
  chunks. When a chunk is full, the caller adds another. The buckets
  are made for a number of records, the table's capacity. A grouping
  that meets more groups than the planner expected fills its buckets:
  when its records reach half the buckets, the chains would start to
  grow long. Then only the array of buckets is made anew, larger, over
  the same chunks (see "One writer"). A join never needs this, since it
  makes its buckets once, when it knows how many rows it has. The
  price, against an older form of the table that kept everything in one
  block and copied it to grow: about 6 to 7 percent more instructions
  for a lookup in cache, 22 to 30 percent more for an insertion, and 2
  to 4 percent more time for a join in SQL. The gain: under an estimate
  200 times too low, a join used 13 to 34 percent less memory, since
  nothing is ever held twice.
- **No lock** (question 3). A process writes records only into chunks
  of its own. It then makes each record visible with one atomic
  compare-and-swap on the record's bucket. A process that reads the
  bucket with the matching memory order sees the whole record. The same
  code runs in a plan of one process, where the compare-and-swap never
  fails. Its price there was measured too, and it was not the main cost
  of an insertion.
- **Wrong bytes are an error, not a crash** (question 4). Every call
  checks the whole header of the table. Every reference is checked
  against its chunk before it is followed. A walk down a chain takes at
  most as many steps as the table has records, so a chain that loops is
  found. The price is a fixed part of every call, a few hundred
  instructions, and one comparison per step.
- **Two ways to put a row in** (question 5). A join *appends* rows,
  and equal keys make separate records. A grouping asks to *find or
  insert*: each row gets the one record of its key, which is made when
  missing.
- **Lookups by batch** (question 6). One call looks up all the rows of
  a batch. When enough rows are selected, the call goes step by step
  over all of them together: it reads every row's bucket, then every
  row's first record, and so on, asking the processor to fetch the next
  memory early. The waits of different rows then overlap. On a table far
  larger than the caches a lookup that finds its key went from about 75
  to about 38 processor cycles a row. On a table that fits in the
  fastest cache it costs about half as much again as a lookup of one
  row at a time. That price was accepted: the tables of real joins are
  larger than the cache.
- **Nothing kept, nothing raised** (question 7). Rust keeps nothing
  between calls, and the table's memory is all the caller's. A call
  allocates only for itself: the text of an error, and the list of the
  columns of a payload wider than 64 columns. A row that does not fit
  stays in the call's mask of rows still to place; the caller adds a
  chunk, or makes a larger index, and calls again. Every error is a
  status that the call returns, and the caller raises it as a
  PostgreSQL error after the call.
- **Short chains.** The table has at least twice as many buckets as the
  records it was made for, and at least 1024. A grouping stops making
  records when they reach half the buckets. The price is the buckets: 8
  bytes for each record the table was made for.

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

The nodes size the index in different ways. A join in one process
appends its whole inner side first and then makes the index for exactly
the rows it got. A grouping makes its index for the planner's estimate
of the groups, no larger than its memory allows, and moves to a larger
one when its records reach half the buckets (see "One writer"). A
parallel join makes the index once, after all processes have appended
(see "Putting rows in").

## Chunks and records

A chunk is a plain block of memory. `tess_table_chunk_init` makes a
block an empty chunk by writing its used mark. Records follow the mark
one after another, all of the same size, and the mark grows by a record
each time one is written. Only the chunk's one writer appends to it.
The links of the table never leave its chunks: a bucket and the field
next of a record name only records of the same table. A payload may
name memory outside the table, as "Values longer than a word" shows
below.

The nodes make the first chunk of a table small, 64 kB, so that a
table of a few rows takes little memory. Later chunks are larger: an
eighth of the node's memory limit, but at most 1 MiB, the limit of a
chunk. A node may also pass a chunk of 8 bytes, its used mark alone,
where it has no chunk to give for a number: nothing fits in it, so
nothing is ever written there.

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

### Values longer than a word

All records of a table have one size, so a record cannot hold a value
whose length varies, such as a string. A key never needs to: a key of
such a type goes in as a number of 64 bits (see "Background"). A
payload keeps such a value outside the table. The node copies the
value's bytes into blocks of its own, its **chunks of values**, and the
payload's word for that column holds a reference to the copy: the
number of the chunk of values plus one, and the byte where the copy
starts. To the table this word is 8 bytes like any other. The spill
format writes the same words to disk and defines them, in [A reference
to a by-reference
value](../spill-format/spec.md#requirement-a-reference-to-a-by-reference-value).

```
 a record of a join on id that keeps the columns amount and name
 ┌────────┬────────┬───────────┬────────┬──────────────────┐
 │ header │ key    │ NULL bits │ amount │ name             │
 │        │ id: 42 │ of amount │ 1500   │ chunk of values  │
 │        │        │ and name  │        │ 0, byte 64       │
 └────────┴────────┴───────────┴────────┴────────┬─────────┘
                                                 │
 chunk of values 0, the node's own memory        ▼ byte 64
 ┌───────────────────────────────────────────┬─────────┬───────┐
 │ other values                              │ 'Smith' │ free  │
 └───────────────────────────────────────────┴─────────┴───────┘
```

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

A node meets that limit only when its own memory limit is larger. A
join or a grouping sends rows to disk when its memory reaches
`hash_mem`, and a sort when it reaches `work_mem`; none of them sends
rows to disk because its chunks are many. So with such a limit above
32 GiB a node can need a 32769th chunk first. It then stops the query
with an error of SQLSTATE `54000`, "program limit exceeded", where
PostgreSQL's own node would go on. A test of the kernels gives a call
32769 chunks and sees it refused; no test reaches the error of a node,
which would take 32 GiB of memory.

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
one and calls again. Append does not read the index at all, so it works
before there is one.

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

The join keeps the phases apart with PostgreSQL's barriers: every
participant appends, all wait, one makes the index, all wait, every
participant links, all wait, and then all probe. How the phases go is
part of the join; the table only has to make each phase safe.

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
check that the model catches mistakes: with relaxed reads of the
buckets a probe reads a record that is not written yet.

## Checks and errors

The table trusts nothing it reads, because it cannot know who wrote the
bytes last. Every call does three kinds of check.

- **The header and the chunks, once a call.** The header's fields must
  agree with each other and with the length of the index, as
  [The index of a table](spec.md#requirement-the-index-of-a-table)
  lists them. Each chunk the call is given must be aligned and of a
  length a chunk may have. The calls that append to chunks before an
  index exists check the chunks only.
- **Every reference before it is followed.** Its chunk must exist, the
  whole record must lie inside the chunk, and the record must claim the
  table's record size. The check uses the chunk's length, not its used
  mark. References come from calls over the same table, and in a
  shared table another process may be moving the used mark of its own
  chunk; reading it would race. A reference past the mark reads unused
  bytes of the chunk, never memory outside it.
- **Every walk is bounded.** A chain takes at most as many steps as the
  header counts records, and never more than the places a reference can
  name in the chunks given to the call. A chain that is longer has a
  loop in it. The second bound matters when the count itself is
  damaged: it comes from the number of chunks in the caller's own list,
  not from the table's bytes. It is loose, 2^17 places a chunk where a
  chunk holds fewer records, so that a damaged table with a loop is
  found later than it could be; in exchange it costs a shift, where a
  sum of the chunks' lengths cost every call two instructions for each
  chunk.

A failed check makes the call return an error status. It never crashes
the server and never loops. The calls check their arguments, the header
and the chunks before they change anything. A damaged record met in the
middle of a call may come after some rows were done, and a table that
gave such an error is not to be used again: the node raises the error,
and the query ends.

These errors mean a bug in Tessera or in its caller, not bad data from
a user or from a disk, so they are internal errors; the status and its
SQLSTATE are in
[Errors of a call](spec.md#requirement-errors-of-a-call). The C node
raises the error with `ereport` after the call returns, because a
PostgreSQL error must not unwind through Rust code.

The checks have a price that grows with the table: a call checks every
chunk it is given, so a table of a thousand chunks pays a thousand
checks a call, whatever the number of rows.

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

## What is not on this page

The same index, chunks and records serve more than a plain table. These
parts are described in [docs/table.md](../../../docs/table.md) until
they get their own pages:

- a table split into partitions to spill, and the calls that append,
  split and merge by partition;
- the Bloom filter of a table's keys, alone and shared;
- the marks of the inner records of a RIGHT or FULL join;
- the aggregate states that a grouping keeps in a payload, and the
  calls that fold rows into them;
- the items a sort makes from records;
- the phases of a shared build and of the rounds over partitions on
  disk.

How a spilled chunk of records is written to disk is in
[spill-format](../spill-format/design.md).

## Files

- `crates/tessera-kernels/src/table/mod.rs`: the table, its calls and
  their rules
- `crates/tessera-kernels/src/table/header.rs`: the header of the index,
  the limits of chunks and references
- `crates/tessera-kernels/src/table/record.rs`: a record, the walk down a
  chain, publishing
- `crates/tessera-kernels/src/table/region.rs`: the memory of a table and
  its atomic operations, with their memory orders
- `crates/tessera-kernels/src/table/batch.rs`: append, link, probe and
  the reads of found records
- `crates/tessera-kernels/src/table/exclusive.rs`: the calls of one
  writer
- `crates/tessera-kernels/src/table/keys.rs`: the keys of a batch, a
  word of rows at a time
- `crates/tessera-kernels/src/table/local.rs`: a table that owns its
  memory, for tests and benchmarks
- `crates/tessera-kernels/src/table/loom.rs`: the model of several
  participants
- `crates/tessera-capi/src/c/table.rs`: the C entry points
- `include/tessera/table.h`, `include/tessera/table_key.h`: the C API

## Tests

- `crates/tessera-kernels/tests/table.rs`: the format and every call,
  with damaged headers, chunks, references and chains; it also runs
  under Miri.
- The unit tests in `crates/tessera-kernels/src/table/mod.rs`: blocks
  that are not chunks or an index.
- `crates/tessera-kernels/src/table/loom.rs`, run by `make rust-loom`.
- `crates/tessera-capi/tests/table.rs`: the entry points as C calls
  them, Datum and dense key columns alike.
- `test/sql/table.sql` with `test/tessera_table_test.c`: the C API
  from a PostgreSQL backend, and the check of the layout when the
  kernels load.
- The benchmarks `table_int32` and `table_large` of `tessera-bench`
  count the instructions and cycles of append, probe and find-or-insert.

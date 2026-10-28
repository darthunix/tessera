# relhash: how it is built

A hash join and a grouping both need to find rows by a key fast. The
crate `relhash` is the hash table they keep such rows in. It is
written in Rust, takes all its memory from its caller, and works the
same in one process and in memory that several processes share.
Tessera's C nodes call it through `tessera/table.h`.

This page explains where the crate ends and its caller begins, what
the table is made of, how a batch of rows goes in and how it is looked
up, how a caller reaches its payloads, how the table grows, and how
several processes use one table at once. Then it explains what the
crate gives a caller to keep beside the table: partitions when the
table spills, a Bloom filter of its keys, and the marks of a RIGHT or
FULL join. Last, it explains how a call fails, and what shows that
bytes which are not a table give an error and never a crash. The
exact bytes, limits and rules are written once, in the documentation
of the crate's items, which the crate's spec, [spec.md](spec.md) next
to this page, links to. Where this page leans on a rule, it links to
the source file whose documentation states it. How Tessera's
nodes use the table, the steps of a shared build, the words of a spill
and the levels of partitions, and what a call checks, are explained in
the design of [hash-table](../hash-table/design.md).

## Background: keys, hashes, buckets, chains

A **key** is the value a row is found by: in a join, the column of the
join condition; in a grouping, the columns of GROUP BY. A key may have
several columns.

A **hash** is a number of 32 bits made from the key. Equal keys always
have equal hashes. Different keys usually have different hashes, but
not always. The caller computes the hash; the table only uses it.

The table keeps each column of a key in a word of 8 bytes and compares
keys bit for bit; it gives a word no type. The caller fills the words
through a **source of keys**, `KeySource`, a word of 64 rows at a
time. Tessera's key columns fill them in two forms: an `int4`, widened
to 8 bytes with its sign, and an `int8`. A key of another SQL type
goes in in one of these two forms, either as its value or as a number
that stands for it, as [Keys of each SQL
type](../hash-table/design.md#keys-of-each-sql-type) in the design of
`hash-table` tells.

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

The table works on **batches**: one call puts in, or looks up, all the
rows of a batch, kept by column: a hash for every row, the source of
its keys, and a mask of the rows to work on. Tessera passes rows
between its nodes in batches of up to 64.

A **mask** says which rows of a batch a call works on: one bit for each
row, 1 for a row the call takes and 0 for a row it leaves alone. The
bits are kept in words of 64 bits, row `r` in bit `r % 64` of word
`r / 64`, so a batch of 64 rows has a mask of one word. In the crate a
mask is a `RowMask`, whose words a call may change, or a
`RowMaskView`, whose words it only reads: the number of rows and the
words.

```
 row    0   1   2   3   4   5   …  63
 bit    1   0   1   1   0   1   …   0     one word of 64 bits
 rows 0, 2, 3 and 5 take part; rows 1 and 4 do not
```

A filter clears the bits of the rows it drops. The calls of the table
read such a mask and narrow masks of their own: the rows still to
place, the rows found, the rows that got a new record.

## The problem

A table in a database engine has to answer some hard questions. These
are the ones this table answers.

1. A plan that runs in one process keeps a node's memory in the
   engine's memory of that process. A parallel hash join keeps the
   build side in shared memory, which every process maps at an address
   of its own. An address that one process stores means nothing to
   another. How can one table, and one code, serve both?
2. The planner guesses how many rows will come, and the guess can be
   wrong by a hundred times. How does the table grow without copying its
   records, and without holding an old and a new copy at once?
3. In a parallel join several processes put rows into one table at the
   same time. How do they do it without a lock? How does a process that
   finds a record know that the record is whole?
4. The table's memory is bytes that its caller hands over. A wrong call
   or a bug elsewhere may leave wrong bytes there. How does the table
   avoid a crash of the server, or a loop that never ends?
5. A join needs every record of a key. A grouping needs exactly one
   record of a key, made the first time the key comes. How does one
   table serve both?
6. A table larger than the processor's caches makes almost every look
   at a record a wait for memory. How can a batch of lookups hide these
   waits?
7. The engine counts the memory of a query and raises its errors in a
   way of its own: PostgreSQL's error must not pass through Rust code,
   and memory the table allocated would be memory PostgreSQL does not
   count. How does a call say "this chunk is full" or "the buckets are
   too few"?
8. A table may not fit in the memory its caller is given. The caller
   then writes some records to disk and reads them back later, a part
   at a time, and a part read back may still be too large. How does
   the table sort its records into such parts?
9. In many joins most rows of the probe side find no pair, and each
   still costs a read of its bucket, a wait for memory on a large table.
   How can such a row be dropped without reading the table?
10. A RIGHT or FULL join also returns the records that no row matched.
    The records are shared and read by every process. Where does the
    join keep which records found a pair?

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
  instructions, and one comparison per step. [Checks and
  errors](../hash-table/design.md#checks-and-errors) in the design of
  `hash-table` explains each check.
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
- **Nothing kept, nothing raised** (question 7). The crate keeps nothing
  between calls, and the table's memory is all the caller's. A call
  allocates nothing; the list of the columns of a payload, up to 2048
  of them, is kept on the stack. A row that does not fit stays in the
  call's mask of rows still to place; the caller adds a chunk, or makes
  a larger index, and calls again. Every error is a value of one word
  that the call returns, and Tessera raises it as a PostgreSQL error
  after the call.
- **Parts by the low bits of the hash** (question 8). A table that
  spills keeps its records in partitions, chosen by the low bits of the
  hash already in each record, each with chunks of its own. A part read
  back that is still too large is read a chunk at a time and split by
  the next bits, so it never has to fit in memory whole. The price is a
  copy: the records appended before the table spilled are copied once
  into their partitions, and once more at each split.
- **A filter checked with one read** (question 9). A join may check its
  rows against a Bloom filter of the table's keys first. Each key sets
  four bits of the filter, and all four lie in the same word of 64
  bits, so checking a row reads one word. The price is 16 bits a record
  at least, and the rows that pass falsely, about one in 200 at 16 bits,
  which then probe the table as before.
- **Marks beside the records** (question 10). A RIGHT or FULL join keeps
  a bit for each record in words of its own and sets the bits of the
  records its pairs matched, by an atomic OR in a shared table. The
  records stay as they were written. The price is a bit a record, and
  in a shared table the words of a full chunk for every chunk.
- **Short chains.** The table has at least twice as many buckets as the
  records it was made for, and at least 1024. A grouping stops making
  records when they reach half the buckets. The price is the buckets: 8
  bytes for each record the table was made for.

## The whole in one picture

A table is one **index** and any number of **chunks**. The caller
allocates all of them and passes them to every call: the address and
length of the index, and a `Chunks`, the address and length of each
chunk, by number.

```
 in each process: index, Chunks     in memory, the same for all

 index ──────────────────────────► ┌──────────┬────────────────────┐
 index_len                         │ header   │ buckets            │
                                   └──────────┴──────────┬─────────┘
                                                         │
 bases[0] ───────────────────────► ┌──────┬────────┬─────▼──┬──────┐
 lens[0]                           │ used │ record │ record │ free │
                                   └──────┴────────┴──┬─────┴──────┘
                                                      │ next
 bases[1] ───────────────────────► ┌──────┬────────┬──▼─────┬──────┐
 lens[1]                           │ used │ record │ record │ free │
                                   └──────┴────────┴────────┴──────┘
 …
```

The index holds the buckets. A bucket holds the reference of the first
record of its chain, and each record holds the reference of the next.
The references are numbers, not addresses, so the right half of the
picture is the same in every process; only the left half differs.
"References" below shows how a reference is made and followed, with a
chain as an example.

The caller holds the table through one of three handles. A `Table` is
a handle that other handles may share: its calls append to chunks of
the caller's own, link, and look rows up; the calls that write are
`unsafe`, since the caller promises that each chunk has one writer. A
`TableMut` is the table's **one writer**, which has the table to
itself: it may also find or insert, change payloads and move to a
larger index. A `ChunkWriter` appends to chunks before there is an
index. The handles over the caller's memory are made by `unsafe`
calls, whose duties say what the caller promises of that memory. An
`OwnedTable`, behind the feature `alloc`, owns its blocks and needs no
`unsafe`.

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
- **Participant**: one process of several that work on a shared table.

## The crate and its caller

The crate holds what the hash join and the grouping of any engine
need, and nothing that knows SQL or the executor of an engine. That
line decides what lies on each side.

- **In the crate**: the index, the chunks and the records; the calls of
  a batch and of one writer; the partitions of a table that spills; the
  Bloom filter of the keys; the marks of a RIGHT or FULL join; and the
  masks of rows, the crate's own types for its batches.
- **With its caller**: what a payload means and how it changes, such as
  the states of aggregates and their sums; the items of a sort; reading
  keys from the caller's columns; the steps of a build by several
  processes; and the files of a spill. Tessera's kernels keep all of
  these, over the calls this page describes.

The crate depends on no crate of Tessera. The masks of rows belong to
the crate, since its calls take them. Tessera's kernels use them too,
through `tessera-core`, which exports them again. So `tessera-core`
depends on the crate, which may look upside down. A third crate for
the masks alone was refused: about 450 lines, and one more crate to
keep.

A view of a mask is its count of rows and its words. Arrow's bitmaps
can also start at a bit inside a byte. No source of Tessera gives such
a mask, so the crate has no such form; it can come with a source that
needs it. A read of a word past a mask's words panics. A check there
would cost 4 to 9 instructions a word in the loops of Tessera's
kernels. A mask is the caller's argument, not the table's bytes, so
the promise over any bytes does not cover it.

The table gives a key no type. A table is made for a count of keys and
a payload size, a `TableConfig`. Each key is a word of 8 bytes
compared bit for bit, and the source of keys puts a value into that
word. The caller keeps the kinds of its keys: Tessera's nodes keep
them to read key columns and to make the items of a sort. A byte for
each key, kept in the header but never read by the table, was refused:
a field of the format that the table itself does not use. So the crate
holds no name of a PostgreSQL type.

The crate builds for 64-bit targets only. Its arithmetic of offsets
takes a `usize` of 64 bits: the bytes of the buckets, 4 for each of up
to 2^31 buckets, would wrap on a 32-bit target and pass the check of
the index. Rather than check every such product, the crate refuses to
build on another target, with an error that says why.

A caller's loops call the crate's hot calls many times a batch.
Tessera builds without link-time optimization, and then the compiler
inlines a function of another crate only when it is generic or marked
`#[inline]`. The hot calls are generic or marked so, and a loop of the
caller compiles as if the table were its own code. With link-time
optimization, which the README advises, the rest can be inlined too.

The API is small and plain. Its items lie at the crate's root. A
reference stays a `u32`: a type of its own would be converted by every
array that crosses into C. The calls over the caller's memory take raw
pointers and lengths, not slices: in shared memory other processes
write while a call runs, which a slice forbids.

## The index

The index is made once for a number of records, its capacity:
`index_size` says how many bytes it needs, the caller allocates them,
and `TableMut::create` writes the header and clears the buckets. The
documentation of
[`HEADER_SIZE`](../../../crates/relhash/src/table/header.rs)
draws the header field by field.

Most fields of the header are written once, when the index is made,
and never change. One field changes while the table is used: the count
of the records linked into the buckets. It is there for two reasons:
`Table::stats` reports it to the caller, for its own rules and for
EXPLAIN in Tessera, and a walk down a chain uses it as its bound.

The bucket of a hash is its high bits. The low bits are left free on
purpose. A caller whose table does not fit in memory splits its rows
into partitions by the low bits of the hash. Inside one partition the
low bits are all the same, but the high bits still spread the rows over
every bucket. So a hash whose high bits do not vary from key to key,
such as small integers taken as their own hashes, puts the rows into a
few chains.

The index has at least twice as many buckets as its capacity, and at
least 1024. With at most one record for every two buckets, most chains
hold one record or none. A table may hold more records than its
capacity; it stays correct, only slower.

## Chunks and records

A chunk is a plain block of memory. `Chunks::init` makes a block an
empty chunk by writing its used mark. Records follow the mark one after
another, all of the same size, and the mark grows by a record each time
one is written. Only the chunk's one writer appends to it. The links of
the table never leave its chunks: a bucket and the field next of a
record name only records of the same table. A payload may name memory
outside the table: Tessera keeps a string in chunks of its own, as
[Values longer than a
word](../hash-table/design.md#values-longer-than-a-word) shows.

A record has a fixed size for the whole table: a short header, a slot
for each key, then the payload. The documentation of
[`Record`](../../../crates/relhash/src/table/record.rs) draws it
byte by byte. Why each piece is there:

- **The hash** is compared before the keys. Two different keys in one
  chain usually have different hashes, so most wrong records are
  skipped after one comparison of 32 bits.
- **A slot of 8 bytes for every key.** A caller with narrower keys
  widens them: Tessera widens an `int4` with its sign. An `int8` that
  fits in an `int4` hashes like that `int4`, so a join of an `int4`
  column with an `int8` column puts both into one table and compares
  them slot by slot.
- **The NULL bits.** A grouping puts NULL keys into a group of their
  own. A NULL key's slot holds 0, which its source of keys gives, and
  its bit in the record's header says that it is NULL, not the number
  0. Where the bits lie is drawn in the documentation of
  [`Record`](../../../crates/relhash/src/table/record.rs).
- **The record's own size** gives every call a cheap check that a
  reference points to a record of this table.
- **The payload** is the caller's. The table writes it when a record is
  appended (zeros when the caller gives none), and only the one writer
  changes it later.

## References

A reference names a record in 32 bits. The high 15 bits are the number
of the record's chunk. The low 17 bits, `UNIT_BITS`, are the record's
place in the chunk, in units of 8 bytes: every record starts at a
multiple of 8, so its byte is the place times 8. The documentation of
[`Chunks`](../../../crates/relhash/src/table/chunks.rs) draws
this split. The split sets
the limits. 2^17 places of 8 bytes make a chunk of at most 1 MiB, and
15 bits number at most 32768 chunks, so one table holds at most 32 GiB
of records.

References lie in two places: a bucket holds the reference of the first
record of its chain, and the field next in the header of every record
holds the reference of the record after it. To follow a reference, a
process splits it, takes the address of the chunk from its own
`Chunks`, and adds the byte. In the example below a record takes 40
bytes, so the records of a chunk start at bytes 8, 48, 88 and so on. A
chain of two records goes from chunk 1 to chunk 0:

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

Before a call reads a record, it checks the reference against its
chunk.

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
    │  append: writes the chunk only
    ▼
 records in a chunk, which nobody finds yet
    │  link: writes the buckets
    ▼
 records in their chains, which probes find
```

**Append** writes the rows of a batch as records into one chunk, in row
order, while whole records fit. A batch, `Batch`, comes with a hash
for every row, the source of its keys, a mask of the rows to place, and
a slot for each row's reference. Each row that is written leaves the
mask and gets the reference of its record. The rows still in the mask
need another chunk: the caller adds one and calls again. A
`ChunkWriter` appends without an index, for the keys and payload size
it is made with, so it works before there is one. An append through a
`Table` or a `TableMut` checks the index as every call does and
refuses keys or a payload size other than the table's. A caller's
mistake then fails where it is made, and is not found later as a
damaged table.

A batch is one value that the caller makes once and gives to call
after call. It holds the mask of the rows still to place by value,
beside the hashes, the keys and the references, and is made from the
caller's words or from a mask. The mask is where the next call goes
on. A call into one chunk places a run of rows from the start, and a
call by partition leaves rows here and there, wherever a partition's
chunk was full: a mask says both in one way. A row number to go on
from was refused: it would do for one chunk but not for partitions,
and two ways to go on are more to learn than one. So was a call that
asks its caller for a new chunk and never stops: it would have to make
a larger index under itself, and spill a partition from inside the
call, far more to pass across the boundary into C than a mask.

The payload of a record comes either from an array that the caller
filled, or, with `append_columns`, straight from the batch's columns,
`PayloadColumns`, in the form that its documentation,
[`PayloadColumns`](../../../crates/relhash/src/table/record.rs),
gives. The second form saves
the caller a pass over the rows to build the array.

**Link** walks the records of one chunk from a given place to its used
mark and puts each of them into its bucket. First it adds their number
to the count in the header, all at once, with one atomic addition: a
fetch-and-add, which always succeeds, not a compare-and-swap. Then it
makes each record the first of its bucket's chain with a
compare-and-swap, which fails when another process has just changed
the bucket and is then tried again ("Several processes at once" shows
the steps). Records of equal keys stay separate records in one chain.

`link_counting` also counts duplicates: after a record is in its
chain, it walks the rest of the chain for a record with the same keys.
Of two records of one key, only the one linked later finds the other,
so the count is exact even when several processes link at once. The
table does not keep this count; the call returns it. A join in one
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

**Probe** takes a batch: hashes, a source of keys and a lookup,
`Matches`: a mask of rows and a slot for each row's reference. For
each row it finds the first record of the chain whose hash, NULL bits
and keys are all equal to the row's. It writes the reference of that
record for the row and keeps the row in the mask; a row without a
record leaves it. The call returns the rows left.

The keys are always compared, never the hash alone, because different
keys can have equal hashes. Two cases make this certain, not only
likely. An `int8` key has 64 bits and its hash 32, so many `int8` keys
share each hash. And under the rule of a grouping a NULL key hashes
like one particular number, so a NULL and that number share a hash and
differ in the slot and the NULL bit.

A key may have many records in a join. **`next_match`** moves each row
of a lookup from its record to the next record of its chain with the
same keys, and leaves in the mask only the rows that have one. The
caller calls it again and again until the mask is empty. In this way a
whole batch walks its chains together, and each step is one call, not
one call per row.

Then the caller reads what it needs from the records it found.
`gather` copies one word of the payload of each found record into an
array, one call for a column of the batch. `gather_key` copies a key's
word, with its NULL flag. `record` gives a record's fields, and the one
writer's `payload_ptrs` gives pointers to payloads, for the few cases
that need more.

A lookup is one value, as in the batch hash tables of other engines,
such as DuckDB's `ScanStructure` and Velox's `HashLookup`. It holds one
mask of the rows still looking and, at each row, the reference of its
record. Every call narrows that one mask in place: a probe keeps the
rows that find a record, `next_match` those that find one more, and
`bloom::probe` those that the filter lets through, before the table's
probe. Each call returns how many rows are left, so a walk over the
records of a key is `while table.next_match(&mut matches)? > 0`. The
reads of found records take the lookup too, so the references and the
rows they read never disagree; a caller whose references come from
elsewhere, such as the items of a sort, makes a lookup of them. The
hashes and the keys stay arguments of the probe, since no read needs
them.

Two other forms were refused. Velox, Arrow, DataFusion and Polars give
their joins a list of pairs of a row and a record, with a limit and a
place to go on from. Here a lookup moves each row one record a call,
and its references are the place it goes on from. And a lookup of two
masks, the rows asked and the rows found, would only make each caller
copy the result of one call into the rows of the next.

When a batch has enough rows selected (8 or more of a word of 64), the
probe goes over them together, step by step: first every row's bucket,
then every row's first record, then the next records. Before each step
it asks the processor to start fetching the memory of the step after
it. This is the "lookups by batch" goal above. With fewer rows a probe
goes row by row, which is cheaper for them. The hint to fetch is one
instruction on AArch64, which reads nothing and never faults; on other
processors, x86-64 among them, the crate gives no hint yet.

Two more ways to read are for records in no useful order. Tessera's
sort keeps the rows it sorts as records of this format, in a table
that is never linked, and gives them out in sorted order, which jumps
from record to record across its chunks. It reads them with
`gather_scattered` and `gather_words`, which first find and prefetch
the records of 64 rows, then read them. For a join they would cost
more than they save, since a probe has just read its records and they
are in the cache.

Every walk down a chain ends, even over damaged bytes. A walk takes at
most as many steps as the table has records, so a chain that loops is
found. Over a table of many chunks that bound can be very large: under
a damaged count of records a loop would take hours a batch. So a walk
past 64 steps also looks for a loop by Brent's method: it saves the
place it reached at each doubling of its steps, and compares each step
with that place. A function out of line runs the method, and a word's
probe hands it only the rows still walking, so the short chains of a
sound table keep their loop of one comparison a step. Against a walk
bounded by the count alone it costs a sound table little, and the cost
was taken: a found row 0.6 to 0.9 instructions fewer, a probe 5 to
8 instructions more a call, and a find or insert that finds few rows 36
to 56 more a call, out of 430 to 730. The access could keep the bound
of 64 steps as it reads the count, which saved 19 instructions a call,
but it took a register from the walk, which then read a pointer from
the stack at each step: 1.2 instructions more a found row. It was
refused.

That bounds one search, but not a walk over all the matches of a key,
which a join makes one call at a time: a record whose next field names
itself would give the same match call after call, each call bounded. So
a lookup counts the steps its rows took to a next match or a next in a
group. A lookup can take no more of them than the table has records,
so a step past that count fails as a damaged chain: one comparison a
call, not a row. A caller that keeps a lookup's references between
calls, as C does, keeps the count with them, `Matches::with_steps`.
Leaving the bound to the caller was refused: every join would have to
write it again.

## A key's records together

Link puts each record at the head of its chain, so the records of one
key end up spread among the records of other keys of the same bucket.
To find the next record of a key, `next_match` must walk until it meets
one, and then walk the rest of the chain to learn that there is no
other.

`link_grouped` links a chunk differently: it first looks the record's
key up, and when the table holds that key already, it puts the new
record right after the record it found. All records of a key then
stand together in their chain, and `next_in_group` reaches the next
one in one step: it looks only at the record right after.

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
such cases. They are calls of `TableMut`, the one writer, and take it
by `&mut`, so that no other call goes through the same handle at once.
That no other handle uses the table's memory meanwhile is what the
caller promises when it makes the handle.

- **`find_or_insert`** gives each row of a batch the record of its
  keys. When the table has no such record, the call makes one in the
  chunk the caller names, with a payload of zeros, and links it at
  once, so that a later row of the same batch finds it. A mask tells
  the caller which rows got a new record, so that it can set up their
  aggregate states; the call adds to it, so the masks of the calls of
  one batch add up. The call stops at the first row that needs a new
  record when the chunk is full, or when the records reach half the
  buckets. That row and the rows after it stay in the mask; the caller
  adds a chunk or makes a larger index and calls again.
- **Payloads change in place.** The grouping finds a row's record and
  adds the row's values to the states in its payload, through
  `payload_mut`, `each_payload` or the cursor of `payloads`. One writer
  also changes the links between records, when it links grouped or
  makes a new index, and a key, when it clears one; in a shared table a
  published record never changes.
- **`scan`** visits the records chunk by chunk, in the order they were
  appended, from a `Cursor` the caller keeps between calls. A grouping
  returns its groups this way.
- **`regrow`** moves the table to a new, larger index over the same
  chunks. It clears the new buckets and links every record of every
  chunk it is given again, grouped. No record moves, so every
  reference a caller holds stays valid. For a moment both indexes
  exist, 4 bytes a bucket each; then the caller frees the old one. An
  index with fewer buckets is refused. Since every chunk given is
  linked, a caller that holds a chunk of other records, such as groups
  read back from disk, gives an empty chunk under that number for the
  call.
- **`clear_key`** writes 0 into one key of every record, keeping its
  NULL bit. A sort keeps its rows as records but never looks them up;
  when it gives up a key's short form, it clears it this way.

A walk over the records reads every record below each chunk's used
mark. An append in another process moves that mark while it writes, so
a walk is also a call for one writer.

## A caller's payloads

The table never reads a payload, but its caller does: a grouping adds
each row's values to the states in its group's payload, and a sort
reads the records it keeps. The crate gives such a caller three ways to
a payload. Each is generic and inlined into the caller's loop, and each
keeps the crate's access to the bytes inside the crate.

- **Cursors by reference.** A cursor is made once a call and keeps
  what the table's access keeps: the chunks and the layout of a record.
  `Table::records` gives a `RecordCursor`, which reads the record that
  a reference names. `TableMut::payloads` gives a `PayloadCursor`,
  which opens the payload that a reference names, to change. It takes
  itself by `&mut`, so two payloads are never open at once, even for
  two rows of one record. A sum that keeps the payloads of a few groups
  at hand checks a reference first, `hold`, and opens its payload later,
  `held`.
- **Pointers to payloads.** `TableMut::payload_ptrs` checks the
  reference of each selected row, as a cursor does, and writes a raw
  pointer to its payload, made from the chunk's base without borrowing
  the bytes. Two rows of one record get the same pointer, and each
  stays valid until the chunk is reset. A grouping whose rows repeat a
  key needs this. Had the crate lent each row's payload as a borrow,
  the second borrow of one record would end the first, and a write
  through the first pointer would break Rust's rules of aliasing. A
  promise that the
  references of a batch differ was refused, since a grouping cannot
  keep it. `each_payload` lends the payloads one at a time, for a
  caller that is done with one before the next.
- **A merge by a rule.** `TableMut::merge` puts the records of a chunk
  into the table by their keys. For a record whose keys the table has,
  the caller's rule, a closure, takes its payload into the payload of
  the table's record; a record of new keys is copied and linked. A
  grouping merges the groups it read back from disk so. The rule's
  error is of the caller's own type, into which the crate's errors
  convert. A rule that fails after it changed a payload leaves the
  payload as it left it, and the table is then the caller's to judge,
  not a damaged one.

The crate has no walk over records of its own: a caller walks a table
with `scan` and reads each record with a cursor. The items of a sort pay
for this. They take their references from `scan` and their keys through
the reading cursor. So each record is checked twice: by the scan, and by
the cursor, which takes the reference apart and checks the chunk again.
That is about ten instructions more a record, and a full sort in Tessera
is about 5 percent slower. A top-N sort, which reads its records by
reference anyway, pays nothing. The scan could hand out each record with
its reference, checked once, but then it would be the walk over records
that the crate does not keep. A grouping by a generic aggregate, such as
`max` of a text, pays the same way: the pointers to its payloads check
each reference again, about 25 instructions a row and 5 percent of its
time. The call that finds or inserts a row's record could hand out the
payload too, but that would change the C API for one loop. Making the
crate's inner access public and `unsafe` was refused: all of it would
then be part of what must be proven.

## The owning table

An `OwnedTable` allocates its own index and chunks. It adds a chunk
when the last one is full, and makes a larger index when its records
reach half the buckets. It is the one way to use the crate without
`unsafe` code, so the README's example uses it, and the crate's tests
and benchmarks build their tables with it. Allocating is its job, so
it stands outside the promise that a call takes no memory, behind the
feature `alloc`, on by default. A build without the feature has
nothing that allocates. Tessera's nodes never use it: their memory is
PostgreSQL's. Moving it into the tests was refused, since a caller
from outside would then have only the `unsafe` way in; so was dropping
it, which would make every test and benchmark manage raw memory.

It keeps nothing between its calls that another call could leave
wrong. An insertion links what it appended, from the used mark of the
last chunk before it, and nothing else, whatever other calls appended,
linked or found before. It links what it appended even when it returns
an error, so the rows placed before the error are found. A place up to
which the table had linked, kept between calls, was refused: every
call would have to move it. The table refuses chunks too small for one
record: with them it would add chunks up to the limit. At the most
records an index can take, it fails as a wrong call, instead of making
the same index again and again. Every handle to it borrows it, so it
may pass to another thread.

## Several processes at once

In a parallel join the table lies in shared memory and several
processes, the participants, use it, each through a `Table` of its
own. What they may do at the same time:

- **Append** at once, each into chunks of its own. A chunk has one
  writer, so its used mark and its records need no atomic operation.
- **Link** at once, each its own chunks. Two links of one bucket meet
  only at the bucket's head, and the compare-and-swap decides their
  order.
- **Probe** at once, once the linking is over.
- Not the calls of one writer: they assume that nothing else happens.

The caller keeps these apart. Tessera uses PostgreSQL's barrier: every
participant appends, all wait, one makes the index, all wait, every
participant links, all wait, and then all probe. [A shared
build](../hash-table/design.md#a-shared-build) in the design of
`hash-table` explains these steps.

Only two places of the table are changed by several processes, and
both are changed by atomic operations: the count of records in the
header, by an addition, and the buckets, by a compare-and-swap. The
bytes of a record are plain memory. They are written by one process
before the record is published, and read by the others only after. A
link and a probe go in these steps; the memory order of each atomic
operation is in the documentation of
[`Table`](../../../crates/relhash/src/table/mod.rs).

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

A caller models its own steps with loom too: Tessera models the steps
of its shared build and of its spill over the table's own code. For
such models the crate gives a model table, only when it is built with
`--cfg loom`: append, size, link and probe over loom's atomics. A
normal build has no such API. A stand-in table in the caller's models
was refused: they would lose the negative test in which linking before
the index exists breaks the table.

## Partitions: a table that spills

A caller is given a limit of memory, and its table may pass it. The
caller then keeps the records in **partitions**, writes whole chunks of
some of them to disk, and reads them back once its input is done. When
it spills, and how it writes a chunk, is the caller's: Tessera's nodes
do it as [Partitions: a table that
spills](../hash-table/design.md#partitions-a-table-that-spills) in the
design of `hash-table` tells. The crate sorts the records into
partitions.

The partition of a record is taken from the low bits of its hash: with
4 partitions, the two lowest bits. A partition read back that is still
too large is split again by the next bits up. The rule, with its
limits, is in the documentation of
[`Partitions`](../../../crates/relhash/src/table/chunks.rs).
For the hash `0x3C27`:

```
 bit           …  7 6 5 4   3 2   1 0
 hash 0x3C27   …  0 0 1 0   0 1   1 1
                            └┬┘   └┬┘
                             │     └─ first, 4 at shift 0: 0b11 = 3
                             └─ split again, 4 at shift 2: 0b01 = 1
```

The record goes to partition 3; if partition 3 is split later, it goes
to partition 1 of that split. The hash is in the record, so nothing is
computed again. The low bits are taken because the buckets take the
high ones: a partition read back gets an index of its own, and its
records then spread over all of the buckets. Had the partitions taken
the high bits, every record of a partition would share them, and all
of them would fall into a few buckets.

A split is not limited to two levels. Each split makes a **level**: its
partitions take the bits just above those of the level it splits, at a
larger shift. A level has at most 65536 partitions, `MAX_PARTITIONS`;
Tessera's nodes make 4 to 1024, so a level takes 2 to 10 bits. A
partition of any level splits again when it is still too large, as long
as bits of the hash are left, so the levels can go on to the length of
the hash. Each level divides a partition's records among its
partitions, so a few levels are enough even for a table far larger
than the memory. What no split helps is many rows of one key: they
share the whole hash, and every level puts them in one partition. [A
partition too large for
memory](../hash-table/design.md#a-partition-too-large-for-memory) in
the design of `hash-table` says what Tessera's nodes do then.

A partition is a list of chunks. Each call that works by partition
takes a `Partitions`: the shift of its level, and one chunk number for
each partition, the chunk that partition appends to now.

```
 partition   its chunks                       Partitions::chunks
     0       chunk 4 (on disk), chunk 9          9
     1       chunk 5                             5
     2       none yet                            1  (the empty chunk)
     3       chunk 7 (full), chunk 8             8
```

A row can be written only where its partition's chunk has room. A
partition that has no chunk yet, partition 2 above, names the **empty
chunk**: a chunk of 8 bytes that holds its used mark alone and has no
room for any record. To the call it is just a full chunk, so the call
needs no case of its own for a partition without a chunk. Nothing is
ever written to the empty chunk, so any number of partitions may name
the same one.

When a row's chunk has no room, the call does not stop. It leaves that
row in the batch's mask of pending rows, the rows it did not place, and
goes on with the next ones, so one full partition does not hold up the
others. The caller then gives each partition with a pending row a new
chunk and calls again for the pending rows.

Three calls work by partition.

- `ChunkWriter::append_partitioned_columns` appends the rows of a batch,
  each to its partition, with the payload taken from columns. It counts
  in an `Appended` the rows of each partition and the columns that held
  a NULL, so that the caller knows how much each partition holds. As
  every call of a chunk writer, it needs no index and links nothing.
- `ChunkWriter::split` copies the records of one chunk, from a byte on,
  each to its partition, and gives the reference and the hash of each
  copy. It stops at a partition whose chunk has no room and names it in
  `Split::full`. A join moves its table into partitions so, a chunk at a
  time, and a partition read back is split into a level below the same
  way.
- `TableMut::find_or_insert_partitioned` finds or makes the record of
  each row's keys as `find_or_insert` does, over one index for the
  chunks of every partition in memory, but writes a new record to the
  chunk of its row's partition. A grouping that spills finds its groups
  so.

## The Bloom filter of the keys

In many joins most rows of the probe side find no pair. Such a row
still costs a read of its bucket, and on a table far larger than the
caches that read waits for memory. A **Bloom filter** answers "surely
not here" from far less memory. It is an array of bits. Each key of the
table sets a few of them, chosen by its hash; a row whose bits are not
all set has no key in the table. A row whose bits are all set may still
have none, a false pass, and then probes the table as it would anyway.

A plain Bloom filter sets its bits anywhere in the array, so a check
reads several places, each a possible wait. The crate's filter is
*blocked*: the four bits of a key lie in one word of 64 bits, so a
check is one read and one comparison. The rule is in the
documentation of the module
[`bloom`](../../../crates/relhash/src/table/bloom.rs); here it is
on an example.

**The size.** A table of 1000 records wants 16 bits a record: 16 000
bits, or 250 words. The words are rounded up to a power of two, 256
words or 2 KiB, so the filter has about 16.4 bits a record. The power of
two lets the word be found by a shift: 256 is 2^8, so the number of a
word is 8 bits long.

**The word and the bits.** The hash of a key, here `0x3C27`, is
multiplied by the odd constant `0x9E3779B97F4A7C15`, and the low 64
bits of the product are kept. The top 8 bits of the product are the
number of the word. The low 24 bits are four numbers of six bits each,
and six bits name one of the 64 bits of a word.

```
 hash 0x3C27 × 0x9E3779B97F4A7C15 = 0x1AFB0517D96DD333

 bits of the product  63 … 56   55 … 24    23…18   17…12   11…6    5…0
                      00011010  not used   011011  011101  001100  110011
                      word 26              27      29      12      51

 filter     word 0   word 1   …   word 26   …   word 255
                                     ▲
            the key sets bits 12, 27, 29 and 51 of word 26
```

A row of the probe side is checked the same way: its hash gives a word
and four bits, and the row passes when all four are set in that word. A
row with the hash `0x3C27` reads word 26 and finds the bits this key
set. A row of another hash passes without a pair when other keys
happened to set all of its bits: a false pass. Two of the four numbers
may be equal, and a key then sets three bits.

**Why the product.** The bucket of a key is taken from the high bits of
the hash itself. Had the word been taken from those bits too, the keys
of neighbouring buckets would share a word and crowd it. The product
mixes every bit of the hash into its high bits, so the word does not
follow the bucket.

**How many pass falsely.** At exactly 16 bits a record, 484 of 100 000
absent keys passed a filter of 4096 keys, about one in 200. Rounding up
to a power of two adds bits, and fewer pass. The size and the measured
bound are in the documentation of
[`bloom::words_for`](../../../crates/relhash/src/table/bloom.rs).

A filter is filled in three ways, for three kinds of table. Whether a
filter is worth its cost is the caller's to decide: Tessera's join
decides by its own rows, as [The Bloom filter of the
keys](../hash-table/design.md#the-bloom-filter-of-the-keys) in the
design of `hash-table` tells.

- **A table of one process.** Once the appends are done, `Table::bloom`
  clears a filter and sets the bits of every record of the chunks,
  linked or not, and `bloom::probe` narrows the rows of each batch
  before the table's probe.
- **A shared table.** The filter is shared too, a `SharedFilter` behind
  a state word. Each participant decides whether it wants the filter,
  and the first whose `Table::try_build_bloom` takes it builds it alone;
  the others go on without it until it is ready. Nobody waits.

  ```
   none ──first participant's compare-and-swap──► building
                                                     │ fills the filter
                                                     ▼
   ready ◄──────────── a store with release ─────────┘
     │
     └─► a participant that reads ready, with acquire, checks its rows
         against the filter; before that, it probes the table alone
  ```

  The acquire read of ready makes every bit the builder set visible, as
  the read of a bucket makes a record visible. `bloom::probe_shared`
  refuses a filter that is not ready. A filter whose build fails stays
  building: it never reads ready, the probes go on without it, and
  nobody waits. Setting it back to none was refused: the next
  participant would take it and fail the same way.
- **A table that spills.** A filter of every row of the build side is
  filled as the rows come, before some of them go to disk: by
  `bloom::add` in one process, or by every participant at once by
  `bloom::add_atomic`, which sets the bits of a word by an atomic OR.
  Rows of the probe side that it rejects have no pair in any partition,
  and are dropped before they are written to disk. The caller orders
  every addition before the first check, by a barrier.
  `bloom::words_within` sizes such a filter within an eighth of the
  caller's memory: it lets more rows through when it is small, but
  leaves the memory to the rows.

The loom model checks the shared filter as it checks the buckets: two
participants race to build it and one does, and a participant that
reads ready sees every key pass. In negative tests, a state stored
relaxed lets a reader see a filter not yet filled, and an addition by a
plain read and write in place of the atomic OR loses bits.

The calls check the count of a filter's words and of a batch's rows.
The words of a filter in one process are a slice, aligned as its type
requires; the calls over a shared filter, or one filled together, take
a pointer and check that the words are aligned to 8, since they read
them as atomic words.

## The marks of a RIGHT or FULL join

A RIGHT join returns every row of its build side, the side the table
holds, and so does a FULL join: the rows that no row of the probe side
matched come out after the probe side, with NULL for its columns. The
join must therefore know which records found a pair. It keeps a **mark**
for each record, one bit, in words of its own beside the chunks: a run
of words for each chunk, bit `i` of word `w` for the chunk's record
`64 w + i`, `mark_words` words for a chunk. The layout is drawn in the
documentation of [`Marks`](../../../crates/relhash/src/table/marks.rs).
With records of 32 bytes, the reference 131353 names chunk 1, byte
2248; that is the chunk's record (2248 - 8) / 32 = 70, so its mark is
bit 6 of word 1 of chunk 1's run.

The words are the caller's, and the crate reaches them through
`Marks`, a trait the caller implements: `set` sets a bit of a word of
a chunk's run, and `word` reads a word.

```
 probe:   a pair passes the join's conditions
            └─► Table::mark: the bit of the pair's record is set
 then:    the probe side is done
            └─► Table::scan_unmarked: the records whose bits are
                clear, a batch at a time, from a cursor; a count of 0
                ends the walk
```

The marks are not kept in the records. The records are read by every
participant while they probe, and stay as they were written. The walk
reads the marks of 64 records in one word, and reads no record whose
bit is set; it reads the header of each record it gives, which every
walk over a chunk checks. The walk needs only the chunks, so `mark` and
`scan_unmarked` take the size of a record from the caller, and
`Table::mark` and `Table::scan_unmarked` take the table's.

In a shared table the marks lie in shared memory, and two participants
may mark records of one word at the same time. `Marks::set` takes
`&self` for this: the caller's marks change each word by an atomic OR,
so no mark is lost. The walk comes after every mark, which the caller
orders, by a barrier. The loom model marks records of one word from two
participants, and a negative test shows that a plain read and write
loses marks.

## Errors of two kinds

A call that refuses returns an `Error`: the rule it found broken, who
broke it, and up to two numbers that show how. Who broke it is the
error's kind. A **wrong call** is a value the caller passed that breaks
a rule: arrays of different lengths, a chunk that does not exist, keys
that are not the table's. A **damaged table** is a byte read from the
table's blocks that cannot be: a field of the header out of its range, a
reference outside its chunk, a chain that loops. What the call read
decides the kind, not the check. One function checks a reference whether
the caller passed it or the call read it from a bucket, and the call,
which knows where it came from, chooses the kind. A reference that the
caller passed but that lands inside a record reads as damage, since the
crate cannot tell the two apart. The documentation of
[`Kind`](../../../crates/relhash/src/error.rs) lists what each kind
covers, [Errors of two kinds](spec.md#requirement-errors-of-two-kinds)
says what is promised, and each `Rule` says what it refuses. Tests
compare the kind and the rule, never the words of the message, so a test
sees which check refused.

The error is one integer of 8 bytes, drawn bit by bit in the
documentation of [`Error`](../../../crates/relhash/src/error.rs). A
result of a count or a reference is then 16 bytes, which a call returns
in two registers. Rust returns a struct through memory, even one of 8
bytes. With the rule in a byte of its own beside seven bytes of numbers,
the walk down a key's records spent a load a row on the address of the
result, so that form was refused. A compile-time assertion keeps the
error at 8 bytes. It allocates nothing and holds no text: it writes its
message only when it is printed, and Tessera passes that message on to
PostgreSQL. The rules are listed once, in a macro that writes both the
enum and the table that turns a number back into its rule, so no
`unsafe` is needed.

Two more forms were refused. A text, as `anyhow` gives, allocates, and
a test could only look for words in it. And an error type of the source
of keys would make every call with keys generic in its error. A source
that cannot give a word answers `None`; the call then names the rule
with the word of rows and the key, and a source that must say why keeps
its own error for its caller to read.

What a call leaves after an error depends on where it stopped.

- A wrong size, count or block that the caller passed is found before
  the call changes anything.
- A call that fails part way, at a source of keys that cannot give a
  word, leaves the table whole. Every exit stores the used marks of the
  chunks it wrote. The rows placed before the error have left the mask
  of pending rows with their references, those that made a record are
  in the mask of inserted rows, and a link counts what it linked and
  moves its start past it. The caller can go on from the masks, as [A
  batch that fails part way](spec.md#requirement-a-batch-that-fails-part-way)
  says.
- A damaged table is to be dropped or reset. Later calls over it stay
  bounded and sound, as over any bytes, but what they answer is not
  promised. `regrow`, `clear_key`, `bloom` and `try_build_bloom` check
  every chunk before their first change, so that a block that cannot
  be a chunk fails them before they change anything. The used marks are
  not stored on the exits of damage: the chains are damaged whatever
  the marks say, and a stop there would cost the hot loops.

## The promise over any bytes

A call reads the index and the chunks, and what it reads may be
anything: the caller may hand over bytes that are not a table, or a
table that a bug damaged. The crate promises an error for such bytes,
never a crash and never a call that does not end, as [Any bytes, a
bounded call](spec.md#requirement-any-bytes-a-bounded-call) says. The
README states the promise whole, with what the caller promises in turn
when it makes a handle over its memory: blocks of the lengths given,
aligned to 8 and initialized; no two of them overlapping; and no other
process or thread writing the table while the call runs.

The `unsafe` calls that make the handles say more of what the caller
promises. A chunk has one writer across every table and chunk writer
over it, and a table that one writer has to itself has its chunks to
itself too, whatever other index names them. The calls that read the
used mark of every chunk, `bloom`, `try_build_bloom` and
`Table::scan_unmarked`, run only once every writer's appends are done
and ordered before them, as by a barrier. A used mark read and written
atomically was refused: it makes the read defined but not the answer,
and the barrier that makes the answer right orders plain reads already.

A shared table that is being built breaks the last condition on
purpose. A call there checks a reference against the length of its
chunk, not against its used mark, so as never to race with the chunk's
writer. A damaged reference can then point at bytes another process is
writing: a race that no check can turn into an error. For such a table
the crate promises only what "Several processes at once" says, over a
table that is not damaged, which loom checks. Promising more would
need an atomic read of every byte of a record, a cost on every probe.

Seven means show the promise, each covering what the others cannot:
the trust points below, each with its check; Kani's proofs of the
checks, for every value; the matrix of damage, which runs every call
over every damage; fuzzing of the same shapes with the bytes and
arguments the fuzzer chooses; every mutant of the crate caught by its
own tests; Miri over every test; and tests of the rest of the promise,
under an allocator that counts. "Tests" says where each lives.

## Where the table trusts its bytes

A **trust point** is a place where a call takes a value from the
table's blocks: a value it did not compute itself, which it must check
before the value decides anything.

What a value decides says what a wrong one would do:

- an **address**, where the call reads or writes next: a wrong one
  would reach memory outside the blocks;
- the **length of a loop**, how many steps a walk takes: a wrong one
  would let the call go on and on;
- only **data**, such as a hash, a key or a payload: a wrong one gives
  a wrong answer, which no check can see, but no crash.

Every read of the index or a chunk goes through the region, the one
layer of memory, so the reads are few and can be listed. Each call of a
read of the region carries a comment `// trust:` with the name of its
point below. `.github/scripts/check-trust.sh` fails when a read has no
mark, when a mark names no point of this list, and when a point is
marked nowhere. It also knows every method of the region as a read or
not, so that a new method has to be sorted. A borrow of a chunk's bytes
for writing is not a read: the table only writes through it, and the
payloads it hands to its caller are bytes the table never reads.

A proof under a point is one of `crates/relhash/src/table/proofs.rs`,
which `cargo kani -p relhash` runs. Kani, a model checker, runs the
crate's own check over a region whose every read gives any value, and
shows for each value that the check keeps the reads it guards inside
the blocks and that nothing panics. The walks are proved over small
chunks, for any record size: down a chain and over a chunk's records
in chunks of up to 80 bytes, three of the smallest records, and over
the marks in a chunk of up to two words of them. A longer walk repeats
the same checks, and a solver of bits, which Kani is, proves the
arithmetic of longer ones too slowly.

The rows of the matrix under a point are damages of
`crates/relhash/src/table/tests/damage.rs`. Its test
`every_damage_against_every_call` builds valid tables of four shapes,
with repeated and NULL keys over several chunks, regrown or split,
damages each in one place, at and around the limits of the field, and
runs every call of the crate over it: each call alone, then all of
them in turn, twice, so that calls follow the ones that failed. A call
must end with a result or the error of the point, and the guard words
around every block must stay as they were.

- **`header`**: the fields of the header, read when a handle attaches
  to the index, and when the index is moved to a larger one.
  - Decides addresses and the length of loops: where the buckets start
    and how many there are, the size of a record and of its parts.
  - Check: `Header::validate` compares every field with the format and
    with the length of the index the caller gave, as the documentation
    of [`HEADER_SIZE`](../../../crates/relhash/src/table/header.rs)
    lists them.
  - Error: damaged, with the rule of the first wrong field, from
    `Magic` to `Reserved`.
  - Proof: `a_header_that_passes_puts_every_bucket_inside_the_index`.
  - Matrix: the rows from `magic` to `last reserved word`.
- **`record-count`**: the count of records in the header.
  - Decides the length of a loop: a walk down a chain takes at most as
    many steps as there are records. A table's statistics hand it to
    the caller as data, and the additions to it use none of the values
    they return.
  - Check: `Access::refresh` cuts it to the places that references can
    name in the chunks the caller gave, 2^17 a chunk. A walk past the
    count, or a long walk that comes back to a record it saved, has
    gone round a loop.
  - Error: damaged, `Chain`.
  - Proof: `the_record_count_is_bounded_by_the_chunks`, and
    `a_walk_down_a_chain_ends_within_the_record_count` for the walk.
  - Matrix: `no record`, `one record`, `a record less`, `a record more`
    and `most records`.
- **`bucket-head`**: the reference a bucket holds, the first record of
  its chain.
  - Decides an address.
  - Check: `Access::place`, before the reference is followed: its chunk
    exists, and the record starts after the chunk's first word, its
    used mark, and ends within the chunk. A head read to be put after
    a new record is not followed then: it becomes the record's next
    field, which a walk checks when it follows it.
  - Error: damaged, `Outside`.
  - Proof: `a_reference_that_passes_names_a_record_inside_its_chunk`.
  - Matrix: the rows from `bucket emptied` to `bucket of most`.
- **`used-mark`**: the first word of a chunk, the bytes its records
  take.
  - Decides an address, where the chunk's writer appends, and the
    length of a loop, how many records a link, a scan or a split
    visits.
  - Check: `used_mark`: at least the mark's own 8 bytes, at most the
    chunk's length, and the end of a whole record. A link that stops on
    an error reads the mark again, as the chunk's one linker, after it
    checked it.
  - Error: damaged, `UsedMark`, or `BlockUsedMark` for a block given
    alone.
  - Proof: `a_used_mark_that_passes_ends_a_record`, and for the walks
    `a_walk_over_a_chunk_stays_inside_it` and
    `a_walk_over_marks_reads_words_within_them`.
  - Matrix: the rows from `used mark zero` to `used mark at the
    chunk's end`.
- **`record`**: the bytes of a record, cut into its fields.
  - Decides an address by its next field, which is checked as a
    bucket's head is when a walk follows it. Its size field is checked.
    Its hash, NULL bits, keys and payload are data.
  - Check: the size the record claims is the table's record size:
    `Access::open_at` for a record a reference names; `record_at`, the
    one check of a chunk's places, for every walk over a chunk, the
    walk over unmarked records too; and `check_record_at` for the link
    of the records just appended, in its own loop for speed, which
    walks only places it saw written.
  - Error: damaged, `Misplaced` for a record a reference names,
    `NoRecord` for one a walk over a chunk meets.
  - Proof: `a_walk_down_a_chain_ends_within_the_record_count`, whose
    walk opens every record it meets.
  - Matrix: `hash`, `NULL bits`, `key`, `payload`, and the rows
    `record size field` above, zero and most.
- **`record-copy`**: the bytes of a record that a split or a merge
  copies whole into another chunk.
  - Data only: the copy's next field is written anew before the copy
    is linked.
  - Check: the record is checked as `record` checks it before the copy.
  - Error: as `record`.
  - Matrix: the rows `record size field`, which a split of the damaged
    chunk meets.
- **`next`**: the next field of a record that the one writer of a table
  reads to put a new record of the same key after it.
  - Decides an address, which becomes the new record's next field and
    is not followed then.
  - Check: as `bucket-head`, when a walk follows it.
  - Error: damaged, `Outside`.
  - Proof: as `bucket-head`.
  - Matrix: the rows from `next to itself` to `next of most`.

## Unsafe code

Unsafe code is code whose soundness the compiler cannot check: a read
through a raw pointer, an index without a bounds check, a promise of a
caller. A mistake in it corrupts memory instead of giving an error. The
crate keeps it in the modules below, each with what its unsafe code
does and what it rests on. Every other module starts with
`#![forbid(unsafe_code)]`, which nothing inside the module can lift,
and the crate's root denies unsafe code. `.github/scripts/check-trust.sh`
fails when a module off this list does not forbid unsafe code or allows
it, and when a module of the list holds none. The paths are under
`crates/relhash/src/`.

- **`table/region.rs`**: the region, the one layer of memory: the
  addresses in the index and the chunks, made from the caller's bases,
  and their atomic words. It rests on the caller's promise that the
  blocks are valid, on its own bounds checks in the index, and in the
  chunks on the checks of the trust points.
- **`table/record.rs`**: a record cut into its fields, the head of a
  bucket read without a bounds check, and the unchecked forms of the
  calls on a record that the loops use. They rest on `Access::place`,
  which puts a record inside its chunk, and on `Header::validate`,
  which puts every bucket inside the index. Proofs:
  `a_header_that_passes_puts_every_bucket_inside_the_index`,
  `a_reference_that_passes_names_a_record_inside_its_chunk` and
  `a_walk_down_a_chain_ends_within_the_record_count`.
- **`table/batch.rs`**, **`table/writer.rs`** and **`table/lanes.rs`**:
  the unchecked calls on a record at places that `Access::place` or
  `Access::room` accepted, and a row's payload taken without a bounds
  check from an array whose length was checked. `lanes.rs` also reads
  the lanes of a word that its mask filled, and gives the prefetch hint
  of AArch64, an instruction that reads nothing and never faults.
  Proofs: `a_used_mark_that_passes_ends_a_record` and
  `a_walk_over_a_chunk_stays_inside_it` for `Access::room` and the
  walks over a chunk; `a_partition_lies_within_the_partitions` and
  `the_partitions_that_pass_name_chunks_of_the_table` for the chunk of
  a partition.
- **`table/keys.rs`**: the key slots of a word of rows, indexed without
  a bounds check below the key count that the header's check fixed.
  Proof: `a_header_that_passes_puts_every_bucket_inside_the_index`.
- **`table/chunks.rs`** and **`table/mod.rs`**: the calls of the API
  that take the caller's memory or the caller's promise, such as
  `Chunks::new`, `Chunks::init`, `ChunkWriter::new`, `Table::attach`,
  `Table::link` and `TableMut::regrow`. Their documentation says, under
  "Safety", what the caller promises.
- **`table/bloom.rs`**: the words of a shared filter, the caller's
  memory, taken as atomic words. Proofs:
  `a_word_of_the_filter_lies_within_it` and
  `the_words_of_a_filter_within_a_limit_are_a_filter`.
- **`table/marks.rs`**: the used mark of a chunk, read through the
  region for the walk over the records without a mark. Proofs:
  `a_mark_sets_the_bit_of_its_record` and
  `a_walk_over_marks_reads_words_within_them`.
- **`table/owned.rs`**: the blocks a table owns: their allocation and
  release, the handles over them, and that the table may move to
  another thread.
- **`table/tests.rs`**, **`table/tests/`** and **`table/loom/`**: the
  tests, which call the unsafe API as a caller does and count
  allocations, and the memory of the loom models.
- **`table/proofs.rs`**: the proofs by Kani, built only for Kani: the
  model region, whose methods assert their safety sections, and a
  chunk of the proof's own memory given to `Chunks::new`.

## The forecast, probed

The contract of `relhash`, [contract.md](contract.md), names the changes
the crate is likely to see and what each would touch. A probe makes such
a change on a branch that is never merged and counts the parts and the
interfaces it touched.

- **A prefetch hint for x86-64.** One module of the crate, the vertical
  probe's; no interface; no code of Tessera. As the forecast says.
- **Keys of more kinds**, a text of at most 16 bytes kept whole. No
  module of the crate, which keeps keys as slots of 64 bits: a key of
  two slots is two keys to it. One interface of Tessera, the kinds of
  the C API's keys, and the C API's source of keys. The nodes read a
  key's kind in 47 places of 12 files and 5 headers, among them both
  planners and the hashing, filters, pruning and spill of the
  aggregation and the join, which a new kind would reach: past the
  forecast, which named no node. The nodes now take a key's kind from
  one header, `nodes/key_kind.h`, which a new kind reaches alone.
- **Removing records**, a record taken out of its chain and its place
  freed. Six modules of the crate: the record, the calls of one writer
  and the API, where the removal lies, and the walks of a batch's
  split, of the filter's fill and of the marks, which each check the
  places of a chunk themselves. Three interfaces: the crate's API, the
  format of a record, whose size field gains a flag, and the C API. No
  node, as none removes yet: past the forecast, which named the calls of
  one writer and the chunks alone. Every walk over a chunk now takes one
  check of its places, `record_at`, which a free place would reach
  alone; the link of the records just appended keeps its own, and meets
  no free place, since a removal frees a linked record.

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
- **A call for one key, and typed keys and values.** For one key at a
  time a general hash map, such as hashbrown, is faster and smaller. A
  layer of typed keys and values may come later, over the calls of
  batches.
- **Removing records.** Records are only appended. A removal by one
  writer is possible later, and "The forecast, probed" counts what it
  would touch; a removal by several processes at once is a hard problem
  of freeing memory.
- **Checksums of records.** Damaged bytes that still look like a table
  give a wrong answer without an error. Only a checksum read with every
  record would catch them, at a cost on every probe.
- **A defense against chosen collisions.** The caller gives the hash.
  The README says to hash keys from outside with a secret seed.
- **A full formal proof**, with Verus or Creusot. The code would have to
  be written again in their subset. The risk lies in the unsafe code
  and in the protocol of several processes, which Kani and loom check.
- **`no_std`.** A call allocates nothing, so the crate could do without
  Rust's standard library, but nobody needs that now. The feature
  `alloc` only leaves the owning table out.

## Files

- `crates/relhash/README.md`: the crate's page, with an example over
  the owning table and the promise over any bytes
- `crates/relhash/Cargo.toml`: the crate alone, and its features
- `crates/relhash/src/lib.rs`: the crate's root, its parts in a picture
  and its examples, and its refusal of a target that is not 64-bit
- `crates/relhash/src/error.rs`: the error, its kinds and its rules
- `crates/relhash/src/row_mask.rs`: the masks of rows
- `crates/relhash/src/bits.rs`: the ones of a word, the rows a word of
  a mask selects
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
  writer, the lookup by partition, the cursors by reference, the
  pointers to payloads and the merge by a rule
- `crates/relhash/src/table/keys.rs`: the keys of a batch, a
  word of rows at a time
- `crates/relhash/src/table/owned.rs`: the table that owns its
  memory
- `crates/relhash/src/table/bloom.rs`: the Bloom filter, alone,
  shared and filled together
- `crates/relhash/src/table/marks.rs`: the marks of a RIGHT or
  FULL join and their walk
- `crates/relhash/src/table/loom/`: the memory of the loom model,
  the model table the models of the phases and the rounds run over,
  and the models of the table, the filter and the marks under several
  participants
- `crates/relhash/src/table/testing.rs`: the keys of the table's own
  tests and loom models
- `crates/relhash/src/table/proofs.rs`: the proofs by Kani of the
  checks of the trust points, the partitions, the filter and the marks
- `crates/relhash/src/table/tests/`: the crate's tests by topic, the
  matrices of damage and of wrong arguments, and their shared world
- `crates/relhash/fuzz/`: the fuzz targets, a crate of their own outside
  the workspace, with their builder of tables over the public API

## Tests

- The unit tests in `crates/relhash/src/table/tests/`: the format and
  every call, with damaged headers, chunks, references and chains, over
  keys of their own; they also run under Miri, at smaller sizes.
  `partial.rs` makes sources and chunks that fail part way, and checks
  that the rows placed stay placed and the masks say so; `owned.rs`
  runs random sequences of the owning table's calls against a model of
  the records of each key.
- The examples of the documentation that must not compile: two
  payloads open at once, a handle sent to another thread, and an
  `unsafe` call made without `unsafe`.
- The job "relhash alone" of `.github/workflows/ci.yml`: the crate's
  tests as the only package, its documentation with every warning an
  error, a dry run of publishing, a 32-bit target refused, and its
  public API against its snapshot,
  `openspec/specs/relhash/public-api.txt`. `make rust-check` also runs its tests
  without the feature `alloc`.
- Every test of the crate and every example of its documentation run
  under Miri in CI, with 16 cases of each property: an access outside a
  block or a borrow that Rust's rules forbid fails them.
- Every mutant of the crate, run in CI on request
  (`.github/workflows/mutants.yml`, package `relhash`) and judged by
  the crate's own tests alone, is caught, or is listed as equivalent
  with its reason in `.cargo/mutants.toml`.
- The unit tests in `crates/relhash/src/table/tests.rs`: blocks that
  are not chunks or an index, partitions, merges, and a shared filter
  that threads race to build.
- The matrix of damage in `crates/relhash/src/table/tests/damage.rs`:
  every damage of its list against every call, alone and after
  failed calls, with guard words around every block; a few of its cases
  also run under Miri. Its world, `tests/world.rs`, runs each call of
  the crate under an allocator that counts and fails the test on any
  memory taken, and the same sequence over two copies of the bytes
  must give the same blocks and answers.
- The matrix of wrong arguments in
  `crates/relhash/src/table/tests/arguments.rs`: each rule of a wrong
  call broken by one argument; the call names its rule, takes no
  memory, and leaves the blocks as they were when it checks the
  argument before its first row. A test there finds a case for every
  rule of the crate.
- The unit tests in `crates/relhash/src/table/marks.rs`: the
  words of the marks, and the records and cursors a walk refuses.
- `crates/relhash/src/table/loom/`, run by `make rust-loom`: links into
  one bucket by several participants, a probe beside a link, a shared
  filter built and read, and a filter and marks set by several
  participants, with the negative tests of the orders.
- The proofs in `crates/relhash/src/table/proofs.rs`, run by
  `cargo kani -p relhash`: the checks of the header, a reference, a
  used mark, the record count, a partition, the filter and the marks
  keep every read inside its block and cannot panic, for every value.
- The fuzz targets in `crates/relhash/fuzz/`, run by cargo-fuzz under
  AddressSanitizer: `damaged` builds a table from the fuzzer's bytes,
  damages it where they choose and runs the calls they choose;
  `arguments` gives the calls of a valid table any references, chunks,
  places, partitions and steps. CI runs each for a minute
  (`.github/workflows/fuzz.yml`), and for longer when started by hand.
- `crates/tessera-capi/tests/table.rs` and `test/sql/table.sql`: the
  calls through Tessera's C API.
- The benchmarks `table_int32` and `table_large` of `tessera-bench`
  count the instructions and cycles of append, probe and find-or-insert;
  `table_large` also those of the Bloom filter's check.

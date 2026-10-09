# spill-format: how it is built

Sometimes a hash join, a grouping or a sort has more rows than fit in
its memory. Then Tessera writes some of the rows to temporary files and
reads them back later. This is called spilling.

This page explains what Tessera writes, how the files are arranged, and
why. The exact bytes, limits and errors are in [spec.md](spec.md). When
a node decides to spill, and how it divides its rows, is described in
[docs/spill.md](../../../docs/spill.md) for the join and the grouping,
and in [docs/nodes.md](../../../docs/nodes.md), "External sort", for
the sort.

## Background: rows, batches, records

A **row** is one row of a table or of a result inside a query.

Tessera does not pass rows between nodes one at a time. A node gets a
**batch**: up to 64 rows at once. A batch stores its rows *as columns*.
It has one array for each column, and row number `i` is element `i` of
every array.

A hash table stores rows in the other way, *as records*. A record holds
all the values of one row next to each other.

```
 three rows with two columns, a and b

 as records: row after row          as columns: column after column
 ┌────┬────┬────┬────┬────┬────┐    ┌────┬────┬────┬────┬────┬────┐
 │ a0 │ b0 │ a1 │ b1 │ a2 │ b2 │    │ a0 │ a1 │ a2 │ b0 │ b1 │ b2 │
 └────┴────┴────┴────┴────┴────┘    └────┴────┴────┴────┴────┴────┘
   row 0     row 1     row 2          column a       column b
```

Records are good for a hash table: to compare a key or to follow a link
to the next record, the node needs the values of one row together.
Columns are good for a batch: a loop over one array is fast.

A value that does not fit in 8 bytes, such as a text, lies in neither a
record nor a column. It lies in separate memory, and the record or the
column holds a reference to it.

A node keeps all of this in large pieces of memory called **chunks**.
There are three kinds of chunk: a chunk of records, a chunk of long
values, and a chunk of columns.

## The problem

PostgreSQL gives each node of a query a limit of memory: `work_mem`.
For a hash table the limit is `hash_mem`, which is `work_mem` multiplied
by `hash_mem_multiplier`. A join, a grouping or a sort must finish even
when its rows need more memory than that.

The usual solution is this. The node divides the rows into partitions
by their hash. It keeps in memory the partitions that fit and writes the
others to disk. Later it reads one partition back, finishes the work on
it, and takes the next one. A sort does a similar thing: it writes
sorted parts, called runs, and then merges them.

This solution leaves questions. The format answers them.

1. A query spills when it is already large. Spilling must not make it
   much slower. How can writing and reading be cheap?
2. A node holds its rows in chunks of three kinds. What exactly should
   go to disk?
3. In a parallel query one process writes a file and another process
   reads it. A memory address of the writer means nothing to the reader.
   What can the bytes on disk point to instead?
4. A node uses the bytes it reads back as its own structures in memory.
   It does not examine every row. How can it be sure the bytes are what
   it expects?
5. A node spills because memory is short. How much memory does reading
   need? Is that known before the read starts?
6. PostgreSQL has rules for temporary files: a limit on their total
   size, special tablespaces, and removal after an error. How does
   Tessera follow them?

## Goals and what they cost

All numbers on this page were measured on a developer's laptop with an
Apple M5 Pro processor. The target platform is Linux on x86-64; the
numbers have not been measured there yet.

1. **A chunk comes back ready to use.**
   - *What Tessera does.* A node writes a whole chunk to disk as one
     piece. To read, it reads the whole chunk back into memory with one
     read. The chunk is then ready: its bytes are already in the form
     the node uses. For a chunk of records, the hash table only adds the
     records to its index, with one call for the chunk. Nothing is done
     for a single row: no memory is allocated for it, and it is not
     decoded.
   - *For comparison.* PostgreSQL's own hash join writes one row at a
     time. When it reads, it allocates memory for each row and copies
     the row there.
   - *The cost.* The bytes on disk have the same layout as the bytes in
     memory. So only a table with the same layout can use them. This is
     why every block carries a version and a fingerprint of the layout.
     Also, one row cannot be read without its whole chunk.
2. **Smaller files for very little processor time.** Before a chunk is
   written, it is packed by lanes. A lane is the same field of every
   record. Each lane is stored with only as many bytes as its values
   need in this chunk. Packing runs at 7 to 10 GB/s and unpacking at 12
   to 14 GB/s on one core. The files of a join and of a grouping become
   2.7 to 3.5 times smaller, and the queries up to 13 % faster.
   - *The cost.* A general compressor would make the files smaller
     still. Long values are not packed at all.
   - *Why not a general compressor.* lz4 runs at 1.3 GB/s. It would cost
     more time than the smaller files save.
3. **Rows that only wait are not turned into records.** Some rows go to
   disk only to wait. Examples: the outer rows of a join whose inner
   partition is still on disk; the rows of a grouping that belong to a
   partition on disk; the rows of a sorted run. Nobody searches among
   these rows. They are written once and read back once.
   - *What Tessera does.* A batch already holds these rows as columns.
     So Tessera writes them as columns and reads them back as columns.
     There is no conversion.
   - *What it did before.* It stored these rows as records. Each row was
     then converted four times: from columns to a record; from records
     to lanes, for packing; back to records, on reading; and back to
     columns, for the batch. A join that spills with a `work_mem` of
     1 MB took 67 to 83 ms and wrote 29.5 MB. With columns it takes 45
     to 55 ms and writes 11.6 MB.
   - *The cost.* There are two forms of chunk for rows, records and
     columns, and each has its own packed form.
4. **Few files and large writes.** A node writes one file for all its
   partitions. It writes through a buffer of up to 256 kB. The usual way
   in PostgreSQL is `BufFile`: one file for each partition, written in
   pieces of 8 kB. One file with a larger buffer made a spilling join 6
   to 9 % faster.
   - *The cost.* After a partition has been read, its bytes stay on disk
     until the whole file is deleted.
5. **A block that fails a check is an error.** A reader checks every
   header and every packed body before it uses them. If a check fails,
   the query stops with the error "damaged data". The checks find a
   block of another table, a block in a wrong place, and a body that
   does not fit its header.
   - *What is not checked.* There is no checksum of the body, so a
     changed byte inside a value is not noticed. The lists of blocks of
     a shared file are checked only to lie inside the file. A
     temporary file is read by the same query that wrote it, on the
     same machine.
     PostgreSQL does not checksum its own temporary files either.
6. **The memory for a read is known before the read starts.** The header
   of a block holds two sizes: the size of the body on disk and the size
   of the chunk in memory. For each partition the node also knows the
   size of its largest block, and the reader's buffer has exactly that
   size.
   - *What this does not cover.* The lists of blocks take memory too:
     16 bytes for each block, and they grow with the amount spilled. A
     reader of another participant's file keeps its own copy of a list.
     `tess_spill_memory` does not count the lists, and a node does not
     count them against its memory limit.
7. **Any process of the query can read any file.** Nothing on disk is a
   memory address. A reference to a long value is a position in a chunk
   of values (see "Values"). In a parallel query, the list of the blocks
   of a file is written into the file itself, because a reader cannot
   see the writer's memory.
8. **All calls to PostgreSQL's file functions are in one source file**,
   `runtime/spill.c`. A database with a different manager of work files,
   such as Greengage, replaces only this file.

## The whole in one picture

```
 a node (join, grouping, sort)         the node's temporary file
 ┌───────────────────────────┐         byte 0
 │ chunks in memory          │         ┌────────┬──────────────────┐
 │ ┌─────────┐ ┌────────┐    │  write  │ header │ body: a chunk    │ A
 │ │ records │ │ values │    │ ──────► ├────────┼──────────────────┤
 │ └─────────┘ └────────┘    │         │ header │ body             │ B
 │ ┌─────────┐               │  read   ├────────┼──────────────────┤
 │ │ columns │               │ ◄────── │ header │ body             │ C
 │ └─────────┘               │         ├────────┴──────────────────┤
 │                           │         │ …                         │
 │ lists of blocks           │         └───────────────────────────┘
 │  partition 0: B           │
 │  partition 1: A, C        │   one entry of a list: where the block
 └───────────────────────────┘   starts in the file, and its size
```

A node writes each chunk to the file as a **block**: a header of 48
bytes and then the body. Blocks of different partitions follow one
another in the order the node writes them.

The file has no header of its own. It starts with the first block at
byte 0, and there is nothing between the blocks.

The file alone does not say which blocks belong to a partition. The
**lists of blocks** say that. For each partition the node keeps a list:
where each block of the partition starts in the file, and how many
bytes it takes. To read a partition, a reader goes through its list.

## The words used below

- A **chunk** is a large piece of a node's memory. A node writes a chunk
  to disk whole. There are three kinds: records, long values, columns.
- A **block** is a chunk on disk: a header of 48 bytes, then the body.
  The body is the chunk itself or its packed form.
- A **partition** is a group of rows that a node writes together and
  reads back together. In a join and in a grouping it is the rows whose
  hashes have the same value in some of their bits. In a sort it is one
  sorted run. A set keeps a list of blocks for each partition.
- A **level** says how many times a join or a grouping divided its
  rows. Sometimes a partition is read back and still does not fit in
  memory. Then it is divided again, by other bits of the hash. The first
  division is level 0, the next is level 1, and so on. A sort always
  uses level 0.
- A **set** (`TessSpill` in the code) is one group of blocks with one
  file and one group of lists. A join and a grouping make a set for one
  level and for one kind of rows. For example, at each level a join has
  two sets: one for its inner rows and one for its outer rows. A sort
  makes a set for a group of runs.
- A **participant** is one process of a parallel query: the leader or a
  worker.
- A **fingerprint** is a number that describes the layout of the data.
  Two tables with the same layout have the same fingerprint.
- A **lane** is one field taken from every record of a chunk and put
  together into one array. For example, the `hash` fields of all the
  records form one lane, and their `len` fields form another. In a chunk
  of columns a lane is one column: one word from every row. Tessera
  writes a chunk to disk lane by lane, because the values inside one
  lane are alike and can be written with few bytes. The rules of this
  encoding are written once, in the spec.

## A block

A block is a header of 48 bytes and then the body. The spec draws the
header, with the place and the size of every field
([spec.md](spec.md), "Block header layout"). This section says what
each field is for.

**magic and version.** They say that these bytes are a block of this
format, and which version of the format.

**kind.** It says which of the three kinds of chunk the body is.

**number.** The writer gives every block a number. Only a join uses it
when it reads. A join numbers its chunks of long values 0, 1, 2, and so
on, and its references contain these numbers. So when a join reads a
block of values, it takes the number from the header and stores the
address of the chunk in an array, at the index equal to this number.
After that every reference to this chunk works again. A sort and the
waiting rows of a grouping do not need the number (see "Values").

**partition and level.** The reader knows which partition and which
level it is reading. It finds a block through the list of blocks of
that partition. After it has read the block, it compares the partition
and the level in the header with its own. If they differ, the list or
the file is damaged, and the query stops with an error.

**fingerprint.** It protects against reading the block into a table
with a different layout. A set has one fingerprint. The writer puts it
into every header, and the reader compares it with its own.

- For a hash table, the fingerprint is a 64-bit FNV-1a hash of five
  things: the version of the table format, the number of keys, the kind
  of each key, the size of a record, and the size of the payload of a
  record.
- A join has a table layout for each of its two sides. It uses the
  fingerprint of the side's layout, also for the outer rows that it
  writes as columns.
- A grouping has two kinds of set. The set with its records uses the
  fingerprint of its hash table. The set with its waiting rows has no
  table, so it uses a simple number: the number of 8-byte words in a
  row.
- A sort has no table either. It combines the number of its columns
  with the number of extra words it stores for each row.

A number of the last two kinds says only how wide a row is. It does not
say what the types of the columns are.

A matching fingerprint is the first check. When a hash table takes a
chunk of records in, it checks the records again, as it checks any
chunk.

**body length and packed length.** These are the two sizes from goal 6.
The body length is the size of the chunk that the reader gets. For
records and values this is the chunk as it was written. For columns it
is a chunk that holds only the rows, without the free space of the
chunk that was written. The packed length is the size of the body on
disk when it is packed, and 0 when it is not packed.

The numbers in the header use the byte order of the machine. There is
no checksum. Goal 5 explains why.

## The three kinds of body

### Records

A hash table keeps its records in chunks. A chunk starts with 8 bytes
that say how many bytes of the chunk are used. Then the records follow.
All records of a table have the same length.

A record starts with four fields of 4 bytes each: its hash, the link
`next`, the NULL bits of its keys, and its length `len`. Then come the
keys, 8 bytes each, and then the payload.
[hash-table](../hash-table/design.md) draws a chunk and a record.

A record is made for fast search, not for storage. On disk much of it
is waste:

- `next` links a record to the next record with the same hash bucket.
  On disk this link means nothing.
- `len` is the same in every record.
- The NULL bits are 0 in most records.
- An `int4` key or value takes 8 bytes, but needs only 4.

For example, take a join on one `int4` key with one `int4` column. A
record takes 40 bytes, and only 12 of them carry information.

So Tessera packs a chunk of records before it writes the chunk. It cuts
every record into pieces of 4 bytes. The first piece of every record
forms lane 0, the second piece forms lane 1, and so on. Then it stores
each lane with as few bytes as the values of this lane need.

```
             hash   next   NULLs  len    key 0        payload
 record 0  │ a91f │ 0007 │ 0000 │ 0005 │ 0017 0000 │ 002a 0000 … │
 record 1  │ 03c2 │ 0000 │ 0000 │ 0005 │ 0018 0000 │ 0007 0000 … │
 record 2  │ 77b0 │ 0009 │ 0000 │ 0005 │ 0019 0000 │ 0063 0000 … │
               │      │      │      │      │    │      │    │
               ▼      ▼      ▼      ▼      ▼    ▼      ▼    ▼
 on disk     4      not    all    all    1     all    1     all
             bytes  stored zero:  equal: byte  zero:  byte  zero:
             each          no     one    each  no     each  no
                           bytes  value        bytes        bytes
```

The values in the picture are invented, and each piece of 4 bytes is
shown shorter than it is. The types of the columns do not matter:
packing looks only at the values.

The spec defines the encoding: the layout of the packed body and the
code that says how each lane is stored ([spec.md](spec.md), "A packed
chunk of records").

For example, 1000 records of the join above take 40 000 bytes in memory.
The hash, the key and the value each need 4 bytes for a record, 12 000
bytes together. The `len` lane takes 4 bytes for the whole chunk, and
the other lanes take nothing. So the packed body is about 12 000 bytes.

When a reader unpacks the block, it gets the chunk exactly as it was,
with one difference: every `next` is 0. The hash table then links the
records again. If packing does not make a chunk shorter, the chunk is
written without packing.

### Values

A text, or any other value that does not fit in 8 bytes, does not lie
inside a record or a column. PostgreSQL calls such values by-reference
values. Tessera copies them into chunks of values. The record or the
column holds a reference to the value in one word of 8 bytes. The spec
defines the reference ([spec.md](spec.md), "A reference to a
by-reference value"): the number of the chunk of values, plus one, and
the byte of the value in that chunk.

A reference is never a memory address. So it stays correct after the
chunk has been on disk, and it is correct in another process. One is
added to the number of the chunk so that a real reference is never 0:
the word 0 always means "no value".

Every node uses this one form. The nodes differ only in how many chunks
of values they have.

**One chunk of values.** A sort, the waiting rows of a grouping and the
gather node write two things together: a chunk of values, and right
after it the chunk of columns that refers to it. This holds even when
the chunk of values is empty. The chunk of values has the number 0, so
the upper half of every reference is 1.

```
 block of values, number 0        block of columns, written next
 ┌───────┬───────┬─────────┐      ┌──────────────┬──────────────┬───┐
 │ Kazan │ Omsk  │ …       │ ◄─── │ ref (0, 0)   │ ref (0, 8)   │ … │
 └───────┴───────┴─────────┘      └──────────────┴──────────────┴───┘
 byte 0   byte 8

 address of a value = start of the chunk of values + byte
```

A sort and a grouping read these blocks from a file. Before they use a
reference, they check it: the upper half must be 1, and the byte must
lie inside the chunk of values. If not, the query stops with the error
"damaged data". Without this check a damaged word would make the node
read memory outside the chunk. The gather node gets its chunks from
another process's memory, not from a file, and does not check them.

**Many chunks of values.** A join keeps many chunks of values and
numbers them 0, 1, 2, and so on. To find a value, it keeps an array
with the addresses of its chunks of values:

```
 the join's array of chunks of values     chunk of values number 2
 ┌─────┐                                  ┌──────────────────────────┐
 │ [0] │ ──► chunk 0                      │ …        │ Kazan │ …     │
 │ [1] │ ──► chunk 1                      └──────────────────────────┘
 │ [2] │ ───────────────────────────────► ▲          ▲
 └─────┘                                  byte 0     byte 40

 reference (2, 40):  upper half 2 + 1 = 3, lower half 40
 address of the value = array[2] + 40
```

When a join reads a block of values back, it puts the address of the
new chunk into this array, at the index from the header's `number`. The
address may be different from the old one. The references do not
change, and they work again. A join writes a chunk of values before the
chunks that refer to it, so a reader has the values in memory when it
meets a reference to them. A join checks that a reference names a chunk
it has read; it does not check the byte.

Before this, a sort and a grouping stored a bare position without a
chunk number, where 0 was a normal position. There were two forms of
reference, and nothing in a block said which one it held.

A chunk of values is written as it is, without packing. Its bytes are
the values themselves.

### Columns

A chunk of columns holds rows in the same way as a batch: as columns.
It is used for the rows that only wait on disk (goal 3). The gather
node also uses such chunks to pass rows from one process to another,
without a file.

In memory, a chunk of columns has a small header and then lanes. A lane
is an array with one 8-byte word for each row. Row number `i` is word
`i` of every lane. The first lanes hold the NULL bits of the rows. After
them there is one lane for each word of a row. The spec draws this
layout and says which bit marks a NULL ([spec.md](spec.md), "A chunk of
columns in memory").

A word of a row is one of three things: a value that fits in 8 bytes,
a reference to a long value, or 0 for a NULL.

A batch copies its columns into the lanes directly, and a batch that is
read back takes its columns from the lanes directly, 64 rows at a time.
The key columns are among the stored words. The hash of a row is not
stored. It is computed again when the row is used, and that is cheap.

On disk a chunk of columns is always packed. Only the rows that the
chunk holds are stored, not its free space. For each lane Tessera finds
the smallest value. Then it stores every value of the lane as the
difference from that smallest value, in as few bytes as the largest
difference needs:

```
 a lane of order numbers           smallest value 1000001, 1 byte each
 1000001 1000002 1000003 …   ──►   0 1 2 …
```

The spec defines the encoding: the layout of the packed chunk, the
descriptor of a lane and the widths a value can have
([spec.md](spec.md), "A chunk of columns on disk").

Why differences? The values of one column are often large but close to
each other, like order numbers or dates. Their differences are small
and need few bytes.

## The files

A set has one file. The file is created when the write buffer goes to
disk for the first time, not when the first block is given. So an error
of creating the file can come from a later write, or from the end of
the writing. The set also keeps the lists of blocks, one list for each
partition. An entry of a list holds two numbers: where the block starts
in the file, and how many bytes it takes, with its header.

```
 the file of a set                            the lists of the set
 byte 0                                       partition 0: B, E
 ┌───────┬───────┬───────┬───────┬───────┐    partition 1: D
 │ A     │ B     │ C     │ D     │ E     │    partition 2: A, C
 │ p = 2 │ p = 0 │ p = 2 │ p = 1 │ p = 0 │
 └───────┴───────┴───────┴───────┴───────┘
```

A block does not go to the file at once. It first goes to the write
buffer of the set. A join and a grouping ask for a buffer of one
sixteenth of `hash_mem`, a sort for one sixteenth of `work_mem`. The
buffer is never less than 8 kB and never more than 256 kB. A block
larger than the buffer is written directly. A larger buffer would not
help much: the time of a write depends mostly on its bytes, not on the
number of calls.

There are two kinds of set.

A **serial** set belongs to one process. Its file is a normal temporary
file of PostgreSQL. It lies in the temporary tablespaces of the query,
and `temp_file_limit` applies to it. The file is deleted when the set
is freed. After an error, PostgreSQL deletes it when it releases the
resources of the query. The lists of blocks stay in memory.

A **shared** set belongs to a parallel query. Each participant writes
its own file, named `<name>.<participant>`. When a participant has
finished writing, any participant can read its file. But a reader
cannot see the memory of the writer, where the lists of blocks are. So
when a participant finishes a shared set, it writes the lists at the
end of its file, and then a trailer that says where the lists start.
The spec draws the end of such a file and the trailer
([spec.md](spec.md), "A shared set").

A reader of a shared file first reads the trailer at the end of the
file. The trailer says where the lists start. The reader then reads the
list of the partition it needs, and then the blocks of that list.

`temp_file_limit` applies to the files of a shared set too. PostgreSQL
counts it for each process separately, and the lists and the trailer
count as well.

A participant's file is deleted when the participant frees its set. Any
file that is left is deleted when the last participant leaves the shared
memory of the query, or when the file set is reset (see below).

**What the caller must do.** The format has no mark that says "this
file is finished" or "this participant wrote nothing".

- A participant that wrote no block has no file. Opening its file gives
  no reader.
- A file that does not exist yet gives the same answer. So a reader
  must not open another participant's file before that participant has
  finished its set. If it does, it gets "no blocks" and rows are lost
  without an error. If the file exists but is not finished, it gets the
  error "damaged data".
- A participant must finish its own set before it opens any file.
- A node can run again in the same query: for example, a parallel join
  under a nested loop. Its participants keep the same file names. So
  before the second run the node must reset the file set
  (`tess_spill_shared_reset`), which deletes all its files. Without the
  reset, a participant that writes nothing in the second run would leave
  its file of the first run, and the others would read old rows as new,
  with no error.

The nodes keep this order with barriers of their own. The format does
not check it.

## Writing and reading

A set first only writes, and then only reads.

1. `tess_spill_write` puts a header and a body into the write buffer.
   If a chunk of records fits in the buffer, it is packed directly into
   the buffer. The call returns the number of bytes the block takes in
   the file. It can also return where the block starts.
2. `tess_spill_finish` ends the writing. A shared set writes its lists
   and its trailer at this moment.
3. `tess_spill_open` creates a reader for one partition. The reader
   starts at the first block of the partition. Its buffer is as large
   as the largest block of the partition. For a partition without
   blocks there is no reader.
4. `tess_spill_read_header` reads the next block of the partition, the
   header and the body together, with one read. Then it checks the
   block:
   - the fields of the header are valid;
   - the fingerprint is the fingerprint of the set;
   - the partition and the level are the reader's;
   - the sizes in the header match the size in the list of blocks.

   At the end of the partition the call returns false.
5. `tess_spill_read_body` gives the body to the caller. A packed body
   is unpacked; any other body is copied.

`tess_spill_seek` moves a reader to one block. The caller gives the
position that `tess_spill_write` returned for that block. A sort uses
it to go to a block inside a run.

A partition of a serial set can have only one reader at a time. A
reader of another participant's file opens the file for itself.

`tess_spill_drop` removes the caller's list of a partition after the
partition has been read. In a shared set the other participants still
read the partition through the list in the file. The bytes stay in the
file until the set is freed. So the disk space of a set is freed all at
once, not partition by partition.

This has a cost when a partition is divided again. The node copies the
partition into the sets of the next level, while the whole file of the
level above is still on disk. With one file for each partition, the
partitions already read would be gone by then.

`tess_spill_stats` returns the number of blocks, the number of bytes
written, and the number of partitions that have blocks.
`tess_spill_memory` returns the current size of the buffers.
`tess_spill_free` frees the set and deletes this participant's file.
`tess_spill_release` only closes a shared set; other participants may
still read its file. Both calls close every open reader of the set, so
the caller must not use such a reader afterwards.

## What a reader rejects

A reader does not trust bytes that it has not checked.

- It rejects a header with a wrong magic, version, kind, fingerprint,
  length or level.
- It rejects a packed body that does not unpack into exactly the chunk
  that the header describes.
- It never uses the body of a rejected block.

The error for such a block is "damaged data", SQLSTATE `XX001`.

A wrong call is a different error. For example, the caller gives a
buffer that is shorter than a header. This is an internal error,
SQLSTATE `XX000`. The two errors must not be mixed: the first says that
the file is bad, the second says that the calling code is bad.

## What we decided not to do

- **A general compressor**, such as lz4. It is several times slower than
  packing by lanes. The time is worth more than the bytes (goal 2).
- **One file for each partition**, written in pieces of 8 kB through
  `BufFile`. One file with a larger buffer is faster and opens fewer
  files (goal 4).
- **Writing rows one at a time**, as PostgreSQL does. Every row would
  be decoded and allocated again on reading (goal 1).
- **Records for rows that only wait.** They would be converted four
  times for nothing (goal 3).
- **A checksum of the body.** The file belongs to one query and lives
  only as long as the query (goal 5).
- **A fixed byte order.** A temporary file never leaves the machine
  that wrote it.

## What is not on this page

These things belong to the join, the grouping and the sort, not to the
format:

- when a node spills;
- what a node keeps in memory;
- how rows are added to a chunk of columns.

[docs/spill.md](../../../docs/spill.md) describes them for the join and
the grouping, [docs/nodes.md](../../../docs/nodes.md) for the sort. How
many partitions a level gets and how long their chunks are
(`tess_spill_partitions`, `tess_spill_chunk_len`) is a rule of the
table that spills, in [hash-table](../hash-table/design.md).

## Files

- `crates/tessera-spill/src/lib.rs`: the header of a block and its
  checks
- `crates/tessera-spill/src/pack.rs`: the packed form of a chunk of
  records
- `crates/tessera-spill/src/columns.rs`: a chunk of columns and its
  packed form
- `crates/tessera-spill/src/damaged.rs`: the error that marks damaged
  data
- `crates/tessera-capi/src/c/spill.rs`: the C entry points
- `crates/tessera-kernels/src/spill_columns.rs`: rows appended to chunks
  of columns
- `crates/tessera-kernels/src/table/header.rs`: the fingerprint of a
  hash table
- `include/tessera/spill.h`: the C API of blocks and of chunks of
  columns
- `include/tessera/runtime_spill.h`, `runtime/spill.c`: the sets of
  files

## Tests

- The unit tests of `crates/tessera-spill/src/lib.rs`, `pack.rs` and
  `columns.rs`.
- `crates/tessera-capi/tests/spill.rs` and `tests/table.rs`: the entry
  points.
- `test/sql/spill.sql` with `test/tessera_spill_test.c`: the sets of
  files; `test/sql/table.sql`: the header through the C API.
- End to end, at a small `work_mem`: the suites `join`, `agg`, `sort`,
  `union`, `parallel` and `types`.

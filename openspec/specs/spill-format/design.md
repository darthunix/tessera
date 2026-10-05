# spill-format: how it is built

When a hash join, a grouping or a sort has more rows than its memory
allows, Tessera writes part of them to temporary files and reads them
back later. This is called spilling. This page explains what is written,
how the files are arranged and why it is done this way. The exact bytes,
limits and errors are in [spec.md](spec.md). When a node decides to
spill, and how it divides its rows, is in
[docs/spill.md](../../../docs/spill.md).

## The problem

PostgreSQL gives every node of a query a budget of memory: `work_mem`,
and for a hash table `hash_mem`, which is `work_mem` multiplied by
`hash_mem_multiplier`. A join whose inner side does not fit, a grouping
with too many groups and a sort of too many rows must still finish. The
usual answer is to divide the rows into partitions by their hash, keep
in memory what fits, write the rest to disk, and come back to each
partition when there is room for it; a sort writes sorted runs and
merges them.

That answer leaves questions, and the format is the reply to them:

1. Spilling is the slow path of a query that is already large. How
   little can writing and reading cost beyond the disk itself?
2. Tessera's nodes do not hold rows one by one. A hash table is made of
   large chunks of records, the texts and other long values lie in
   chunks of their own, and rows that only wait are kept by column.
   What exactly goes to disk?
3. In a parallel query one process writes and another reads. What may
   the bytes on disk refer to, if not addresses?
4. The bytes read back are used as structures in memory, without a
   parse of every row. How does a reader know they are what it expects?
5. A node spills because memory is short. How much memory does reading
   need, and is it known before the read starts?
6. PostgreSQL has rules for temporary files: a limit on their size,
   tablespaces for them, removal after an error. How are they kept?

## Goals and what they cost

1. **A chunk comes back ready to use.** A node writes whole chunks of
   its memory, never rows one by one, and reading gives the chunk back:
   no row is allocated or parsed, and the hash table links the records
   of a chunk into a new index in one call. PostgreSQL's own hash join
   writes a tuple at a time and allocates each on reading. *The cost:*
   the bytes on disk follow the layout in memory, so they mean something
   only to the table that wrote them; every block therefore carries a
   version and a fingerprint of that layout. One row cannot be read
   without its chunk.
2. **Smaller files for almost no processor time.** A chunk is packed by
   lanes: the same field of every record is stored together, at the
   width its values need in this chunk. On the project's reference
   machine, an Apple M5 Pro, packing runs at 7 to 10 GB/s and unpacking
   at 12 to 14 GB/s on one core; the files of a spilled join and of a
   spilled grouping are 2.7 to 3.5 times smaller and the queries up to
   13 % faster. *The cost:* less is saved than a general compressor
   would save, and long values are not packed at all. A general
   compressor was refused: lz4 runs at 1.3 GB/s, which costs more time
   than the smaller files give back.
3. **Rows that only wait are not turned into records.** The outer rows
   of a join that wait for their partition, the rows a grouping sets
   aside and the rows of a sort's run are never linked or probed, only
   written and read back once. They go to disk by column, straight from
   the columns of a batch and straight back into them. Before that the
   outer rows were records too and went from columns to records, to
   lanes, back to records and back to columns: a spilled join at a
   `work_mem` of 1 MB took 67 to 83 ms and wrote 29.5 MB; by column it
   takes 45 to 55 ms and writes 11.6 MB. *The cost:* two shapes of
   chunk, each with its own packed form.
4. **Few files and large writes.** A node writes one file for all its
   partitions, through a buffer of up to 256 kB, instead of a file a
   partition in pieces of 8 kB, as PostgreSQL's `BufFile` would. That
   took 6 to 9 % off a spilled join. *The cost:* the bytes of a
   partition that has been read stay on disk until the whole file goes.
5. **A wrong block is an error, never a wrong answer.** A reader checks
   every header and every packed body before it uses them, and reports
   a block that fails as damaged data. *The cost that was refused:* a
   checksum of the body. A temporary file is read by the query that
   wrote it, on the same machine, so a flipped bit inside a value is not
   looked for, as PostgreSQL does not look for it in its own temporary
   files.
6. **The memory of a read is known before it starts.** A header says
   both how many bytes lie on disk and how many the chunk takes in
   memory, and a reader's buffer is as large as the largest block of
   its partition.
7. **Any process of the query can read any file.** Nothing on disk is an
   address: a record refers to a long value by the number of its chunk
   and the byte in it. In a parallel query the list of a file's blocks
   is written into the file itself, since the writer's memory is not
   the readers'.
8. **One place knows PostgreSQL's files.** Every call of PostgreSQL's
   file layer for spilling is in `runtime/spill.c`. A core with another
   manager of work files, like Greengage's, replaces that file and
   nothing else.

## The whole in one picture

```
 a node: join, grouping, sort                       a temporary file
 ┌───────────────────────────┐                   ┌────────────────────┐
 │ chunks in memory          │                   │ block, partition 2 │
 │  ┌─────────┐ ┌────────┐   │  write: a header  │ block, partition 0 │
 │  │ records │ │ values │   │  before a chunk   │ block, partition 2 │
 │  └─────────┘ └────────┘   │ ────────────────► │ block, partition 1 │
 │  ┌─────────┐              │                   │ block, partition 0 │
 │  │ columns │              │  read: the blocks │ …                  │
 │  └─────────┘              │  of one partition │                    │
 │                           │ ◄──────────────── │                    │
 └───────────────────────────┘                   └────────────────────┘
```

The words used below:

- A **chunk** is a piece of a node's memory that is written whole. There
  are three kinds: records of a hash table, the long values that records
  or rows refer to, and rows kept by column.
- A **block** is a chunk on disk: a header of 48 bytes, then the stored
  body, which is the chunk as it is or its packed form.
- A **partition** is the rows whose hashes share some bits. A node reads
  a partition back as a whole, when it has room for it.
- A **level** counts how many times rows were divided. A partition that
  still does not fit when it is read back is divided again, by other
  bits of the hash, at the next level. The first level is 0.
- A **set** (`TessSpill`) is everything one node writes at one level
  for one kind of rows. A set has one file; in a parallel query each
  process has its own.
- A **participant** is a process of a parallel query: the leader or a
  worker.
- The **fingerprint** is a hash of a table's layout: its format, the
  kinds of its keys, the sizes of a record and of its payload.
- A **lane** is the same field of every record, or of every row, taken
  together, as a column is.

## A block

```
 ┌───────────────── header, 48 bytes ─────────────────┬─ stored body ──┐
 │ magic   version   kind   number   partition  level │ the chunk as   │
 │ packed length     fingerprint     body length      │ it is, or its  │
 │                                                    │ packed form    │
 └────────────────────────────────────────────────────┴────────────────┘
```

Every field is there for a reader:

- **magic and version** say that these bytes are a block of this
  format, and of which version of it.
- **kind** says which of the three chunks the body is.
- **number** is the chunk's number in its table. A record names a long
  value by the number of the value's chunk, so a chunk must come back
  under the number it had.
- **partition and level** let a reader check that the block belongs to
  what it is reading, and not to another place in the file.
- **fingerprint** ties the block to the table that wrote it. With it, a
  block of another table, or a damaged file, is an error status, never a
  record read the wrong way. The records are then checked once more, as
  any chunk is, when the table takes the chunk in.
- **body length and packed length** are the two sizes of goal 6: the
  bytes the chunk takes in memory, and the bytes that lie on disk when
  the body is packed.

The bytes are in the machine's own order and there is no checksum, for
the reason given with goal 5.

## The three kinds of body

### Records

A hash table keeps its records in chunks. A chunk begins with a mark of
the bytes used, and records of one length follow:

```
 a chunk of records
 ┌────────┬──────────┬──────────┬──────────┬─────┐
 │ used   │ record 0 │ record 1 │ record 2 │  …  │
 │ 8 bytes│          │          │          │     │
 └────────┴──────────┴──────────┴──────────┴─────┘

 a record
 ┌──────┬──────┬───────────┬──────┬─────────┬─────┬─────────┐
 │ hash │ next │ NULL bits │ len  │ key 0   │  …  │ payload │
 │  4   │  4   │    4      │  4   │   8     │     │         │
 └──────┴──────┴───────────┴──────┴─────────┴─────┴─────────┘
```

A record is built for linking and probing, not for storage. `next`, the
next record of the same bucket, means nothing on disk; `len` is the same
in every record; the NULL bits are mostly 0; and an `int4` key or value
takes a slot of 8 bytes. A record of a join on one `int4` key with one
`int4` column takes 40 bytes for 12 bytes of content.

So a chunk of records is packed before it is written, when that makes it
shorter. The records are seen as lanes of 4 bytes, and each lane is
stored at the width its values need in this chunk:

```
             hash   next   NULLs  len    key 0        payload
 record 0  │ a91f │ 0007 │ 0000 │ 0005 │ 0017 0000 │ 002a 0000 … │
 record 1  │ 03c2 │ 0000 │ 0000 │ 0005 │ 0018 0000 │ 0007 0000 … │
 record 2  │ 77b0 │ 0009 │ 0000 │ 0005 │ 0019 0000 │ 0063 0000 … │
               │      │      │      │      │    │      │    │
               ▼      ▼      ▼      ▼      ▼    ▼      ▼    ▼
 on disk     whole  left   all    all    one   all    one   all
             words  out    zero:  equal: byte  zero:  byte  zero:
                           no     one    each  no     each  no
                           bytes  word         bytes        bytes
```

The values in the picture are made up, and a lane's 4 bytes are shown
short. Nothing depends on the types of
the columns: the widths follow the values. Reading unpacks the lanes
into the caller's buffer and gives the chunk as it was, except that
every `next` is 0, as in a chunk not yet linked; the table then links
the records into a new index. A chunk that packing would not shorten is
written as it is.

### Values

A text, or any other value that does not fit a word of 8 bytes, does not
lie in a record or in a row. It lies in a chunk of values, and the
record's or the row's word holds a reference to it: the number of the
value's chunk plus one, and the byte in that chunk. PostgreSQL calls
such values by-reference. The reference is not an address, so it means
the same after the chunk has been on disk and in another process. A
reading node puts the chunk's place in memory into its array of value
chunks, under the chunk's number, and every reference to it is good
again.

A chunk of values is written as it is. It is not packed: its bytes are
the values themselves.

### Columns

Rows that only wait on disk are kept by column. In memory a chunk of
columns is a header and lanes of 8-byte words, one word a row:

```
 ┌──────────────────── header, 16 bytes ─────────────────────┐
 │ rows       capacity in rows   words a row   magic "COLS"  │
 ├───────────────────────────────────────────────────────────┤
 │ lanes of NULL bits: one lane for every 64 words of a row, │
 │ at least one. Bit w % 64 of a row's word in lane w / 64   │
 │ is set when the row's word w is NULL.                     │
 ├───────────────────────────────────────────────────────────┤
 │ lane of word 0:  row 0 │ row 1 │ row 2 │ …  │ free        │
 │ lane of word 1:  row 0 │ row 1 │ row 2 │ …  │ free        │
 │ …                                                         │
 └───────────────────────────────────────────────────────────┘
```

A row's word is a value that fits a word, a reference to a long value,
or 0 for a NULL. A batch appends its rows to a chunk straight from its
columns, and a batch read back takes its columns straight from the
lanes, 64 rows at a time. The keys are among the stored words; their
hash is not stored and is computed again when the rows are probed,
which costs little.

On disk a chunk of columns is always packed, and only its rows are
stored, not its free capacity. Each lane is stored as the differences
from its least value:

```
 ┌──────┬─────────────┬───────────────────────┬───────────────────────┐
 │ rows │ words a row │ a descriptor for each │ each lane's values:   │
 │  4   │      4      │ lane, 16 bytes: the   │ value − least, in 0,  │
 │      │             │ width and the least   │ 1, 2, 4 or 8 bytes    │
 │      │             │ value                 │ each                  │
 └──────┴─────────────┴───────────────────────┴───────────────────────┘

 a lane of order keys           least value 1000001, width 1 byte
 1000001 1000002 1000003 …  ─►  0 1 2 …
```

The values of a column are close to each other far more often than they
are small, which is why differences are stored and not the values
themselves. A lane whose values are all equal takes no bytes beyond its
descriptor.

## The files

A set writes one file, made when its first block is written. The blocks
of every partition go into it one after another, in the order the node
writes them, and the set keeps a list for each partition: where each of
its blocks starts and the bytes it takes.

```
 the file of a set                             the lists of the set
 ┌───────┬───────┬───────┬───────┬───────┐     partition 0: B, E
 │ A     │ B     │ C     │ D     │ E     │     partition 1: D
 │ p = 2 │ p = 0 │ p = 2 │ p = 1 │ p = 0 │     partition 2: A, C
 └───────┴───────┴───────┴───────┴───────┘
```

A block goes to the file through the set's write buffer: a sixteenth of
`hash_mem`, at least a block of PostgreSQL, 8 kB, and at most 256 kB. A
block larger than the buffer is written as it is. A buffer larger still
gains little, since a write costs with its bytes more than with its
calls.

There are two kinds of set.

A **serial** set belongs to one process. Its file is a temporary file of
PostgreSQL: the query's temporary tablespaces are used,
`temp_file_limit` applies, and the file is deleted when the set is
freed or, after an ERROR, when the query's resources are released. The
lists stay in memory.

A **shared** set belongs to a parallel query. Every participant writes
a file of its own, named `<name>.<participant>`, in the query's shared
set of files, and any participant can read any of them once its writer
has finished. The readers cannot see the writer's memory, so finishing
a shared set writes the lists into the file, with a trailer that says
where they are:

```
 the file of one participant
 ┌───────┬───────┬─────┬────────────────┬──────────────────┬──────────┐
 │ block │ block │  …  │ the number of  │ for every block: │ trailer, │
 │       │       │     │ blocks in each │ where it starts, │ 4 words  │
 │       │       │     │ partition      │ the bytes it     │          │
 │       │       │     │                │ takes            │          │
 └───────┴───────┴─────┴────────────────┴──────────────────┴──────────┘
                        ◄── written when the set is finished ────────►

 the trailer
 ┌───────┬───────────────────────┬──────────────────────┬─────────────┐
 │ magic │ where the lists start │ number of partitions │ fingerprint │
 └───────┴───────────────────────┴──────────────────────┴─────────────┘
```

The files of a shared set are deleted when the last participant leaves
the query's shared memory.

## Writing and reading

A set first writes and then reads; it never does both at once.

1. `tess_spill_write` puts a header and a body into the write buffer. A
   chunk of records is packed straight into the buffer when its bytes
   fit there. The call returns the bytes the block takes in the file,
   and can return where the block starts.
2. `tess_spill_finish` ends the writing. A shared set writes its lists
   and its trailer here.
3. `tess_spill_open` gives a reader at the first block of a partition,
   with a buffer as large as the partition's largest block. A partition
   without blocks gives no reader.
4. `tess_spill_read_header` reads the next block whole, header and
   stored body, in one read, and checks it: the fields of the header,
   the fingerprint against the set's, the partition and the level
   against the reader's, the lengths against the bytes on disk. It
   answers false at the end of the partition.
5. `tess_spill_read_body` gives the body: unpacked when it was stored
   packed, copied otherwise.

`tess_spill_seek` moves a reader to a block by the position its write
returned; with it the participants of a parallel query take the blocks
of one partition each on its own. A partition of a serial set has one
reader at a time; a reader of another participant's file opens a handle
of its own.

`tess_spill_drop` forgets a partition's blocks once they have been
read. Their bytes stay in the file until the set goes: the disk of a
level is freed with its set, not a partition at a time. For a join
that costs nothing at the peak, since every outer row is written before
any partition is joined.

`tess_spill_stats` gives the blocks and the bytes written and the
partitions that have blocks; `tess_spill_memory` gives the bytes of the
buffers now. `tess_spill_free` deletes this participant's file with the
set; `tess_spill_release` only closes a shared set, whose file the
other participants may still read.

## What a reader refuses

A reader trusts nothing it has not checked. A header with a wrong
magic, version, kind, fingerprint, length or level is refused, and so is
a packed body that does not unpack into exactly the chunk its header
promises. The body of a refused block is never interpreted. The error
is "damaged data", SQLSTATE `XX001`. A call used wrongly, such as a
buffer shorter than a header, is an internal error, `XX000`, and not
damaged data: the two must not be confused, since the first blames the
file and the second the caller.

## What was refused

- **A general compressor**, such as lz4. It is several times slower than
  packing by lanes, and the time is worth more than the bytes (goal 2).
- **A file for each partition**, written in pieces of 8 kB through
  `BufFile`. One file through a larger buffer is faster and opens fewer
  files (goal 4).
- **Rows one at a time**, as PostgreSQL writes them. Each would be
  parsed and allocated again on reading (goal 1).
- **Records for rows that only wait.** They would be converted twice in
  each direction for nothing (goal 3).
- **A checksum of the body.** The file is the query's own and lives as
  long as the query (goal 5).
- **A byte order fixed by the format.** A temporary file never leaves
  the machine that wrote it.

## What is not on this page

When a node spills, how many partitions it makes and how large their
chunks are (`tess_spill_partitions`, `tess_spill_chunk_len`), what it
keeps in memory, and how rows are appended to a chunk of columns belong
to the join, the grouping and the sort. Until those parts have pages of
their own, [docs/spill.md](../../../docs/spill.md) describes them.

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

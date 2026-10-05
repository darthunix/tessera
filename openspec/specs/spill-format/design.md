# spill-format: how it is built

What a spilled block is, byte by byte, and what a reader refuses is in
[spec.md](spec.md). This page says why the format is so and how the
files are handled. What the join and the grouping do with them is in
[docs/spill.md](../../../docs/spill.md).

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

## History

Items 5.6 (spilling), 5.12 (one file a set, a join's outer rows by
column) and 9.17 (rows appended by the kernels) of
[the working plan](../../../docs/plan/plan-migration.txt).

Described as a capability by the change `spec-spill-format`, pull
request 46, 2026-10-05. The text came from `docs/spill.md`, and two of
its statements were corrected on the way: the writers of chunks of
columns (the grouping and the external sort, not only the join) and the
floor of the write buffer (a block, not 32 kB). Left to the capabilities
of the join and of the grouping: the partition plan
(`tess_spill_partitions`, `tess_spill_chunk_len`) and how rows are
appended to a chunk of columns, beyond the layout the append fills. What
the comparison of the documents, the comments, the code and the tests
found waits for a decision in [the roadmap](../../roadmap.md), "Not
placed".

## What is written

A node spills chunks of its table whole, never rows one by one:

- a chunk of **records**, in the table's own format: its 8-byte used
  mark, then the records one after another, each with its hash, keys and
  payload (the `next` field is left as it was: a new index relinks the
  records when the chunk is read back);
- a chunk of **values**: the by-reference inner values the records refer
  to. A payload word holds a value's reference, the value chunk's number
  plus one and the byte in it, not an address, so the words mean the same
  after the chunk has been on disk; a reading node sets the chunk's base
  in its array of value bases under the chunk's number;
- a chunk of **columns**: rows that only wait on disk — a join's outer
  rows that wait for their partition, the rows a grouping sets aside for
  a partition, the rows of a run of the external sort
  (`crates/tessera-spill/src/columns.rs`, `tessera/spill.h`). They are
  never linked or probed as records, only written and read back once, so
  they are kept by column: lanes of NULL bits and a lane for each stored
  word (the spec, "A chunk of columns in memory"). A batch appends to
  its partitions' chunks straight from its columns
  (`tess_spill_columns_append_partitioned`), and a batch read back takes
  its columns straight from the lanes, a window of 64 rows at a time: a
  by-value word where it lies, a reference turned into a pointer. On
  disk a lane is stored for the chunk's rows only, as differences from
  its least value (the spec, "A chunk of columns on disk"): the values
  of a column are close to each other far more often than they are
  small. The keys are among the stored columns, and their hash is
  computed again when the rows are probed, which costs little. Before
  this (plan item 5.12e) the outer rows were records too, and went from
  columns to records, to lanes when packed, back to records and back to
  columns: a spilled join at a `work_mem` of 1 MB took 45 to 55 ms
  instead of 67 to 83, and wrote 11.6 MB instead of 29.5.

Reading a chunk back gives a chunk that is ready at once: no row is
allocated, and `tess_table_link` or `tess_table_link_grouped` puts its
records into a new index. PostgreSQL's own spilling writes a tuple at a
time (its hash, then the minimal tuple) and allocates each on reading.

**Packing.** A record in memory is built for linking and probing: the
next-record reference means nothing on disk, the length is the same in
every record, NULL bits are mostly 0, and an `int4` key or value takes a
slot of 8 bytes, so a record of a join on one `int4` key with one `int4`
column takes 40 bytes for 12 of content. A chunk of records therefore
goes to disk packed when that makes it shorter (`tess_spill_pack`,
`crates/tessera-spill/src/pack.rs`): its records seen as lanes of 4
bytes, the same lane of every record together, each lane stored at the
width its values need in this chunk, and the next-record lane dropped
(the spec, "A packed chunk of records", has the codes). Reading unpacks
it into the caller's buffer, a chunk as it was but for next-record
references of 0, as a chunk not yet linked has them.
Nothing depends on the column types: the widths follow the values.
Packing runs at 7 to 10 GB/s and unpacking at 12 to 14 GB/s on one
core; at the data multiplier 10 of the bench/pg spill family they cut the
join's and the grouping's files 2.7 to 3.5 times (the joins' below the
core's) and the queries' time by up to 13 %, where general compression
(lz4 1.3 GB/s) would cost more time than the smaller files save.

## The block header

The fields of the header and what a reader checks are in the spec
("Block header layout", "Kinds of block and their lengths", "Damaged and
foreign blocks are refused"). The reasons behind them:

- **The fingerprint.** `tess_table_fingerprint` is a hash of the table
  format, the key kinds and the record and payload sizes. With it a block
  of another table or a damaged file is an error status, never a record
  read the wrong way. The records themselves are then checked as any
  chunk is, when the table attaches them.
- **No checksum, native byte order.** Temporary files are read by the
  query that wrote them, on the same machine, as PostgreSQL's are.
- **Two lengths.** The body length is the chunk's size in memory, which
  a reader allocates; the packed length is what lies on disk when the
  body is stored packed. A reader therefore knows both before it reads
  the body.

## The files of a set

The runtime library writes and reads the blocks (`runtime/spill.c`,
declared in `tessera/runtime_spill.h`). A node makes a `TessSpill` per
level of partitioning, with the table's fingerprint, the longest body it
accepts, the number of partitions and the bytes of its write buffer
(`TESS_SPILL_BUFFER_LEN`: a sixteenth of `hash_mem`, from a block of
PostgreSQL, 8 kB, to 256 kB). A set writes one file, made on its first
block: the blocks of every partition go into it one after another,
through the write buffer, and the set keeps each partition's list of
blocks, where each starts and the bytes it takes. A block larger than
the buffer is written as it is. BufFile, a file per partition written in
pieces of 8 kB, is not used: one file through a larger buffer took 6–9 %
off a spilled join (plan item 5.12). A buffer larger still gains little,
since a write costs with its bytes more than with its calls.

- A **serial** set writes a PostgreSQL temporary file
  (`OpenTemporaryFile`): the query's temporary tablespaces are looked up
  when the set is made, `temp_file_limit` applies, and the file is
  deleted when the set is freed or, after an ERROR, when the query's
  resources are released.
- A **shared** set writes this participant's file of a `SharedFileSet`
  in the node's chunk of the query's shared memory
  (`tess_spill_shared_init` in the leader, `tess_spill_shared_attach` in a
  worker), named `<name>.<participant>`. `tess_spill_finish` writes the
  lists of the blocks and a trailer at the file's end (the spec, "A
  shared set"), since the writer's memory, where a serial set keeps the
  lists, is not the readers'; every participant opens any participant's
  file once its writer finished the set, and reads its partition's list
  from there. The files are deleted
  when the last participant detaches the segment.

A set first writes: `tess_spill_write` puts a header and the body into the
buffer, a chunk of records packed straight into it when its raw bytes fit,
and can return where the block starts. After `tess_spill_finish` it reads:
`tess_spill_open` gives a reader at the partition's first block, with a
buffer of its largest block, so that its memory is known before any read;
`tess_spill_read_header` reads the next block whole, header and stored
body, in one read and checks the header against the set's fingerprint,
longest body, partition, level and the bytes on disk; then
`tess_spill_read_body` unpacks or copies its body; `tess_spill_seek` goes
to a block by its position, which lets participants take blocks of one
partition each on their own. A serial partition has one reader at a time,
as before; a shared reader of another participant opens a handle of its
own. `tess_spill_drop` forgets a partition's blocks once they have been
read; their bytes stay in the file until the set goes, so a level's disk
is freed with its set, not a partition at a time (every outer row is
written before any partition is joined, so the peak is the same).
`tess_spill_stats` gives the blocks and bytes written and the partitions
with blocks, `tess_spill_memory` the bytes of the buffers now.
`tess_spill_free` deletes this participant's file with the set;
`tess_spill_release` only closes a shared set's, which the other
participants may still read.

These are the only calls of PostgreSQL's file layer for spilling: a core
with another manager of work files, like Greengage's, replaces this file.

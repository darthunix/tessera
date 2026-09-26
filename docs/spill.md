# Temporary storage for the hash nodes

TessHashJoin and TessAgg keep their state in the hash table of
[table.md](table.md). When the table would outgrow `hash_mem`, a node
writes parts of it to PostgreSQL's temporary files and reads them back
later (plan item 5.6). This guide describes what goes to disk and how it
is checked; the nodes' policies (partitions, recursion, what stays in
memory) are in [nodes.md](nodes.md).

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
  in its array of value bases under the chunk's number.

Reading a chunk back gives a chunk that is ready at once: no row is
decoded or allocated, and `tess_table_link` or `tess_table_link_grouped`
puts its records into a new index. PostgreSQL's own spilling writes a
tuple at a time (its hash, then the minimal tuple) and allocates each on
reading.

## The block header

Each chunk goes to disk as a header of `tess_spill_header_size()` bytes
(48) and then the chunk's bytes (`include/tessera/spill.h`,
`crates/tessera-spill`):

```
 0  magic "TESSSPIL"    8  version = 1    12  kind (1 records, 2 values)
16  chunk number       20  partition     24  level (below 32)   28  reserved = 0
32  table fingerprint  40  body length (a multiple of 8; at least 8 for records)
```

`tess_spill_header_read` checks the magic, the version, the kind, the
reserved word, the body length against the most the reader accepts, and
the fingerprint against the reading table's (`tess_table_fingerprint`: a
hash of the table format, the key kinds and the record and payload
sizes), so that a block of another table or a damaged file is an error
status, never a record read the wrong way; the records themselves are
then checked as any chunk is, when the table attaches them. The header is
in native byte order and carries no checksum: temporary files are read by
the query that wrote them, on the same machine, as PostgreSQL's are.

## Files

The runtime library writes and reads the blocks
(`runtime/spill.c`, declared in `tessera/runtime.h`). A node makes a
`TessSpill` per level of partitioning, with the table's fingerprint, the
longest body it accepts and the number of partitions; each partition gets
a file on its first block, so a partition that never spills has none.

- A **serial** set writes PostgreSQL's temporary files
  (`BufFileCreateTemp`): the query's temporary tablespaces are looked up
  when the set is made, `temp_file_limit` applies, and the files are
  deleted when the set drops them or, after an ERROR, when the query's
  resources are released.
- A **shared** set writes this participant's files of a `SharedFileSet`
  in the node's chunk of the query's shared memory
  (`tess_spill_shared_init` in the leader, `tess_spill_shared_attach` in a
  worker), named `<name>.<participant>.<partition>`; every participant
  opens any participant's file once its writer finished the set, and the
  files are deleted when the last participant detaches the segment.

A set first writes: `tess_spill_write` puts a header and the body into the
partition's file and can return where the block starts. After
`tess_spill_finish` it reads: `tess_spill_open` gives a reader at the
file's first block, `tess_spill_read_header` the next header, checked
against the set's fingerprint, longest body, partition and level, then
`tess_spill_read_body` its body whole; `tess_spill_seek` goes to a block
by its position, which lets participants take blocks of one file each on
their own. A serial file has one reader at a time, since the reader moves
the file's own position; a shared reader opens a handle of its own.
`tess_spill_drop` deletes a partition's file once it has been read, and
`tess_spill_stats` gives the blocks and bytes written and the files open,
each of which holds a buffer of one page.

These are the only calls of PostgreSQL's file layer for spilling: a core
with another manager of work files, like Greengage's, replaces this file.

## In the hash join

TessHashJoin spills a table of its own ([nodes.md](nodes.md),
"Spilling"); a shared table does not spill yet.

**Partitions and bits.** A partition is `(hash >> shift) &
(npartitions - 1)` of the 32-bit hash the table stores; the buckets take
its high bits, the first level of partitions its low ones from bit 0,
and a level below the bits right above its parent's. A partition's
chunks are small, `hash_mem / (16 × partitions)` from 8 kB to 1 MB, so
that every partition's tail on both sides fits in half of `hash_mem`.

**Files.** Each level has two sets: the inner side's partitions and the
outer rows written, each a table of its own layout and fingerprint.
Both sides append the same way: a row goes into its partition's chunk,
then its by-reference values into the partition's value chunks, and a
chunk that fills is written after the value chunks opened since the
last one. A file is therefore a series of groups, value chunks and then
the chunks of records that refer to them (a partition that went to disk
whole makes one group of all it had); without by-reference columns,
every chunk is a group. The outer rows are read back one chunk at a
time, with only that chunk's values in memory.

**What never goes to disk.** The resident partitions, joined while the
outer child is read; each partition's tail chunk and the value chunks
after the last write, joined as they are, so a partition whose rows fit
in its tail never touches the disk; the outer rows without a pair, found
so by the empty inner partition or by the Bloom filter of every inner
row, answered at once; and the first level's filter itself, freed once
the outer child is done.

**A partition too large.** Its file is compared with what `hash_mem`
leaves once the rest of spilling, on every level, is counted. One that
holds less than nine tenths of the inner rows its level split splits
into a level below while bits last; a single key cannot split, and its
partition is joined in pieces of whole groups, the outer rows read once
per piece, a bit per outer row recording a pair for left, semi and anti
joins, and a last pass without a table answering left and anti joins'
rows without one.

**Memory.** The spill's contexts use small blocks, so that a chunk of a
few kB or more takes a block of its own size; `Memory Usage` counts the
tables, the chunks, the values and the filter of every level.

## In the grouping

TessAgg spills a table of its own ([nodes.md](nodes.md), "Spilling"
under TessAgg). A group's record holds its keys and its states, no
by-reference value, so only chunks of records go to disk, one file per
partition and level.

**States, not rows.** The core's hash aggregate stops creating groups
once full and writes the input rows of new groups; TessAgg writes a
partition's groups whole when memory runs short, and the partition's
rows go on folding into new records in memory. A group whose partition
went to disk several times has several records, one per time; merging
them (`tess_table_combine`) adds counts and sums and keeps extremes. A
hot group costs a record per eviction, not a row per row; a group of
few rows spread through the input may cost more than its rows would.

**One index.** Every partition's chunks lie under one index while the
input is read, a new group going to its partition's chunk
(`tess_table_find_or_insert_partitioned`); sending a partition to disk
frees its chunks and makes the index anew over the rest.

**Giving out.** After the input, each partition makes a table of its
own: its records in memory, each group once, are linked, and its chunks
read back merge in. A partition too large splits by the next bits into
a level below first; a split does not find a group's other records, so
that level merges its chunks in memory as it merges those from disk.

**Partial mode.** Under a `Gather` the node writes nothing: past seven
eighths of `hash_mem` it sends its groups up as partials, which the
core's Finalize Aggregate merges, and starts its table anew.

**Memory.** The first index takes at most a quarter of `hash_mem`, a
chunk an eighth; the node acts at seven eighths, the rest left for a
batch's growth, and counts its files' buffers, a page each.

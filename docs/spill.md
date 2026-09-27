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
  in its array of value bases under the chunk's number;
- a chunk of **columns**: a join's outer rows that wait for their
  partition (`crates/tessera-spill/src/columns.rs`, `tessera/spill.h`).
  They are never linked or probed as records, only written and read back
  once, so they are kept as a header and then a lane per column of the
  chunk's capacity: the rows' NULL bits, a lane per 64 stored words (bit
  `w % 64` of lane `w / 64` for word `w`), then a word per stored column (a
  by-value Datum, or a value's reference as above; 0 for a NULL). A batch
  appends to its partitions' chunks straight from its columns
  (`tess_spill_columns_append_partitioned`), and a batch read back takes
  its columns straight from the lanes, a window of 64 rows at a time: a
  by-value word where it lies, a reference turned into a pointer. On disk
  each lane is stored for the chunk's rows only, by frame of reference:
  the lane's least value and each value's difference from it in 1, 2, 4
  or 8 bytes, or none when all are equal. The keys are among the stored
  columns, and their hash is computed again when the rows are probed,
  which costs little. Before this (plan item 5.12e) the outer rows were
  records too, and went from columns to records, to lanes when packed,
  back to records and back to columns: a spilled join at a `work_mem` of
  1 MB took 45 to 55 ms instead of 67 to 83, and wrote 11.6 MB instead of
  29.5.

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
width its values need in this chunk: nothing when all are 0, one word
when all are equal, or 1, 2 or 4 bytes each; the next-record lane is
dropped. Reading unpacks it into the caller's buffer, a chunk as it was
but for next-record references of 0, as a chunk not yet linked has them.
Nothing depends on the column types: the widths follow the values.
Packing runs at 7 to 10 GB/s and unpacking at 12 to 14 GB/s on one
core; at the data multiplier 10 of the bench/pg spill family they cut the
join's and the grouping's files 2.7 to 3.5 times (the joins' below the
core's) and the queries' time by up to 13 %, where general compression
(lz4 1.3 GB/s) would cost more time than the smaller files save.

## The block header

Each chunk goes to disk as a header of `tess_spill_header_size()` bytes
(48) and then the chunk's bytes (`include/tessera/spill.h`,
`crates/tessera-spill`):

```
 0  magic "TESSSPIL"    8  version = 2    12  kind (1 records, 2 values)
16  chunk number       20  partition     24  level (below 32)
28  packed length (bytes on disk of a packed chunk of records, 0 for a body as it is)
32  table fingerprint  40  body length (a multiple of 8; at least 8 for records)
```

`tess_spill_header_read` checks the magic, the version, the kind, the
packed length (records only, a multiple of 8 below the body length), the
body length against the most the reader accepts, and
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
longest body it accepts, the number of partitions and the bytes of its
write buffer (`TESS_SPILL_BUFFER_LEN`: a sixteenth of `hash_mem`, 32 to
256 kB). A set writes one file, made on its first block: the blocks of
every partition go into it one after another, through the write buffer,
and the set keeps each partition's list of blocks, where each starts and
the bytes it takes. A block larger than the buffer is written as it is.
BufFile, a file per partition written in pieces of 8 kB, is not used:
one file through a larger buffer took 6–9 % off a spilled join (plan
item 5.12). A buffer larger still gains little, since a write costs with
its bytes more than with its calls.

- A **serial** set writes a PostgreSQL temporary file
  (`OpenTemporaryFile`): the query's temporary tablespaces are looked up
  when the set is made, `temp_file_limit` applies, and the file is
  deleted when the set is freed or, after an ERROR, when the query's
  resources are released.
- A **shared** set writes this participant's file of a `SharedFileSet`
  in the node's chunk of the query's shared memory
  (`tess_spill_shared_init` in the leader, `tess_spill_shared_attach` in a
  worker), named `<name>.<participant>`. `tess_spill_finish` writes the
  lists at the file's end, a count of blocks per partition, then the
  blocks by partition, then a trailer that says where they start; every
  participant opens any participant's file once its writer finished the
  set, and reads its partition's list from there. The files are deleted
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

## In the hash join

TessHashJoin spills a table of its own ([nodes.md](nodes.md),
"Spilling"), and a shared one (below).

**Partitions and bits.** A partition is `(hash >> shift) &
(npartitions - 1)` of the 32-bit hash the table stores; the buckets take
its high bits, the first level of partitions its low ones from bit 0,
and a level below the bits right above its parent's. A partition's
chunks are small, `hash_mem / (16 × partitions)` from 8 kB to 1 MB, so
that every partition's tail on both sides fits in half of `hash_mem`.

**Files.** Each level has two sets: the inner side's partitions, in
chunks of records, and the outer rows written, in chunks of columns,
each with its own fingerprint. Both sides append the same way: a row
goes into its partition's chunk,
then its by-reference values into the partition's value chunks, and a
chunk that fills is written after the value chunks opened since the
last one. A file is therefore a series of groups, value chunks and then
the chunks of records that refer to them (a partition that went to disk
whole makes one group of all it had); without by-reference columns,
every chunk is a group. The outer rows are read back one chunk at a
time, with only that chunk's values in memory.

**Which partitions stay.** While the build takes more than `hash_mem`,
counted as the node reports it, the largest resident partition goes to
disk; room stays for the outer side's tails of the partitions on disk
only (a chunk of columns, one of values when the outer side keeps a
by-reference column, and a file's buffer), since a resident partition
writes no outer row. Resident partitions holding less than a quarter of
the inner rows go to disk too, once that is so: probing them would cost
every outer batch the whole probe for the few rows of theirs, more than
writing them saves (1 resident partition of 32 made a join of 5 M inner
rows 3 % slower than none, 2 of 4 made one of 1 M 7 % faster).

**What never goes to disk.** The resident partitions, joined while the
outer child is read; the outer rows without a pair, found so by the
empty inner partition or by the Bloom filter of every inner row,
answered at once; and the first level's filter itself, an eighth of
`hash_mem` at most (a smaller one lets more rows through), freed once
the outer child is done. A partition's tails, its last chunk and value
chunks of each side, are written and freed when its level starts
joining: kept, every partition's tails left the partition being joined,
or a level below, little of a small `hash_mem`, and a semi join at a
`hash_mem` of 256 kB split partitions 9483 times (636 ms against the
core's 16; 20 splits and 37 ms written).

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

**A shared table.** Under a `Gather` the participants build one table in
the query's shared memory, and its budget is every participant's
`hash_mem`, as the core's. Their chunks' bytes are counted in words
every participant maps ([table.md](table.md), `shared_spill.rs`); the
first whose chunks pass the budget publishes a filter of every inner
row and splits the table into partitions, two per participant at least.
Each participant, once it sees the split after a batch, splits its own
chunks so far into partitions whose chunks are in shared memory too and
appends partitioned from then on; while the chunks take more than the
budget, the largest partition goes to disk, marked by one participant,
and every participant writes its own chunks of it to its own files of
the table's `SharedFileSet`. The chunk lists are each participant's
own, so none is unlinked from a list others add to.

After the inner side (`FLUSH`), each participant writes its tails of
the partitions on disk and hands its chunks of the others to the table,
which is indexed for them alone. Then (`OUTER`) every participant writes
its share of the outer side before any row goes out, since a participant
that returns rows may not wait at a barrier, as the core's rule is: the
rows of the partitions on disk to their files, the rows of the
partitions in memory, and for a left or anti join those without a pair
or with a NULL key, to a file the shared table answers; the core writes
all of its outer side, this node what its filter lets through. Each
participant then reads such files one at a time as it takes them
(`tess_table_spill_take_file`) and probes the shared table, and leaves
the build (the last one frees the table).

Then each participant goes round the partitions on disk from a start of
its own (`tess_table_spill_start`), so that they spread. A partition
whose blocks and index fit in one participant's `hash_mem`, as the
elected participant of `SIZE` decides from the bytes and blocks every
participant wrote, is a round, as the core's batches are: the
participants that come elect one, which makes its index in shared
memory; all load its inner files, one at a time as they take them, each
block into a block of shared memory, a chunk of records numbered by the
round's counter and linked at once; all probe it with its outer files,
taken the same way, and leave without waiting, the last one freeing it.
One that comes when the round is past loading has nothing to do there.
A larger partition, a skewed key's, is taken whole by one participant
(`tess_table_spill_take_alone`) and joined from every participant's
files as a serial table joins its partitions, splitting or in pieces;
the others meanwhile take the rounds and the other partitions, so a skew
holds up one participant, not all. A participant that releases its set leaves its files for
the others (`tess_spill_release`); the set deletes them when the last
participant detaches, or at a rescan.

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
frees its chunks and makes the index anew over the rest. So once the
table passes seven eighths of `hash_mem`, the largest partitions go to
disk until it takes half, and the index is made once for them all:
evicting one partition at a time made it anew after each, 3598 times for
5 M groups of a row each at a `work_mem` of 4 MB (1.38 s against the
core's 0.94; now 0.48). An index a batch could fill counts twice its
size as it would grow, so that eviction comes first, never a larger
index in the middle of a batch.

**Giving out.** After the input, a partition with records on disk also
writes its chunks in memory, since it merges from disk anyway: kept,
they narrow the room the others merge in (sixteen of 50 to 100 kB took
1.3 MB of 2 in the tests, and partitions split that fit alone). Then
the partitions wholly in memory go out first, and each partition makes
a table of its
own: its records in memory, each group once, are linked, and its chunks
read back merge in. What a merge holds is a record per group, not the
file: a group evicted many times has as many records on disk and one
after the merge. Each partition therefore keeps an estimate of its
groups (HyperLogLog over the hashes of every record made in it, 64
registers, as the core's hash aggregate keeps one per spilled
partition), and only a partition whose groups, with a third more for the
estimate's error, would not fit splits by the next bits into a level
below first; a split does not find a group's other records, so that
level merges its chunks in memory as it merges those from disk. Grouping
20 M rows into 1 M groups at a `work_mem` of 4 MB, the estimate took the
files from 447 MB to 211 MB (the core writes 446 MB).

**Partial mode.** Under a `Gather`, past seven eighths of `hash_mem`,
the node sends its groups up as partials, which the core's Finalize
Aggregate merges, and starts its table anew, when that folds: a table of
fewer groups than half the rows read since it started. Groups spread
over the input fold nothing before the table fills, a group per row
read, and sending them up would hand the Finalize Aggregate every row
(1 M groups of 20 M rows at a `work_mem` of 4 MB: 6.15 M partials of
6.7 M rows per participant, and the core's Finalize wrote 0.59 GB). Then
the node spills from then on, as a serial one, and gives its groups out
as partials once the input is done, as the core's partial hash
aggregate does: that grouping ran in 1.03 s instead of 2.04 (the core
1.35–1.51).

**Memory.** The first index takes at most a quarter of `hash_mem`, a
chunk an eighth; the node acts at seven eighths, the rest left for a
batch's growth, and counts its files' buffers, a page each.

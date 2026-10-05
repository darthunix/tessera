# Temporary storage for the hash nodes

TessHashJoin and TessAgg keep their state in the hash table of
[table.md](table.md). When the table would outgrow `hash_mem`, a node
writes parts of it to PostgreSQL's temporary files and reads them back
later. What goes to disk and how it is checked is the
capability `spill-format`: its
[spec](../openspec/specs/spill-format/spec.md) says what a block and a
file are, its [design](../openspec/specs/spill-format/design.md) why.
This guide describes what the two nodes do with them; the rest of their
policies is in [nodes.md](nodes.md).

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

**Which partitions stay.** By the rule of the weights (below): while
the build takes more than `hash_mem`, counted as the node reports it,
the largest resident partition goes to disk; room stays for the outer
side's tails of the partitions on disk only (a chunk of columns, one of
values when the outer side keeps a by-reference column, and a file's
buffer), since a resident partition writes no outer row. Resident
partitions holding less than a quarter of the inner rows go to disk
too, once that is so: probing them would cost every outer batch the
whole probe for the few rows of theirs, more than writing them saves (1
resident partition of 32 made a join of 5 M inner rows 3 % slower than
none, 2 of 4 made one of 1 M 7 % faster).

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

**A partition too large.** Its file is compared with two thirds of what
`hash_mem` leaves once the rest of spilling, on every level, is counted
(the rule of a split, below). One that holds less than nine tenths of
the inner rows its level split splits into a level below while bits
last; a single key cannot split, and its
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
budget, the largest partition goes to disk, marked by one participant
(the rule of the weights, one partition a check), and every participant
writes its own chunks of it to its own files of the table's
`SharedFileSet`. The chunk lists are each participant's
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

There is no skew table as the core's hash join keeps (the inner rows of
the outer side's most common keys held in memory, so that their outer
rows are not written): an outer row of a hot key goes to its partition
on disk like any other. Measured, it costs no loss to the core: half the
outer rows of one key, joined with 100 000 inner rows at a work_mem of
1 MB (8 partitions, 9 MB on disk), take 43 ms against the core's 139,
and the same join without the skew 46 against 164 (bench family exec,
`skew_join` and `even_join`).

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
disk until it takes half (the rule of the weights, below; a partition
that went before is among them, its new groups' bytes weighed as any
other's), and the index is made once for them all:
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
below first (the rule of a split, below); a split does not find a group's other records, so that
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
1.35–1.51). Groups of sum states spill no state,
since a merge of spilled records takes a word an aggregate, nor rows, which
the table sent up would lose: they go up every time the table fills,
their rests' memory counted, and the grouping above merges them.

**Memory.** The first index takes at most a quarter of `hash_mem`, a
chunk an eighth; the node acts at seven eighths, the rest left for a
batch's growth, and counts its files' buffers, a page each.

## The weights

Which partition goes to disk, and when, is one rule
(`tess_table_spill_evict`, `Spill::evict` in `shared_spill.rs`), over
the words that count each partition's bytes in memory and mark those on
disk: a process's own words for its own spill, the shared ones for a
shared table. A check sends partitions while the memory, with a reserve
for each partition on disk, passes a share of the limit, `start` for
its first and `target` for each after it: each time the partition with
the most bytes in memory, those already on disk weighed by `spilled` (0
leaves them out). Then, with any partition on disk and those in memory
holding fewer than a share `resident` of the records, each of these
goes as well. A check sends `per_check` partitions at most (0 for any).

The nodes differ by the weights alone, settings of their own whose
defaults are the rules above:

| weight | grouping | hash join | shared hash join |
|---|---|---|---|
| `start` | `tessera.agg_spill_start`, 7/8 | `tessera.join_spill_start`, 1 | the same, of the budget |
| `target` | `tessera.agg_spill_target`, 1/2 | `tessera.join_spill_target`, 1 | the same |
| `spilled` | `tessera.agg_spill_spilled_weight`, 1 | `tessera.join_spill_spilled_weight`, 0 | the same |
| reserve | none: its memory counts its files' buffers | `tessera.join_spill_tail_weight` × a tail, 1 | none: the words count the table's chunks |
| `resident` | none | `tessera.join_spill_resident_share`, 0.25 | none: the words count records once built |
| `per_check` | any | any | `tessera.join_shared_spill_evictions`, 1 |

A grouping's memory is its contexts' and its index's, with the room a
batch's index may grow by; a partition that went to disk takes new
groups into memory and may go again.

A shared table's participants send one partition a check, since the
others write their chunks of it at their next batch; a participant whose
check found the partition marked by another one first sends none.

A partition read back from disk splits into a level below by one rule
too (`tess_table_spill_splits`): its size, as the node estimates it,
passes a share `room` of what `hash_mem` leaves besides the rest of the
spill, two bits of the hash are left for the level below, and, with a
share `key`, the partition holds fewer than that share of its level's
rows (more is one key, which no split parts, and is joined in pieces).

| weight | grouping | hash join |
|---|---|---|
| size | its groups by their estimate, a record and two words of index each, and a chunk | its file |
| `room` | `tessera.agg_spill_split_room`, 1 | `tessera.join_spill_split_room`, 2/3 (the rest for the index) |
| `key` | none | `tessera.join_spill_split_key_share`, 0.9 |

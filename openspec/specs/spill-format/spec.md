# spill-format Specification

## Purpose
The C API over the spill format of the crate `relspill`, and what
PostgreSQL and SQL see of a spill. The hash nodes and the external sort
write chunks of their memory to temporary files as blocks, and read
them back.

A chunk is a piece of a node's memory that is written whole: records of
a hash table, the values they refer to, or rows kept by column. A block
is a chunk on disk: a header, then the stored body. A set is the blocks
a node writes into one file, with a list of the blocks of each of its
partitions. For a join and a grouping a partition is the rows of some
bits of the hash, and a set belongs to one level of partitioning; for
the sort a partition is one sorted run. The blocks, their packed forms
and what each call of the crate promises are in the spec of
[relspill](../relspill/spec.md). The requirements here start with the
C calls over the crate, then the errors of a damaged block, the chunks
of columns, the file of a set, a shared set, reading a partition and
the memory of a set. When a node spills and what it keeps in memory
belong to the node's own capability. [design.md](design.md) explains
the whole and the reasons.

## Requirements

### Requirement: The C API over the crate
Each call of `tessera/spill.h` over a block SHALL do what the calls of
the crate `relspill` do, as the requirement of the spec of `relspill`
named here says, and SHALL return the crate's error as a status, as
"Damaged and foreign blocks are refused" says:

- `tess_spill_header_size`, which returns 48, `tess_spill_header_write`
  and `tess_spill_header_read`: "The header of a block";
- `tess_spill_pack` and `tess_spill_unpack`: "A packed chunk of
  records";
- `tess_spill_columns_init`, `tess_spill_columns_pack` and
  `tess_spill_columns_unpack`: "A chunk of columns" and "A chunk of
  columns on disk";
- `tess_spill_columns_append` and
  `tess_spill_columns_append_partitioned`, which take Datums and their
  types: "A chunk of columns" and "A reference to a value";
- `tess_spill_columns_pair`, `tess_spill_columns_check` and
  `tess_spill_columns_window`, which gives a window of a checked chunk's
  first columns as Datums and NULL flags, a value by reference as a
  pointer into the values: "The pair of a chunk of columns".

Beside them the C API SHALL check what the crate leaves to its caller:
`tess_spill_columns_check` SHALL check that a value ends inside the
values by its type's length, and a window of words or rows past the
chunk's SHALL be an internal error, SQLSTATE `XX000`.

#### Scenario: A header through the C API
- **WHEN** a module calls `tess_spill_header_size()` and writes and
  reads a header through `tess_spill_header_write` and
  `tess_spill_header_read`
- **THEN** the size is 48 and the fields read back as written
- **Verified by:** `test/sql/table.sql::tessera_test_spill_header`

#### Scenario: A chunk of columns through its entry points
- **WHEN** rows of a column by value and of a varlena, with NULLs, are
  appended, checked with their values and read by windows through the
  entry points; a block of the pair of another kind or number is read,
  or a chunk with a word that refers past its values; and a window past
  the rows or a block past the pair is asked for
- **THEN** every word, NULL and value is the one written, a varlena a
  pointer to its bytes in the values; the damaged ones are `XX001`, and
  the window and the block past the pair `XX000`
- **Verified by:**
  `crates/tessera-capi/tests/spill.rs::a_chunk_of_columns_reads_back_through_its_entry_points`

#### Scenario: Packed and plain blocks in a file
- **WHEN** a chunk of 1000 records that packs and bytes that are no
  chunk of records are written to a set and read back
- **THEN** the first takes its header and packed body in the file, the
  second its header and whole body, and both read back as written but
  for the next-record references
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_packed`

### Requirement: Damaged and foreign blocks are refused
A block read back SHALL pass the checks of the crate's reader, as "The
header of a block", "A packed chunk of records" and "Any bytes, a
bounded read" of the spec of `relspill` say, before Tessera uses it,
and a body that failed them SHALL NOT be read. A failed check SHALL be
reported as damaged data: status `TESS_ERROR_DATA_CORRUPTED` with
SQLSTATE `XX001`. A misuse of a call, such as a buffer shorter than a
header, SHALL be reported as an internal error, SQLSTATE `XX000`, not as
damaged data. A node MUST also check a block it reads back against what
it wrote there: its kind, a chunk of columns' stored words and, where
the node counted them, its rows, and a block of values' number against
the chunks of values written, or, where each writer of a file numbers
its own, against the number of the block of values before it. A call of
the hash table over a chunk of records read back that the table refuses
SHALL be damaged data too, though the table's own calls report it as a
wrong argument: they cannot tell damaged bytes from a wrong call, and
take either as a wrong argument.

#### Scenario: A block of another table
- **WHEN** a valid block is read with the fingerprint of another table
- **THEN** the read raises an ERROR
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_error(7)`

#### Scenario: A damaged packed body of records
- **WHEN** a packed body of records read from a file has a lane code
  that does not exist
- **THEN** the read raises `XX001`
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_sqlstate(23)`

#### Scenario: The status and SQLSTATE of damage and of misuse
- **WHEN** the C entry points read a damaged header or unpack a damaged
  body, and when they are given a buffer shorter than a header
- **THEN** the first report `XX001` and the second `XX000`
- **Verified by:**
  `crates/tessera-capi/tests/spill.rs::damaged_blocks_report_data_corrupted`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(5)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(7)`

#### Scenario: A block a node did not write so
- **WHEN** a node reads back a block of records where it wrote a chunk
  of columns, a chunk of columns of 3 words where it wrote 4, one of 4
  rows where it counted 5, and a block of values numbered 2^30 of 3
  written
- **THEN** each is damaged data, SQLSTATE `XX001`
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_sqlstate(29)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(30)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(31)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(32)`

#### Scenario: Damage under a node
- **WHEN** a join, a sort or a grouping that spills reads back a block
  damaged past the reader's own checks: one of another kind, a block of
  values numbered past those written or as the one read before it, a
  block of a sort's run numbered as the run's next, a chunk of columns
  of a word or a row fewer than was written, a value longer than the
  bytes left in its chunk, a word that refers past every chunk or is 0
  in a row it does not mark NULL, or a chunk of records whose used mark
  ends no record, which the hash table refuses as the node splits,
  links or merges it
- **THEN** the query fails as damaged data, SQLSTATE `XX001`
- **Verified by:**
  `test/sql/spill_damage.sql::spec: spill-format/Damage under a node`

### Requirement: The accessors of a chunk of columns
The inline accessors of `tessera/spill.h` SHALL read and write a chunk
of columns in the layout of "A chunk of columns" of the spec of
`relspill`: `tess_spill_columns_rows`, `tess_spill_columns_capacity`,
`tess_spill_columns_words` and `tess_spill_columns_set_rows`, the
fields of its header; `tess_spill_columns_null_lanes`,
`tess_spill_columns_null_lane`, `tess_spill_columns_null_bit` and
`tess_spill_columns_is_null`, its NULL bits; `tess_spill_columns_lane`
and `tess_spill_columns_word`, its lanes. They SHALL agree with the
offsets and the counts the kernels' entry points give of it: the fields
of its header, `tess_spill_columns_layout`, and the lanes of NULL bits,
the lane of a word and the room a packed chunk may take past its
lanes, `tess_spill_columns_shape`.

#### Scenario: The C formulas match the kernels'
- **WHEN** the C macros and the kernels' entry points compute the lanes
  of NULL bits and the room a packed chunk may take past its lanes at
  every count of words up to 4096
- **THEN** they agree, word `w` has its NULL bit at bit `w % 64` of lane
  `w / 64`, and an unknown question to `tess_spill_columns_shape`
  answers 0
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_lanes`;
  `crates/tessera-capi/tests/table.rs::chunks_of_columns_round_trip_through_the_entry_points`

#### Scenario: Joins that keep wide rows on disk
- **WHEN** joins that keep 70 and 1100 outer columns on disk run
- **THEN** they return the core's rows
- **Verified by:** `test/sql/join.sql::70 outer columns kept on disk`;
  `test/sql/join.sql::1100 outer columns kept on disk`

### Requirement: A reference to a by-reference value
A word of a record or of a row that refers to a by-reference value SHALL
be the reference of "A reference to a value" of the spec of `relspill`:
`tess_spill_value_byte` SHALL give the byte it names, and
`tess_spill_value_in` the value it names in the one chunk of values of
a chunk of columns, or none when it is no reference into them. A reader
of a chunk of columns from a file MUST refuse, as damaged data, a word
of a by-reference column that is no reference into its chunk of values.
A reader of any value through a reference from a file MUST refuse, as
damaged data, a chunk it did not read and a value that does not end
inside its chunk. The bytes a reference names SHALL be the value's own,
as `datumCopy` copies them: an expanded object, which is a pointer into
its owner's memory, SHALL be flattened, and
`tess_spill_columns_append`, which copies a batch's values by their
bytes, SHALL refuse one.

#### Scenario: What is a reference into a chunk of values
- **WHEN** a word names the first or the last byte of a chunk of
  columns' one chunk of values; and a word is 0, a bare offset, names
  another chunk, or names the byte past the chunk
- **THEN** the first two give the value's place and the others are no
  reference; the report of such a word in a row read back is damaged
  data, SQLSTATE `XX001`
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_value`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(28)`

#### Scenario: A value read back whole
- **WHEN** a reference read back names a chunk past those read, one not
  read, one other than the chunk read with it, or the byte past its
  chunk; or a varlena longer than the bytes left, one whose header of 4
  bytes the chunk cuts, a compressed one whose header of 8 bytes the
  chunk cuts, a TOAST pointer into a process's memory, a C string
  without its end, a value of a fixed length past the chunk; and when
  values of each kind end at the chunk's last byte
- **THEN** the first are damaged data, SQLSTATE `XX001`, and the last
  are read
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_sqlstate(33)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(34)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(41)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(35)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(36)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(37)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(42)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(38)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(39)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(40)`;
  `test/sql/spill.sql::tessera_test_spill_value`

#### Scenario: An expanded value copied flat
- **WHEN** an expanded array is copied, kept as a row's value, spilled
  in the rows of a grouping, or sent by a parallel worker, and its
  object is then freed
- **THEN** each copy reads back as the array's flat bytes, and both
  queries give the results they give without Tessera
- **Verified by:** `test/sql/rows.sql::tessera_test_rows_value`;
  `test/sql/agg.sql::An expanded array as a generic aggregate's argument`;
  `test/sql/parallel.sql::An expanded array computed in the workers`

#### Scenario: An expanded value refused by the append of columns
- **WHEN** `tess_spill_columns_append` is given an expanded value,
  read-only or read-write, after a plain one in the same row
- **THEN** it refuses the call and copies nothing
- **Verified by:**
  `crates/tessera-capi/tests/spill.rs::rows_of_datums_append_to_a_chunk_of_columns`

### Requirement: The writer and the reader of chunks of columns
`TessColumnsWriter` SHALL append a batch's rows to its chunk of
columns, their by-reference values copied into the chunk's own values,
until the chunk is full of rows or of values. A row whose values pass
an empty chunk's values SHALL grow them, and the writer SHALL keep them
grown. `tess_columns_writer_write` SHALL write the chunk as its pair
and start it anew, as `tess_columns_writer_reset` does without a write.
`TessColumnsReader` SHALL load a pair into buffers of its own and check
it whole (`tess_columns_reader_load`), or show a chunk that a writer of
the query filled in memory, unchecked (`tess_columns_reader_show`). It
SHALL give the rows by windows of Datums and one Datum at a time, a
value by reference as a pointer into the values, valid until the next
load or show.

#### Scenario: Rows through the writer and back through the reader
- **WHEN** rows of a column by value and of a varlena, with NULLs and
  one value larger than a chunk's values, are appended, written as
  pairs, loaded and read by windows
- **THEN** every row reads back as written, the large value first in a
  chunk of its own, and the values grown for it stay for the chunks
  after it
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_columns`

#### Scenario: A chunk with no values
- **WHEN** a writer writes a chunk whose rows have no value by
  reference, and a reader loads it
- **THEN** the chunk is written as its rows alone, with no block of
  values, and loads as a pair does, every word and NULL the one written
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_columns`

#### Scenario: A chunk shown from memory, and buffers handed over
- **WHEN** a writer's chunk is shown through a reader and the next pair
  loaded after it; and a loaded pair's buffers are handed over before
  the next load
- **THEN** the window reads the shown chunk where it lies, and the load
  reads into the reader's own buffers; the buffers handed over keep
  their values after the next load, and a reader freed holds none
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_columns`

### Requirement: A set writes one temporary file
A set of spilled blocks (`TessSpill`) SHALL write one temporary file,
made when the set first writes to disk, in which the blocks of every
partition follow one another, and SHALL keep for each partition the list
of its blocks. `tess_spill_write` SHALL return the bytes the block takes
in the file, the header and the stored body. After the blocks of a set
that wrote any, `tess_spill_finish` SHALL write the lists of the blocks:
a count of blocks for each partition (8 bytes each), then for each block
of each partition where it starts and the bytes it takes (8 bytes each),
then a trailer of four words of 8 bytes: a magic (the number
0x5445535354524149), where the lists start, the number of partitions and
the fingerprint. After `tess_spill_finish` the set only reads: a
partition's blocks MUST read back in the order they were written, and a
partition without blocks opens as no reader. A serial set's file is a
temporary file of PostgreSQL, made in the session's temporary
tablespaces: `temp_file_limit` applies to it, and it MUST be gone when
the set is freed and, after an ERROR, when the query's resources are
released.

```
 ┌────────┬─────┬─────────────────┬────────────────────┬─────────────┐
 │ blocks │  …  │ blocks in each  │ for each block of  │ trailer     │
 │        │     │ partition,      │ each partition:    │             │
 │        │     │ 8 bytes each    │ start, bytes taken │ 32 bytes    │
 └────────┴─────┴─────────────────┴────────────────────┴─────────────┘

 the trailer, words of 8 bytes
 ┌───────┬───────────────────────┬────────────┬─────────────┐
 │ magic │ where the lists start │ partitions │ fingerprint │
 └───────┴───────────────────────┴────────────┴─────────────┘
```

#### Scenario: Blocks of several partitions through one file
- **WHEN** blocks of records and values, among them an empty one and one
  of several write buffers, are written to two of four partitions of a
  serial set and read back
- **THEN** each partition gives its blocks in order with the bytes
  written, a partition without blocks gives no reader, and the counts of
  blocks, bytes and partitions with blocks match what was written
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_serial`;
  `crates/relspill/src/set.rs::a_set_written_in_memory_reads_back`

#### Scenario: Misuse of a set is an error
- **WHEN** a block is written after the finish, to a partition that does
  not exist, or with a body the header does not take or the set does not
  accept; the next header is read before the body, or a body at another
  length than its header's; another participant's file of a serial set
  or a second reader of a serial partition is opened; a reader seeks to
  a position that holds no block of its partition; a partition is
  dropped while a reader of it is open, or before its shared set is
  finished; a block of a kind that does not exist is written
- **THEN** each call raises an ERROR; the seek, the drops and the
  unknown kind are internal errors, SQLSTATE `XX000`
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_error`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(16)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(20)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(21)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(43)`

#### Scenario: The limit and the tablespace of temporary files
- **WHEN** a serial set, or a participant of a shared set, writes more
  bytes than `temp_file_limit` allows; and a serial set writes while
  `temp_tablespaces` names a tablespace
- **THEN** the write raises PostgreSQL's ERROR, and no temporary file
  and no directory of a shared set is left; the set's file is in that
  tablespace while the set holds it
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_bytes`;
  `test/sql/spill.sql::tessera_test_spill_shared_bytes`;
  `test/sql/spill.sql::pg_ls_dir`;
  `test/sql/spill.sql::tessera_test_spill_tablespace`

### Requirement: A shared set
In a shared set every participant SHALL write a file of its own in the
query's shared file set, named `<name>.<participant>`, which ends with
its lists and its trailer as every set's file does. Once a participant
has finished its set, any participant MUST be able to open that
participant's file and read a
partition's blocks from it, each reader at a position of its own. A file
without a valid trailer, with the fingerprint of another set than the
reader's, or whose lists do not fill exactly the bytes between the
blocks and the trailer, or name a block that does not lie among the
file's blocks, runs into the next block of its partition, is shorter
than a header or is longer than the longest block the set takes, MUST be
refused as damaged data. `temp_file_limit` applies to a participant's
file as to a serial set's. A participant that wrote no block has no
file, and its partitions open as no reader. A participant's file is
deleted when the participant frees its set, and the files that are left
when the last participant detaches from the shared memory.

A file set that is used again, by a node that runs anew in the same
query, MUST be reset first: `tess_spill_shared_reset` SHALL delete every
participant's file, so that nothing of the earlier use is read.

The format has no mark of a finished file. The caller ensures that a
participant's file is opened only after that participant finished its
set, and that the opener finished its own.

A reader of another participant's file SHALL take the bounds the
participants agreed on. Each gives the bounds of its finished set,
`tess_spill_bounds`: the longest chunk its `TessSpillConfig` names, and
the longest chunk of values it wrote. The caller gives the largest of
all to every participant's set through `tess_spill_agree`, and a set
MUST refuse to open another participant's file before that call. Bounds
below those of the blocks a participant wrote, and an agreement of a
serial set, MUST be refused as a misuse.

#### Scenario: A participant reads another's file
- **WHEN** two participants write blocks to a shared set and a third
  writes none, they finish, and each of the two opens the other's
  partitions, two readers on one partition among them
- **THEN** every reader gets that participant's blocks in order, the
  readers do not disturb each other, a partition of the participant
  without blocks opens as no reader, a partition its writer dropped
  opens as no reader for the writer and is still read by the other, and
  a participant that releases its set leaves its file readable by the
  other
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_shared`

#### Scenario: Bounds agreed by the participants
- **WHEN** participants of a shared set agree on the bounds of their
  blocks, and one of them wrote a partition of more blocks than a
  reader's page of entries and a chunk of values longer than any chunk;
  or a join that spills keeps values longer than any of its chunks
- **THEN** every participant reads the others' blocks back; a chunk of
  values longer than the agreed bound is damaged data; a file opened
  before the agreement, an agreement below a participant's own blocks
  and an agreement of a serial set are internal errors, SQLSTATE
  `XX000`
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_shared`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(44)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(45)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(46)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(47)`;
  `test/sql/join.sql::spec: spill-format/Bounds agreed by the participants`

#### Scenario: A file without its lists
- **WHEN** a participant's file ends without a trailer, the trailer's
  magic, counts or fingerprint do not match, a list counts more blocks
  than the lists hold, in the partition that is opened or in another, or
  a block's entry ends past the file's blocks, runs into the next block
  of its partition, is shorter than a header or is longer than the
  longest block the set takes; or the file is opened before its writer
  finished it
- **THEN** opening it fails as damaged data
- **Verified by:**
  `test/sql/spill.sql::tessera_test_spill_sqlstate(7)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(10)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(11)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(12)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(17)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(18)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(22)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(24)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(25)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(26)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(27)`

#### Scenario: A file set used again
- **WHEN** two participants write to a shared set and leave their files,
  the file set is reset, and in its second use one of them writes
  nothing
- **THEN** every partition of that participant opens as no reader, and
  the other's blocks of the second use read back
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_reset`

### Requirement: Reading a partition
`tess_spill_open` SHALL give a reader at a partition's first block with
a buffer as large as the partition's largest block, so that the memory
of a read is known before it starts. `tess_spill_read_header` SHALL read
the next block whole and MUST check its header against the set's
fingerprint and the bound of its kind, the partition, the level, the bytes on
disk and, for a packed body, the length that the body's own counts give;
it answers false at the partition's end. `tess_spill_read_body` SHALL
then give the body, unpacked when it was stored packed.
`tess_spill_seek` SHALL move a reader to a block by the position
`tess_spill_write` returned for it; any other position is a misuse of
the call. `tess_spill_drop` SHALL forget the caller's list of a
partition's blocks, which the caller then opens as no reader; the bytes
stay in the file until the set goes. A shared set SHALL refuse a drop
before it is finished, as its finish writes the lists the others read.
`tess_spill_stats` SHALL give the blocks and the bytes written, headers
included, and the partitions that have blocks. Freeing or releasing a
set closes its open readers, which the caller MUST NOT use afterwards.

#### Scenario: Read to the end, reopen, drop
- **WHEN** a partition is read to its end, opened again, and then dropped
- **THEN** the second reader starts at the first block, and after the
  drop the partition opens as no reader and is no longer counted among
  the partitions with blocks
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_serial`

#### Scenario: A reader goes to a block by its position
- **WHEN** a reader seeks to the position a write returned, in a serial
  set and in another participant's file of a shared set
- **THEN** the next header and body are that block's
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_serial`;
  `test/sql/spill.sql::tessera_test_spill_shared`

#### Scenario: A block that is not where the list says
- **WHEN** a block's header names another partition or level than the
  reader's, its lengths do not match the bytes on disk, or its body
  length is not what its packed body unpacks into
- **THEN** the read fails as damaged data
- **Verified by:**
  `test/sql/spill.sql::tessera_test_spill_sqlstate(13)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(14)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(15)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(19)`

### Requirement: The memory of a set
`tess_spill_memory` SHALL give the bytes of the buffers a set holds now:
its write buffer until the set is finished, the buffer through which a
chunk longer than the write buffer is packed, and the buffer of each
open reader, which is as large as the largest block of its partition.

#### Scenario: The buffers of a set through its life
- **WHEN** a chunk of records longer than the write buffer is written to
  a new set, the set is finished, and a reader of the chunk's partition
  is opened and closed
- **THEN** the memory grows by at least the chunk when it is written,
  falls by the write buffer at the finish, grows by the stored block
  while the reader is open and falls back when it is closed; the chunk
  reads back as it was written
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_memory`

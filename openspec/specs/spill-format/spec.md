# spill-format Specification

## Purpose
The format of the blocks that the hash nodes and the external sort write
to temporary files, the checks a reader applies before it trusts a
block, and the sets of files that hold the blocks. When a node spills
and what it keeps in memory belong to the node's own capability.

A chunk is a piece of a node's memory that is written whole: records of
a hash table, the values they refer to, or rows kept by column. A block
is a chunk on disk: a header, then the stored body. A set is the blocks
a node writes into one file, with a list of the blocks of each of its
partitions. For a join and a grouping a partition is the rows of some
bits of the hash, and a set belongs to one level of partitioning; for
the sort a partition is one sorted run. The requirements go from the
smallest piece to the largest: the header of a block, the three kinds of
body and their packed forms, the file of a set, a shared set, reading a
partition. [design.md](design.md) explains the whole and the reasons.

## Requirements

### Requirement: Block header layout
Every spilled block SHALL begin with a header of 48 bytes in the
machine's byte order, followed by the stored body.
`tess_spill_header_size()` SHALL return 48.

```
 byte    0           4           8           12          16
         ┌───────────────────────┬───────────┬───────────┐
       0 │ magic                 │ version   │ kind      │
         ├───────────┬───────────┼───────────┼───────────┤
      16 │ number    │ partition │ level     │ packed    │
         ├───────────┴───────────┼───────────┴───────────┤
      32 │ fingerprint           │ body length           │
         └───────────────────────┴───────────────────────┘
      48   the stored body
```

- magic: a number of 64 bits whose bytes are `TESSSPIL` on a
  little-endian machine;
- version: the version of the format, 2;
- kind: 1 records, 2 values, 3 columns;
- number: a number the writer gives the chunk; a join finds a chunk
  of values by it;
- partition, and level: the level of partitioning, 0 for the first;
- packed: the packed length, the bytes of the stored body when it is
  stored packed, 0 when it is stored as it is;
- fingerprint: the number the set was made with, which a reader
  compares with its own: of a hash table's layout, or of the width of
  a row;
- body length: the bytes of the chunk a reader gets; for kind 3 this
  is a chunk of its rows only.

The header carries no checksum: a temporary file is read by the query
that wrote it, on the same machine.

#### Scenario: A header reads back as written
- **WHEN** a header of a chunk of records or of values is written and
  then read with the same fingerprint and length limit
- **THEN** every field is returned unchanged
- **Verified by:**
  `crates/tessera-spill/src/lib.rs::a_header_reads_back_as_written`

#### Scenario: The size of a header through the C API
- **WHEN** a module calls `tess_spill_header_size()` and writes and
  reads a header through `tess_spill_header_write` and
  `tess_spill_header_read`
- **THEN** the size is 48 and the fields read back as written
- **Verified by:** `test/sql/table.sql::tessera_test_spill_header`

### Requirement: Kinds of block and their lengths
A block SHALL hold one of three bodies, and a writer MUST refuse a
header that its own reader would reject.

- Kind 1, records: a chunk of the hash table's records with its 8-byte
  used mark first. The body length is a multiple of 8 and at least 8.
  The packed length is 0, or a multiple of 8, at least 16 and less than
  the body length.
- Kind 2, values: a chunk of the by-reference values that records or
  columns refer to. The body length is a multiple of 8. The packed
  length is 0.
- Kind 3, columns: a chunk of rows by column. The body length is a
  multiple of 8 and at least 16. The packed length is a multiple of 8
  and at least 8: a chunk of columns is always stored packed.

For every kind the level is below 32 and the body length is at most the
limit the reader accepts.

#### Scenario: A header that would not read back is not written
- **WHEN** a writer is given a records header with a body length of 0, a
  body longer than the limit, or a values header with a packed length
- **THEN** the write fails and no header is produced
- **Verified by:**
  `crates/tessera-spill/src/lib.rs::a_header_that_would_not_read_back_is_not_written`

#### Scenario: The lengths of a columns header
- **WHEN** a header of kind 3 has a body shorter than 16 bytes, or a
  packed length that is 0 or not a multiple of 8
- **THEN** it is refused on writing and on reading
- **Verified by:**
  `crates/tessera-spill/src/lib.rs::a_columns_header_keeps_its_lengths`

#### Scenario: The level and the limit for every kind
- **WHEN** a header of records, of values or of columns has a level of
  32, or a body longer than the limit
- **THEN** it is refused on writing and on reading, while a header at
  level 31 with a body of exactly the limit is accepted
- **Verified by:**
  `crates/tessera-spill/src/lib.rs::the_level_and_the_limit_hold_for_every_kind`

### Requirement: Damaged and foreign blocks are refused
A reader MUST check the magic, the version, the kind, the fingerprint,
the body length, the packed length and the level before it uses a block,
and MUST NOT interpret the body of a block that failed a check; a packed
body MUST be checked as it is unpacked. A failed check SHALL be reported
as damaged data: status `TESS_ERROR_DATA_CORRUPTED` with SQLSTATE
`XX001`. A misuse of a call, such as a buffer shorter than a header,
SHALL be reported as an internal error, SQLSTATE `XX000`, not as damaged
data.

#### Scenario: A damaged field of a header, or another table's block
- **WHEN** one field of a valid header is overwritten with an invalid
  value (the magic, the version, the kind, the packed length, the
  fingerprint, the body length or the level), or a valid block is read
  with the fingerprint of another table
- **THEN** reading the header fails as damaged data, and a query that
  reads such a block raises an ERROR
- **Verified by:**
  `crates/tessera-spill/src/lib.rs::every_damaged_field_is_refused`;
  `test/sql/spill.sql::tessera_test_spill_error(7)`

#### Scenario: A damaged packed body of records
- **WHEN** a packed body is too short for its counts or its lane codes,
  does not match the chunk it is unpacked into, has a lane code that
  does not exist, stores the lane of the next-record references, has
  records of another length than its own, or has bytes past its lanes
- **THEN** unpacking fails as damaged data
- **Verified by:**
  `crates/tessera-spill/src/pack.rs::damaged_packed_bodies_are_refused`

#### Scenario: The status and SQLSTATE of damage and of misuse
- **WHEN** the C entry points read a damaged header or unpack a damaged
  body, and when they are given a buffer shorter than a header
- **THEN** the first report `XX001` and the second `XX000`
- **Verified by:**
  `crates/tessera-capi/tests/spill.rs::damaged_blocks_report_data_corrupted`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(5)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(7)`

### Requirement: A packed chunk of records
A chunk of records SHALL be stored packed only when the packed form is
shorter than the chunk; otherwise it is stored as it is, with a packed
length of 0. Reading a packed chunk back MUST give the chunk byte for
byte, except that every next-record reference is 0, as in a chunk not
yet linked.

The packed body sees the records as lanes of 4 bytes, the same lane of
every record together: the number of records (4 bytes), the length of a
record (4 bytes), one code byte for each lane padded to a multiple of 4,
then the lanes' data, padded with zeros to a multiple of 8.

```
 ┌─────────┬─────────┬──────────────────┬──────────────────────────┐
 │ records │ record  │ a code for each  │ the lanes' data, a lane  │
 │         │ length  │ lane, 1 byte     │ after a lane             │
 │ 4 bytes │ 4 bytes │ padded to 4      │ padded to 8              │
 └─────────┴─────────┴──────────────────┴──────────────────────────┘
```

A lane's code says how its values are stored. A writer gives a lane the
first code of this list that fits its values:

- 0: every value is 0, and nothing is stored; the next-record lane
  always has this code, and a body that gives it another is refused as
  damaged data;
- 1: all values are equal, and one value of 4 bytes is stored;
- 2: every value is below 256, and 1 byte is stored for a record;
- 3: every value is below 65536, and 2 bytes are stored for a record;
- 4: any other lane, and 4 bytes are stored for a record.

A packed body whose records do not have its record length, or that does
not end with its last lane, padded to a multiple of 8, MUST be refused
as damaged data.

#### Scenario: Lanes take the width their values need and read back
- **WHEN** a chunk of records with lanes of zeros, equal values, and
  values of one, two and four bytes is packed and unpacked
- **THEN** each lane is stored at its width and the chunk reads back the
  same but for the next-record references
- **Verified by:**
  `crates/tessera-spill/src/pack.rs::widths_follow_the_values_of_each_lane`

#### Scenario: A chunk that does not get shorter stays as it is
- **WHEN** the bytes are not a chunk of records, the used mark does not
  match, the records differ in length, or packing would not shorten them
- **THEN** packing is refused and the block is stored as it is
- **Verified by:**
  `crates/tessera-spill/src/pack.rs::what_is_no_chunk_of_records_stays_as_it_is`

#### Scenario: Packed and plain blocks in a file
- **WHEN** a chunk of 1000 records that packs and bytes that are no
  chunk of records are written to a set and read back
- **THEN** the first takes its header and packed body in the file, the
  second its header and whole body, and both read back as written but
  for the next-record references
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_packed`

### Requirement: A chunk of columns in memory
A chunk of columns SHALL begin with a header of 16 bytes: the rows it
holds (4 bytes), its capacity in rows (4 bytes), the words stored for a
row (4 bytes) and the magic `COLS` (4 bytes). Lanes of `capacity` words
of 8 bytes follow: first the lanes of NULL bits, one for every 64 stored
words and at least one, where bit `w % 64` of lane `w / 64` of a row is
set when the row's word `w` is NULL; then one lane for each stored word.
A row's word is a by-value Datum, a reference to a by-reference value,
or 0 for a NULL. The inline accessors of `tessera/spill.h` and the Rust
kernels MUST agree on this layout.

```
 byte 0      4          8             12       16
      ┌──────┬──────────┬─────────────┬────────┐
      │ rows │ capacity │ words a row │ "COLS" │
      ├──────┴──────────┴─────────────┴────────┤
   16 │ lanes of NULL bits, capacity × 8 bytes │
      │ each: one for every 64 words a row,    │
      │ at least one                           │
      ├────────────────────────────────────────┤
      │ lanes of words, capacity × 8 bytes     │
      │ each: one for each word of a row       │
      └────────────────────────────────────────┘
```

#### Scenario: A lane of NULL bits for every 64 words
- **WHEN** the lanes of NULL bits are counted for 0, 64, 65 and 130
  words, and a chunk of 130 words a row is filled, packed and read back
- **THEN** there are 1, 1, 2 and 3 lanes, the words' lanes start after
  them, and every lane reads back
- **Verified by:**
  `crates/tessera-spill/src/columns.rs::wide_chunks_take_a_lane_of_null_bits_per_64_words`

#### Scenario: The C formulas match the kernels'
- **WHEN** the C macros and the kernels' entry points compute the lanes
  of NULL bits and the size of a chunk at every count of words up to
  4096
- **THEN** they agree, word `w` has its NULL bit at bit `w % 64` of lane
  `w / 64`, and an unknown question to `tess_spill_columns_shape`
  answers 0
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_lanes`;
  `crates/tessera-capi/tests/table.rs::chunks_of_columns_round_trip_through_the_entry_points`

#### Scenario: Rows appended to a chunk
- **WHEN** rows with NULLs, by-value words and by-reference values in up
  to 69 columns are appended to a chunk
- **THEN** each row's words lie in their lanes at the row's place, a
  NULL has its bit set and a word of 0, and a by-reference word is the
  place of its bytes
- **Verified by:**
  `crates/tessera-kernels/src/spill_columns.rs::rows_go_in_order_until_the_chunk_or_the_values_fill`

### Requirement: The limits of a chunk of columns
A chunk of columns SHALL hold at most 4096 words a row and at most
131071 rows, and no more rows than pack into the 4 GiB that the 32 bits
of a header's packed length can name. A chunk MUST NOT be made in bytes
that are too short for the header or of a length that is no multiple of
8; a chunk of such a length MUST NOT be packed; and bytes without the
magic MUST be refused as a chunk of columns.

#### Scenario: A chunk that is not one of columns
- **WHEN** a chunk of 8 bytes is initialized, or a chunk whose magic is
  not `COLS` is packed
- **THEN** the call is refused
- **Verified by:**
  `crates/tessera-spill/src/columns.rs::a_damaged_or_foreign_chunk_is_refused`

#### Scenario: The limits of a chunk
- **WHEN** a chunk is initialized with more than 4096 words a row or
  with a length that is no multiple of 8, its length would hold more
  than 131071 rows or more than pack into 4 GiB, or a chunk with a byte
  past a length of 8s is packed
- **THEN** the first two are refused, the capacity stops at 131071 or
  at the rows that pack into 4 GiB, and the packing is refused
- **Verified by:**
  `crates/tessera-spill/src/columns.rs::a_chunk_keeps_within_its_limits`

### Requirement: A chunk of columns on disk
A chunk of columns SHALL be stored packed, for its rows only: the rows
(4 bytes), the words a row (4 bytes), a descriptor of 16 bytes for each
lane (the width in its first byte, the lane's least value in its last
8), then each lane's values as their difference from the least value in
0, 1, 2, 4 or 8 bytes each, the fewest that hold the lane's largest
difference, a lane padded to a multiple of 8. The words of a lane are
compared as signed numbers of 64 bits, and a difference is taken modulo
2^64. A lane whose values are all equal has a width of 0 and takes no
bytes beyond its descriptor; in a chunk without rows every lane has a
width of 0 and a least value of 0. The lanes of NULL bits are stored as
the lanes of words are. Reading
it back MUST give a chunk with the same rows, words and values, whose
capacity is its rows. A packed chunk with a width that does not exist, a
lane cut short, a length that does not match its counts, or bytes past
its lanes MUST be refused as damaged data.

```
 ┌─────────┬─────────────┬─────────────────────┬─────────────────────┐
 │ rows    │ words a row │ a descriptor for    │ each lane's values  │
 │         │             │ each lane, 16 bytes │ less its least one, │
 │ 4 bytes │ 4 bytes     │                     │ padded to 8         │
 └─────────┴─────────────┴─────────────────────┴─────────────────────┘

 a descriptor
 byte 0       1                    8                   16
      ┌───────┬────────────────────┬───────────────────┐
      │ width │ zeros              │ least value       │
      └───────┴────────────────────┴───────────────────┘
```

#### Scenario: Lanes pack at the width of their span
- **WHEN** a chunk with lanes of equal values and of spans that need 1,
  2, 4 and 8 bytes is packed and unpacked, and a chunk whose lane holds
  -1 and 0 is packed
- **THEN** the packed length is the sum of those widths and the chunk
  reads back; the lane of -1 and 0 has a least value of -1, a width of 1
  byte and the bytes 0 and 1
- **Verified by:**
  `crates/tessera-spill/src/columns.rs::lanes_pack_at_the_width_of_their_span_and_read_back`;
  `crates/tessera-spill/src/columns.rs::a_lane_is_stored_from_its_least_signed_value`

#### Scenario: Empty and full chunks
- **WHEN** a chunk with no rows and a chunk filled to its capacity are
  packed and unpacked
- **THEN** both read back as they were
- **Verified by:**
  `crates/tessera-spill/src/columns.rs::empty_and_full_chunks_round_trip`

#### Scenario: A damaged packed chunk of columns
- **WHEN** a packed chunk of columns is cut short, has a wrong length
  for its counts or a width of 3
- **THEN** unpacking fails as damaged data
- **Verified by:**
  `crates/tessera-spill/src/columns.rs::a_damaged_or_foreign_chunk_is_refused`

### Requirement: A set writes one temporary file
A set of spilled blocks (`TessSpill`) SHALL write one temporary file,
made with its first block, in which the blocks of every partition follow
one another, and SHALL keep for each partition the list of its blocks.
`tess_spill_write` SHALL return the bytes the block takes in the file,
the header and the stored body. After `tess_spill_finish` the set only
reads: a partition's blocks MUST read back in the order they were
written, and a partition without blocks opens as no reader. A serial
set's file is a temporary file of PostgreSQL: `temp_file_limit` applies
to it, and it MUST be gone when the set is freed and, after an ERROR,
when the query's resources are released.

#### Scenario: Blocks of several partitions through one file
- **WHEN** blocks of records and values, among them an empty one and one
  of several write buffers, are written to two of four partitions of a
  serial set and read back
- **THEN** each partition gives its blocks in order with the bytes
  written, a partition without blocks gives no reader, and the counts of
  blocks, bytes and partitions with blocks match what was written
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_serial`

#### Scenario: Misuse of a set is an error
- **WHEN** a block is written after the finish, to a partition that does
  not exist, or with a body the header does not take or the set does not
  accept; the next header is read before the body, or a body at another
  length than its header's; another participant's file of a serial set
  or a second reader of a serial partition is opened; a reader seeks to
  a position that holds no block of its partition
- **THEN** each call raises an ERROR, and the seek an internal error,
  SQLSTATE `XX000`
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_error`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(16)`

#### Scenario: The limit of temporary files stops a spill
- **WHEN** a set writes more bytes than `temp_file_limit` allows
- **THEN** the write raises PostgreSQL's ERROR and no temporary file is left
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_bytes`;
  `test/sql/spill.sql::pg_ls_tmpdir`

### Requirement: A shared set
In a shared set every participant SHALL write a file of its own in the
query's shared file set, named `<name>.<participant>`.
`tess_spill_finish` SHALL write after the blocks the lists of the
blocks: a count of blocks for each partition (8 bytes each), then for
each block of each partition where it starts and the bytes it takes (8
bytes each), then a trailer of four words of 8 bytes: a magic (the
number 0x5445535354524149), where the lists start, the number of
partitions and the fingerprint. Once a participant has finished its set,
any participant MUST be able to open that participant's file and read a
partition's blocks from it, each reader at a position of its own. A file
without a valid trailer, with the fingerprint of another set than the
reader's, or whose lists name more blocks than they hold or a block that
does not lie among the file's blocks, MUST be refused as damaged data. A
participant that wrote no block has no file, and its partitions open as
no reader. A participant's file is deleted when the participant frees
its set, and the files that are left when the last participant detaches
from the shared memory.

The format has no mark of a finished file. The caller ensures that a
participant's file is opened only after that participant finished its
set, and that the opener finished its own.

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

#### Scenario: A participant reads another's file
- **WHEN** two participants write blocks to a shared set and a third
  writes none, they finish, and each of the two opens the other's
  partitions, two readers on one partition among them
- **THEN** every reader gets that participant's blocks in order, the
  readers do not disturb each other, a partition of the participant
  without blocks opens as no reader, and a participant that releases
  its set leaves its file readable by the other
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_shared`

#### Scenario: A file without its lists
- **WHEN** a participant's file ends without a trailer, the trailer's
  magic, counts or fingerprint do not match, a list counts more blocks
  than the lists hold, or a block's entry ends past the file's blocks
- **THEN** opening it fails as damaged data
- **Verified by:**
  `test/sql/spill.sql::tessera_test_spill_sqlstate(7)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(10)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(11)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(12)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(17)`;
  `test/sql/spill.sql::tessera_test_spill_sqlstate(18)`

### Requirement: Reading a partition
`tess_spill_open` SHALL give a reader at a partition's first block with
a buffer as large as the partition's largest block, so that the memory
of a read is known before it starts. `tess_spill_read_header` SHALL read
the next block whole and MUST check its header against the set's
fingerprint and longest body, the partition, the level, the bytes on
disk and, for a packed body, the length that the body's own counts give;
it answers false at the partition's end. `tess_spill_read_body` SHALL
then give the body, unpacked when it was stored packed.
`tess_spill_seek` SHALL move a reader to a block by the position
`tess_spill_write` returned for it; any other position is a misuse of
the call. `tess_spill_drop` SHALL forget the caller's list of a
partition's blocks, which the caller then opens as no reader; the bytes
stay in the file until the set goes. `tess_spill_stats` SHALL give the
blocks and the bytes written, headers included, and the partitions that
have blocks. Freeing or releasing a set closes its open readers, which
the caller MUST NOT use afterwards.

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

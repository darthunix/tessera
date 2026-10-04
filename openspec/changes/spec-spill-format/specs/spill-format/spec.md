## Purpose

The format of the blocks that the hash nodes and the external sort write
to temporary files, the checks a reader applies before it trusts a
block, and the sets of files that hold the blocks. When a node spills
and what it keeps in memory belong to the node's own capability.

## ADDED Requirements

### Requirement: Block header layout
Every spilled block SHALL begin with a header of 48 bytes in the
machine's byte order, followed by the stored body.
`tess_spill_header_size()` SHALL return 48.

- bytes 0 to 7: the magic, the bytes `TESSSPIL`;
- bytes 8 to 11: the format version, 2;
- bytes 12 to 15: the kind: 1 records, 2 values, 3 columns;
- bytes 16 to 19: the number of the chunk in its table;
- bytes 20 to 23: the partition;
- bytes 24 to 27: the level of partitioning, 0 for the first;
- bytes 28 to 31: the packed length: the bytes of the stored body when
  it is stored packed, 0 when it is stored as it is;
- bytes 32 to 39: the fingerprint of the table's layout;
- bytes 40 to 47: the body length: the bytes of the chunk in memory.

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
- **Verified by:** review only — no test writes or reads a header of
  kind 3 on its own; the sort and join suites pass through the rule

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
  does not match the chunk it is unpacked into, or has a lane code that
  does not exist
- **THEN** unpacking fails as damaged data
- **Verified by:**
  `crates/tessera-spill/src/pack.rs::damaged_packed_bodies_are_refused`

#### Scenario: The status and SQLSTATE of damage and of misuse
- **WHEN** the C entry points read a damaged header or unpack a damaged
  body, and when they are given a buffer shorter than a header
- **THEN** the first report `XX001` and the second `XX000`
- **Verified by:**
  `crates/tessera-capi/tests/spill.rs::damaged_blocks_report_data_corrupted`

### Requirement: A packed chunk of records
A chunk of records SHALL be stored packed only when the packed form is
shorter than the chunk; otherwise it is stored as it is, with a packed
length of 0. Reading a packed chunk back MUST give the chunk byte for
byte, except that every next-record reference is 0, as in a chunk not
yet linked.

The packed body sees the records as lanes of 4 bytes, the same lane of
every record together: the number of records (4 bytes), the length of a
record (4 bytes), one code byte for each lane padded to a multiple of 4,
then the lanes' data, padded with zeros to a multiple of 8. A lane's
code says how its values are stored:

- 0: nothing is stored, since every value is 0; the next-record lane
  always has this code;
- 1: one value of 4 bytes, since all are equal;
- 2: 1 byte a record;
- 3: 2 bytes a record;
- 4: 4 bytes a record.

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
131071 rows. Bytes that are too short for the header, of a length that
is no multiple of 8, or without the magic MUST be refused as a chunk of
columns.

#### Scenario: A chunk that is not one of columns
- **WHEN** a chunk of 8 bytes is initialized, or a chunk whose magic is
  not `COLS` is packed
- **THEN** the call is refused
- **Verified by:**
  `crates/tessera-spill/src/columns.rs::a_damaged_or_foreign_chunk_is_refused`

#### Scenario: The limits of a chunk
- **WHEN** a chunk is initialized with more than 4096 words a row or
  with a length that is no multiple of 8, or its length would hold more
  than 131071 rows
- **THEN** the first two are refused and the capacity stops at 131071
- **Verified by:** review only — no test reaches the limits; they are
  `MAX_WORDS` and `MAX_ROWS` of `crates/tessera-spill/src/columns.rs`

### Requirement: A chunk of columns on disk
A chunk of columns SHALL be stored packed, for its rows only: the rows
(4 bytes), the words a row (4 bytes), a descriptor of 16 bytes for each
lane (the width in its first byte, the lane's least value in its last
8), then each lane's values as their difference from the least value in
0, 1, 2, 4 or 8 bytes each, a lane padded to a multiple of 8. A lane
whose values are all equal takes no bytes beyond its descriptor. Reading
it back MUST give a chunk with the same rows, words and values, whose
capacity is its rows. A packed chunk with a width that does not exist, a
lane cut short, a length that does not match its counts, or bytes past
its lanes MUST be refused as damaged data.

#### Scenario: Lanes pack at the width of their span
- **WHEN** a chunk with lanes of equal values and of spans that need 1,
  2, 4 and 8 bytes is packed and unpacked
- **THEN** the packed length is the sum of those widths and the chunk reads back
- **Verified by:**
  `crates/tessera-spill/src/columns.rs::lanes_pack_at_the_width_of_their_span_and_read_back`

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
  or a second reader of a serial partition is opened
- **THEN** each call raises an ERROR
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_error`

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
bytes each), then a trailer of four words of 8 bytes: a magic, where the
lists start, the number of partitions and the fingerprint. Once a
participant has finished its set, any participant MUST be able to open
that participant's file and read a partition's blocks from it, each
reader at a position of its own. A file without a valid trailer MUST be
refused as damaged data. The files are deleted when the last participant
detaches from the shared memory.

#### Scenario: A participant reads another's file
- **WHEN** two participants write blocks to a shared set, finish, and
  each opens the other's partitions, two readers on one partition among
  them
- **THEN** every reader gets that participant's blocks in order, the
  readers do not disturb each other, and releasing a set leaves the
  other's file readable
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_shared`

#### Scenario: A file without its lists
- **WHEN** a participant's file ends without a trailer, or the trailer's
  magic or counts do not match
- **THEN** opening it fails as damaged data
- **Verified by:** review only — no test damages a shared file; the
  check is `read_trailer` of `runtime/spill.c`

### Requirement: Reading a partition
`tess_spill_open` SHALL give a reader at a partition's first block with
a buffer as large as the partition's largest block, so that the memory
of a read is known before it starts. `tess_spill_read_header` SHALL read
the next block whole and MUST check its header against the set's
fingerprint and longest body, the partition, the level and the bytes on
disk; it answers false at the partition's end. `tess_spill_read_body`
SHALL then give the body, unpacked when it was stored packed.
`tess_spill_seek` SHALL move a reader to a block by the position
`tess_spill_write` returned for it. `tess_spill_drop` SHALL forget a
partition's blocks, which then opens as no reader; the bytes stay in the
file until the set goes. `tess_spill_stats` SHALL give the blocks and
the bytes written, headers included, and the partitions that have
blocks.

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
  reader's, its lengths do not match the bytes on disk, or a seek names
  a position that holds no block of the partition
- **THEN** the read fails as damaged data
- **Verified by:** review only — no test damages the bytes of a file;
  the checks are in `tess_spill_read_header` and `tess_spill_seek` of
  `runtime/spill.c`

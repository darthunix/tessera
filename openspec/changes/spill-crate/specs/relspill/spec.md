## ADDED Requirements

### Requirement: A crate of its own
The spill SHALL be the Rust crate `relspill`, which depends on no crate
of Tessera except `relhash`. It SHALL build, pass its tests and be
packaged with `relhash` alone, on targets with 64-bit pointers; on any
other target it SHALL fail to build, with an error that says why. Its
documentation SHALL be complete by itself: every public item SHALL say
what it is for, what it takes and gives, who owns what and for how
long, and the errors it returns, and the documentation SHALL hold the
rules, the layouts and the limits of the API.

#### Scenario: The crate packaged alone
- **WHEN** the crate is packaged with `relhash`, as for publishing
- **THEN** it builds from the packaged files and the crates they name,
  and `relhash` is the only one of them from Tessera's workspace
- **Verified by:**
  `.github/workflows/ci.yml::cargo publish --dry-run --locked -p relhash
  -p relspill`

#### Scenario: A 32-bit target
- **WHEN** the crate is built for a target with 32-bit pointers
- **THEN** the build fails with an error that says that 64-bit pointers
  are needed
- **Verified by:**
  `.github/workflows/ci.yml::needs a target whose pointers are 64 bits wide`

#### Scenario: Records packed as relhash lays them out
- **WHEN** the crate packs and unpacks chunks of records that a
  `relhash` table wrote
- **THEN** each record reads back equal to the table's, field by field,
  except the reference of the next record, which comes back as zero,
  and another table links the chunk read back and finds every row
- **Verified by:**
  `crates/relspill/tests/records.rs::a_chunk_a_table_wrote_comes_back_and_links_again`

#### Scenario: Documentation built with warnings as errors
- **WHEN** the documentation of the crate is built with every warning
  an error, and with a lint that refuses a public item without a text
- **THEN** it builds
- **Verified by:**
  `.github/workflows/ci.yml::RUSTDOCFLAGS="-D warnings" cargo doc
  --locked -p relspill --no-deps`;
  `crates/relspill/src/lib.rs::#![deny(missing_docs)]`

#### Scenario: The examples run
- **WHEN** the tests of the documentation run
- **THEN** every example passes
- **Verified by:**
  `.github/workflows/ci.yml::cargo test --locked -p relspill --doc`

### Requirement: No input or output
The crate SHALL NOT open, read, write or remove a file. A call that
writes SHALL return the bytes to write and the offset of each in the
file. A call that reads SHALL name the offset and the length it needs,
and SHALL take the bytes the caller read. When the caller reports an
action it could not finish, the set or the machine SHALL accept only a
free or a reset afterwards, and SHALL refuse any other call as a wrong
call.

#### Scenario: A set written and read in memory
- **WHEN** a caller keeps the file of a set as bytes in memory, writes
  the ranges the crate returns, and reads the ranges it asks for
- **THEN** every block of every partition reads back as it was written
- **Verified by:**
  `crates/relspill/src/set/reader.rs::a_set_reads_back_by_its_lists_and_from_its_file`

#### Scenario: A write that fails half way
- **WHEN** the caller reports a write that it finished only in part
- **THEN** the set refuses every call but a free or a reset, and a set
  that was reset writes and reads like a new one
- **Verified by:**
  `crates/relspill/src/set.rs::a_set_cut_by_a_failed_write_takes_only_a_reset`

### Requirement: No memory allocated
A call of the crate SHALL allocate no memory, even when it fails. Every
region a call uses SHALL come from its caller. A call that cannot start
without a larger region SHALL return how many bytes it needs and change
nothing. A call that appends rows MAY first append the rows that fit;
it SHALL then say which rows it appended and how many bytes the next
row needs.

#### Scenario: Every call under a counting allocator
- **WHEN** every call of the crate runs under an allocator that counts
  allocations
- **THEN** the count does not change
- **Verified by:** pending

#### Scenario: A full block list
- **WHEN** a block is written to a partition whose block list has no
  room left
- **THEN** the call returns the size the list needs, and the set is
  unchanged
- **Verified by:**
  `crates/relspill/src/set.rs::a_full_block_list_asks_for_room`

#### Scenario: Rows that fit in part
- **WHEN** two rows are appended to a chunk of columns whose region of
  values holds only the first
- **THEN** the first row is appended, the call says so, and it returns
  the bytes the second row needs
- **Verified by:**
  `crates/relspill/src/spill_columns.rs::rows_go_in_order_until_the_chunk_or_the_values_fill`;
  `crates/tessera-capi/tests/spill.rs::rows_of_datums_append_to_a_chunk_of_columns`

### Requirement: The header of a block
Every block SHALL begin with the header that
[`BlockHeader`](../../../crates/relspill/src/lib.rs) draws, of
[`HEADER_SIZE`](../../../crates/relspill/src/lib.rs) bytes, and then
its stored body: a chunk of a
[`BlockKind`](../../../crates/relspill/src/lib.rs), of the lengths its
kind allows. A writer SHALL refuse a header that a reader would refuse,
and a reader SHALL check every field of a header before it returns it.

#### Scenario: A header reads back as written
- **WHEN** a header of a chunk of records or of values is written and
  then read with the same fingerprint and bounds
- **THEN** every field is returned unchanged
- **Verified by:**
  `crates/relspill/src/lib.rs::a_header_reads_back_as_written`

#### Scenario: A header that would not read back is not written
- **WHEN** a writer is given a header of records with a body length of
  0, a body past its bound, or a header of values with a packed length
- **THEN** the write fails and no header is written
- **Verified by:**
  `crates/relspill/src/lib.rs::a_header_that_would_not_read_back_is_not_written`

#### Scenario: The lengths and the level of each kind
- **WHEN** a header of columns has a body shorter than 16 bytes, or a
  packed length of 0 or not a multiple of 8; and a header of any kind
  has a level of 32, or a body past the bound of its kind
- **THEN** it is refused on writing and on reading, while a header at
  level 31 with a body of exactly its bound is taken
- **Verified by:**
  `crates/relspill/src/lib.rs::a_columns_header_keeps_its_lengths`;
  `crates/relspill/src/lib.rs::the_level_and_the_limit_hold_for_every_kind`

#### Scenario: A damaged field, or a block of another set
- **WHEN** one field of a header is damaged: the magic, the version, the
  kind, the packed length, the fingerprint, the body length or the
  level; or a block is read with the fingerprint of another set
- **THEN** the read fails as damaged bytes, by the rule of that field
- **Verified by:**
  `crates/relspill/src/lib.rs::every_damaged_field_is_refused`

### Requirement: Bounds of a block by its kind
A writer and a reader of a set SHALL take a bound for each kind of
block, a [`Bounds`](../../../crates/relspill/src/lib.rs): one for a
chunk of records or of columns, by the plan of the node's level, and
one for a chunk of values, by the longest one the set's writers wrote.
A writer SHALL refuse a block past the bound of its kind as a wrong
call, and SHALL give the bounds a reader of its set takes,
[`Writer::read_bounds`](../../../crates/relspill/src/set.rs). A reader
SHALL refuse a block past the bound of its kind as damaged bytes.

#### Scenario: A block past the bound of its kind
- **WHEN** a block of values or a chunk is longer than the bound of its
  kind, though not than the other kind's
- **THEN** a writer refuses it as a wrong call and a reader as damaged
  bytes; a writer's bound for values is the longest it wrote
- **Verified by:**
  `crates/relspill/src/set.rs::a_block_past_the_bound_of_its_kind_is_refused`;
  `crates/relspill/src/set/reader.rs::a_block_past_the_bound_of_its_kind_is_damaged`;
  `crates/relspill/src/lib.rs::each_kind_of_block_takes_its_own_bound`

#### Scenario: Values longer than a chunk
- **WHEN** a join that spills keeps values longer than any of its
  chunks
- **THEN** each goes to disk in a chunk of values of its own, and reads
  back by the bound its set gives
- **Verified by:**
  `test/sql/join.sql::spec: relspill/Values longer than a chunk`

### Requirement: A packed chunk of records
A chunk of records SHALL be stored packed, in the body that
[`pack`](../../../crates/relspill/src/pack.rs) draws, only when that
body is shorter than the chunk, and as it is otherwise.
[`unpack`](../../../crates/relspill/src/pack.rs) SHALL give the chunk
back byte for byte, except that every next-record reference is 0, as
in a chunk not yet linked. It SHALL check the body as it unpacks it,
and refuse as damaged bytes one whose counts, codes and lanes do not
fill the chunk and the body exactly, as its documentation lists.

#### Scenario: Lanes take the width their values need and read back
- **WHEN** a chunk of records with lanes of zeros, equal values, and
  values of one, two and four bytes is packed and unpacked
- **THEN** each lane is stored at its width, and the chunk reads back
  the same but for the next-record references
- **Verified by:**
  `crates/relspill/src/pack.rs::widths_follow_the_values_of_each_lane`

#### Scenario: A chunk that does not get shorter stays as it is
- **WHEN** the bytes are not a chunk of records, the used mark does not
  match, the records differ in length, or packing would not shorten them
- **THEN** packing is refused, and the block is stored as it is
- **Verified by:**
  `crates/relspill/src/pack.rs::what_is_no_chunk_of_records_stays_as_it_is`

#### Scenario: A damaged packed body of records
- **WHEN** a packed body is too short for its counts or its lane codes,
  does not match the chunk it is unpacked into, has a lane code that
  does not exist, stores the lane of the next-record references, has
  records of another length than its own, or has bytes past its lanes
- **THEN** unpacking fails as damaged bytes
- **Verified by:**
  `crates/relspill/src/pack.rs::damaged_packed_bodies_are_refused`

### Requirement: A chunk of columns
A chunk of rows by column SHALL have the layout and the limits that
[`columns`](../../../crates/relspill/src/columns.rs) draws, of at most
[`MAX_WORDS`](../../../crates/relspill/src/columns.rs) words a row and
[`MAX_ROWS`](../../../crates/relspill/src/columns.rs) rows. A call SHALL
refuse as a wrong call bytes too short for the header or of a length
that is no multiple of 8, and bytes without the magic of a chunk of
columns.

#### Scenario: A lane of NULL bits for every 64 words
- **WHEN** the lanes of NULL bits are counted for 0, 64, 65 and 130
  words, and a chunk of 130 words a row is filled, packed and read back
- **THEN** there are 1, 1, 2 and 3 lanes, the words' lanes start after
  them, and every lane reads back
- **Verified by:**
  `crates/relspill/src/columns.rs::wide_chunks_take_a_lane_of_null_bits_per_64_words`

#### Scenario: Rows appended to a chunk
- **WHEN** rows with NULLs, by-value words and by-reference values in up
  to 69 columns are appended to a chunk
- **THEN** each row's words lie in their lanes at the row's place, a
  NULL has its bit set and a word of 0, and a by-reference word is a
  reference to the place of its bytes
- **Verified by:**
  `crates/relspill/src/spill_columns.rs::rows_go_in_order_until_the_chunk_or_the_values_fill`

#### Scenario: Rows of more than 64 words appended by partition
- **WHEN** rows of 64, 65, 70 and 130 words, with NULLs past word 64,
  are appended by partition
- **THEN** the NULL bit of word `w` lies in lane `w / 64`, and the words
  lie in their lanes
- **Verified by:**
  `crates/relspill/src/spill_columns.rs::rows_wider_than_64_words_keep_a_lane_of_null_bits_per_64`;
  `crates/tessera-capi/tests/table.rs::rows_of_70_words_append_by_partition`

#### Scenario: A chunk that is not one of columns
- **WHEN** a chunk of 8 bytes is made, or a chunk whose magic is not
  `COLS` is packed
- **THEN** the call is refused
- **Verified by:**
  `crates/relspill/src/columns.rs::a_damaged_or_foreign_chunk_is_refused`

#### Scenario: The limits of a chunk
- **WHEN** a chunk is made with more than 4096 words a row or with a
  length that is no multiple of 8, its length would hold more than
  131071 rows or more than pack into 4 GiB, or a chunk with a byte past
  a length of 8s is packed
- **THEN** the first two are refused, the capacity stops at 131071 or
  at the rows that pack into 4 GiB, and the packing is refused
- **Verified by:**
  `crates/relspill/src/columns.rs::a_chunk_keeps_within_its_limits`

### Requirement: A reference to a value
A word of a record or of a row that refers to a by-reference value
SHALL be the reference that
[`value_ref`](../../../crates/relspill/src/columns.rs) draws, which
names the value's chunk of values and its byte there, and is never 0
for the chunks and the bytes its documentation allows.

#### Scenario: Rows appended with their values
- **WHEN** rows with by-reference values are appended to a chunk of
  columns and their values to its chunk of values
- **THEN** the word of each such value is a reference to chunk 0 at the
  byte its bytes were copied to
- **Verified by:**
  `crates/relspill/src/spill_columns.rs::rows_go_in_order_until_the_chunk_or_the_values_fill`;
  `crates/tessera-capi/tests/spill.rs::rows_of_datums_append_to_a_chunk_of_columns`

### Requirement: A chunk of columns on disk
A chunk of columns SHALL be stored packed, for its rows only, in the
body that [`columns::pack`](../../../crates/relspill/src/columns.rs)
draws: each lane by frame of reference, its least value and every
value's difference from it in the fewest bytes that hold them all.
[`columns::unpack`](../../../crates/relspill/src/columns.rs) SHALL give
back a chunk with the same rows, words and values, whose capacity is
its rows, and SHALL refuse as damaged bytes a body whose counts, widths
and lanes do not fill the chunk and the body exactly, as its
documentation lists.

#### Scenario: Lanes pack at the width of their span
- **WHEN** a chunk with lanes of equal values and of spans that need 1,
  2, 4 and 8 bytes is packed and unpacked, and a chunk whose lane holds
  -1 and 0 is packed
- **THEN** the packed length is the sum of those widths and the chunk
  reads back; the lane of -1 and 0 has a least value of -1, a width of 1
  byte and the bytes 0 and 1
- **Verified by:**
  `crates/relspill/src/columns.rs::lanes_pack_at_the_width_of_their_span_and_read_back`;
  `crates/relspill/src/columns.rs::a_lane_is_stored_from_its_least_signed_value`

#### Scenario: Empty and full chunks
- **WHEN** a chunk with no rows and a chunk filled to its capacity are
  packed and unpacked
- **THEN** both read back as they were
- **Verified by:**
  `crates/relspill/src/columns.rs::empty_and_full_chunks_round_trip`

#### Scenario: A damaged packed chunk of columns
- **WHEN** a packed chunk of columns is cut short, has a wrong length
  for its counts, a width of 3, bytes past its lanes, or one row more
  than a chunk of its words holds
- **THEN** unpacking fails as damaged bytes
- **Verified by:**
  `crates/relspill/src/columns.rs::a_damaged_or_foreign_chunk_is_refused`;
  `crates/relspill/src/columns.rs::a_chunk_keeps_within_its_limits`

### Requirement: The pair of a chunk of columns
A chunk of columns written with its chunk of values SHALL be written as
the [`PAIR`](../../../crates/relspill/src/spill_columns.rs) of blocks
under one number, its values first, so that a reader has the values in
hand before the words that refer to them; a chunk with no values SHALL
be written as its rows alone.
[`check_pair`](../../../crates/relspill/src/spill_columns.rs) SHALL
refuse as damaged bytes a block read as one of the pair that is of
another kind or number, and
[`check`](../../../crates/relspill/src/spill_columns.rs) SHALL check a
chunk read back once, so that
[`window`](../../../crates/relspill/src/spill_columns.rs) reads its
rows unchecked after: its words, its rows when the caller counted them,
and every word of a column by reference of a row not NULL, which SHALL
refer into its values, to a byte aligned to 8 or to their end, by a
value that ends inside them.

#### Scenario: A pair read back
- **WHEN** rows of a column by value and of a varlena, with NULLs, are
  appended to a chunk, and the chunk is checked with its values and
  read by windows
- **THEN** every word, NULL and value is the one written, a varlena a
  place of its bytes in the values
- **Verified by:**
  `crates/relspill/src/spill_columns.rs::a_chunk_read_back_is_checked_then_read_by_windows`

#### Scenario: A pair not as it was written
- **WHEN** a block of the pair of another kind or number is read, or a
  chunk of another count of words or rows, or with a word that refers
  past its values, to another chunk, to nothing, or to a byte not
  aligned to 8; and a chunk with no values is read as the first block
- **THEN** each of the first is damaged bytes, and the chunk with no
  values is the whole pair
- **Verified by:**
  `crates/relspill/src/spill_columns.rs::a_chunk_read_back_of_another_shape_or_reference_is_damaged`

### Requirement: The file of a set
The crate SHALL own the whole format of the file of a set, which
[`set`](../../../crates/relspill/src/set.rs) draws: the blocks of every
partition one after another, in the order they were written, then the
block list of each partition and a trailer, which a set that wrote a
block writes when it finishes. Its writer,
[`Writer`](../../../crates/relspill/src/set.rs), SHALL fill the write
buffer and the lists its caller keeps, and lay the lists and the
trailer out. Its reader,
[`Reader`](../../../crates/relspill/src/set/reader.rs), SHALL give back
the blocks of one partition of a finished set in the order they were
written, from the writer's own file or another participant's, and no
block of another partition. It SHALL check the trailer, every list and
every entry before it asks for the bytes they name, and each block's
header against the set and its entry, and it SHALL go to a block only
by the place a write gave it.

#### Scenario: A shared file read by another participant
- **WHEN** a participant finishes its file, and another participant
  reads one of its partitions
- **THEN** the reader gets the blocks of that partition in the order
  they were written, and no block of another partition
- **Verified by:**
  `crates/relspill/src/set/reader.rs::a_set_reads_back_by_its_lists_and_from_its_file`

#### Scenario: Blocks of several partitions through one file
- **WHEN** blocks of records and values, among them an empty one and one
  of several write buffers, are written to two of four partitions of a
  set kept in memory and read back
- **THEN** each partition gives its blocks in order with the bytes
  written, and a partition without blocks has none
- **Verified by:**
  `crates/relspill/src/set.rs::a_set_written_in_memory_reads_back`

#### Scenario: A file laid out by hand
- **WHEN** a file is laid out byte by byte as `set` draws it, its blocks
  then its lists and its trailer
- **THEN** the reader gives its blocks back
- **Verified by:**
  `crates/relspill/src/set/reader.rs::a_file_laid_out_by_hand_reads_back`

#### Scenario: A file without its lists
- **WHEN** a file ends without a trailer, the trailer's magic, counts or
  fingerprint do not match, a list counts more or fewer blocks than the
  lists hold, in the partition read or in another, an entry ends past
  the file's blocks, runs into the next block of its partition, is
  shorter than a header or longer than the longest block, or a block's
  header names another partition, level or length than its entry
- **THEN** the read fails as damaged bytes before it asks for the bytes
  that part names
- **Verified by:**
  `crates/relspill/src/set/reader.rs::every_damaged_part_of_a_file_is_refused`;
  `crates/relspill/src/set/reader.rs::a_part_is_checked_before_what_it_names_is_read`

#### Scenario: Calls a set or a reader does not take
- **WHEN** a participant drops a partition of a shared set before it
  finishes the set, and a reader seeks to a place that holds no block of
  its partition or reads a body before its header
- **THEN** each call is a wrong call, and the set and the reader are
  unchanged
- **Verified by:**
  `crates/relspill/src/set.rs::a_shared_set_drops_no_partition_before_its_finish`;
  `crates/relspill/src/set/reader.rs::misuse_of_a_reader_is_a_wrong_call`

### Requirement: The words of a spill
A table that spills SHALL keep its decisions in the words that
[`Spill`](../../../crates/relspill/src/shared_spill.rs) draws, as many
as [`words_for`](../../../crates/relspill/src/shared_spill.rs) gives
for 1 to 65536 partitions: a process's own spill in its own memory, a
shared table's in memory every participant maps. The words of a process
and the shared words SHALL take the same steps, the second by atomic
operations. [`Spill::split`](../../../crates/relspill/src/shared_spill.rs)
SHALL split a table once: the first split holds for every participant.
[`Spill::add_bytes`](../../../crates/relspill/src/shared_spill.rs) SHALL
answer whether the bytes in memory pass the budget, and no count SHALL
go below zero or past 2^64 - 1: a call that would leaves the words as
they were.

#### Scenario: The count of words
- **WHEN** the words are sized for 1, 4 and 65536 partitions, and for 0
  and 65537, and words of another count are given
- **THEN** the first three are 15, 30 and 327690 words, and the others
  are refused
- **Verified by:**
  `crates/relspill/src/shared_spill.rs::the_words_are_sized_for_1_to_65536_partitions`;
  `crates/relspill/src/shared_spill.rs::words_attach_only_as_words_for_sizes_them`

#### Scenario: Own and shared words
- **WHEN** the same steps run over a process's own words and over shared
  ones
- **THEN** every answer is the same
- **Verified by:**
  `crates/relspill/src/shared_spill.rs::local_words_choose_as_shared_ones_do`

#### Scenario: The first split holds
- **WHEN** two participants past the budget split the table at once,
  into 4 and into 8 partitions
- **THEN** the first split holds for both, and a count not a power of
  two is refused
- **Verified by:**
  `crates/relspill/src/loom.rs::participants_past_the_budget_agree_on_one_split`;
  `crates/relspill/src/shared_spill.rs::the_first_split_holds_and_the_largest_partition_goes_to_disk`

#### Scenario: The budget
- **WHEN** bytes up to the budget, past it, below zero and past 2^64 - 1
  are added, the last to a total past which a partition's count would
  still go, and records past 2^64 - 1
- **THEN** the first is not over, the second is, and the others fail
  and leave the words, own or shared, as they were; against another
  participant's call that takes away what it did not add, no count
  wraps, and a call whose second count refuses gives the first one its
  part back
- **Verified by:**
  `crates/relspill/src/shared_spill.rs::a_total_equal_to_the_budget_is_not_over`;
  `crates/relspill/src/shared_spill.rs::a_counter_never_goes_below_zero`;
  `crates/relspill/src/shared_spill.rs::counts_past_their_words_are_refused`;
  `crates/relspill/src/shared_spill.rs::a_refused_update_leaves_the_words_as_they_were`;
  `crates/relspill/src/loom.rs::a_refused_addition_lends_no_bytes_to_another_call`;
  `crates/relspill/src/loom.rs::a_refused_taking_away_gives_the_partition_back`

### Requirement: The plan of a level
[`plan`](../../../crates/relspill/src/plan.rs) SHALL give the
[`Plan`](../../../crates/relspill/src/plan.rs) of a level of a node's
spill, by the one rule its documentation gives, from the node's
[`Shape`](../../../crates/relspill/src/plan.rs) and the level's
[`Level`](../../../crates/relspill/src/plan.rs): its partitions, the
length of its chunks, the spill words that count its partitions, the
bits of the hash it takes, and whether a level fits below it. A shape
of one partition SHALL take no bits, and no level SHALL fit below it.
[`expected`](../../../crates/relspill/src/plan.rs) SHALL give the
bytes a first level expects.

#### Scenario: By the bytes, the participants and the reserve
- **WHEN** a level of 4 to 1024 partitions with a reserve of 48 KiB
  expects 10 MiB in a limit of 4 MiB, then wants 16 partitions at
  least, then has a limit of 256 KiB, then is given 64 partitions
- **THEN** it gets 8 partitions, then 16, then 4, then 64
- **Verified by:**
  `crates/relspill/src/plan.rs::a_level_doubles_by_its_bytes_participants_and_reserve`;
  `crates/relspill/src/plan.rs::a_level_takes_the_partitions_it_is_given`

#### Scenario: By the bits of the hash
- **WHEN** a level of 4 to 1024 partitions that expects far more than
  its limit comes after 28, 29, 30 and 31 bits; and a level of 32
  partitions comes after each level above it; and a level of one
  partition comes after 31 bits and after 32
- **THEN** it gets 8, 4 and 4 partitions, and the last is refused; the
  level of 32 takes 5 bits after those above, and fits six times, from
  bit 0 to bit 25; the level of one has no level below, and after 32
  bits is refused
- **Verified by:**
  `crates/relspill/src/plan.rs::a_level_keeps_to_the_bits_of_the_hash`;
  `crates/relspill/src/plan.rs::each_level_takes_the_bits_past_those_above`

#### Scenario: Wrong bounds and bytes
- **WHEN** the least or the most partitions are not powers of two or
  are out of order, the expected bytes are below zero or not a number,
  the partitions given are no power of two of the shape, the share is
  0, the least chunk is not a multiple of 8, or the partitions pass
  what spill words count
- **THEN** the call is a wrong call, and gives no plan
- **Verified by:**
  `crates/relspill/src/plan.rs::a_plan_refuses_bounds_out_of_order`;
  `crates/relspill/src/plan.rs::a_level_takes_the_partitions_it_is_given`

#### Scenario: A share of the limit
- **WHEN** at a share of 16, with chunks of 8 KiB to 1 MiB, 4 MiB is
  shared among 8 partitions and among 1024, and 1 GiB among 4; and
  1000000 bytes among 8, with chunks of 8 bytes to 1 MiB
- **THEN** the chunks are 32 KiB, 8 KiB and 1 MiB, and 7808 bytes
- **Verified by:**
  `crates/relspill/src/plan.rs::a_chunk_is_a_share_of_the_limit_within_its_bounds`

#### Scenario: The bytes a first level expects
- **WHEN** 1000 bytes are held in 10 rows of 15 expected, of 30, of 30
  for 4 participants, and in no rows
- **THEN** the level expects 2000, 3000, 12000 and 2000 bytes
- **Verified by:**
  `crates/relspill/src/plan.rs::the_first_level_expects_twice_its_bytes_or_the_estimate`

### Requirement: Sending partitions to disk
[`Spill::evict`](../../../crates/relspill/src/shared_spill.rs) SHALL
choose the next partition to send to disk, and mark it so, by the rule
its documentation gives over the
[`Weights`](../../../crates/relspill/src/shared_spill.rs) each node
gives: the partition with the most bytes while the memory passes a
share of the limit, then, once one is on disk, those in memory that
hold too few of the records. A shared table SHALL weigh the bytes its
words count against their budget, a process's own spill the bytes it
measured against its limit, as
[`Memory`](../../../crates/relspill/src/shared_spill.rs) says. A
partition SHALL be marked and counted once, by the participant whose
mark set its flag; one already on disk SHALL be chosen again when it
holds the most, and its holder SHALL then free what it holds of it.
[`splits`](../../../crates/relspill/src/shared_spill.rs) SHALL answer
whether a partition read back splits into a level below, by the
[`SplitWeights`](../../../crates/relspill/src/shared_spill.rs) its
documentation names.

#### Scenario: The weights
- **WHEN** a grouping's weights and a join's choose partitions while
  their memory goes down, with a reserve, a share of records, and a
  limit of partitions a check
- **THEN** the grouping sends from its start down to its target, the
  join keeps room for its tails and a quarter of the records in memory,
  and no check passes its limit
- **Verified by:**
  `crates/relspill/src/shared_spill.rs::a_grouping_goes_from_its_start_down_to_its_target`;
  `crates/relspill/src/shared_spill.rs::a_join_keeps_reserve_for_its_tails_and_a_quarter_in_memory`;
  `crates/relspill/src/shared_spill.rs::the_first_split_holds_and_the_largest_partition_goes_to_disk`

#### Scenario: Marked once
- **WHEN** two participants send the largest partition at once
- **THEN** it is marked and counted once
- **Verified by:**
  `crates/relspill/src/loom.rs::a_partition_goes_to_disk_once`

#### Scenario: A partition chosen again
- **WHEN** a partition on disk holds the most bytes again, with the
  partitions on disk weighed in, and then with a weight of 0 for them
- **THEN** it is chosen again and counted once; with the weight of 0
  the largest in memory goes instead
- **Verified by:**
  `crates/relspill/src/shared_spill.rs::a_grouping_goes_from_its_start_down_to_its_target`

#### Scenario: Room, bits and keys
- **WHEN** partitions of sizes on both sides of the room, with two
  bits left and one, and with a share of rows on both sides of `key`,
  are asked about
- **THEN** only the one past the room, with bits left and below the
  share, splits, and bits past 32 are refused
- **Verified by:**
  `crates/relspill/src/shared_spill.rs::a_partition_splits_past_its_room_with_bits_left_and_many_keys`;
  `crates/relspill/src/shared_spill.rs::a_level_past_the_bits_of_the_hash_is_refused`

### Requirement: The phases of a shared build
A participant of a shared build,
[`Participant`](../../../crates/relspill/src/phases.rs), SHALL step
through the phases that
[`phases`](../../../crates/relspill/src/phases.rs) lists, which the
host's barrier separates, and never wait itself: each step returns an
[`Action`](../../../crates/relspill/src/phases.rs), the host does it,
and the barrier's answer goes into the next step. A participant that
attaches late SHALL join the phase the others are in. A participant in
no known state, or a phase past the last after an attach, SHALL be a
wrong call. The participants SHALL count the build in the four
[`Counters`](../../../crates/relspill/src/phases.rs) they share: each
chunk number goes to one call, and a count past 2^64 - 1 is refused. A
participant that leaves while it probes SHALL first mark a
[`StopWord`](../../../crates/relspill/src/phases.rs), and the last
participant to leave SHALL then return no record without a mark.

#### Scenario: A participant alone
- **WHEN** one participant attaches at the first phase and is elected
  at the size and last at its leave
- **THEN** its actions are attach, build, wait, flush, wait, size,
  wait, link, wait, outer, wait, probe, leave and free
- **Verified by:**
  `crates/relspill/src/phases.rs::a_participant_alone_builds_flushes_sizes_links_and_probes`

#### Scenario: A participant that attaches late
- **WHEN** a participant attaches at each phase from the flush to the
  free
- **THEN** it does that phase's work, or waits at the size, or leaves
  at the free, and then the phases after it
- **Verified by:**
  `crates/relspill/src/phases.rs::a_late_participant_joins_the_phase_the_others_are_in`

#### Scenario: A state or a phase unknown
- **WHEN** a participant holds a state no step makes, or an attach
  answers a phase past the last
- **THEN** the step is a wrong call
- **Verified by:**
  `crates/relspill/src/phases.rs::an_unknown_state_is_an_error`;
  `crates/relspill/src/phases.rs::an_attach_past_the_last_phase_is_an_error`

#### Scenario: Participants that count a build
- **WHEN** three participants number chunks, report records and NULL
  bits, and add duplicates at once, and a count passes 2^64 - 1
- **THEN** no number is given twice, the records and the duplicates are
  their sums and the NULL bits their OR, and the count past the word is
  refused
- **Verified by:**
  `crates/relspill/src/phases.rs::participants_count_a_build_at_once`;
  `crates/relspill/src/loom.rs::three_participants_attach_at_any_phase`;
  `crates/relspill/src/phases.rs::counts_past_their_words_are_refused`

#### Scenario: A participant that stops while it probes
- **WHEN** one of two or three participants of a shared table stops
  while it probes, in every order of their steps; when the model marks
  the word only after that participant left; and when a participant
  asks to stop before it probes or after it left
- **THEN** the last participant to leave sees the mark, and with the
  late mark the model reports a last one that missed it; out of the
  probe nothing is marked
- **Verified by:**
  `crates/relspill/src/loom.rs::the_last_participant_sees_one_that_stopped_while_it_probed`;
  `crates/relspill/src/loom.rs::a_stop_marked_after_leaving_goes_unseen`;
  `crates/relspill/src/phases.rs::a_participant_stops_only_while_it_probes`

### Requirement: The rounds over a partition on disk
After a shared build that spilled, each partition on disk SHALL be a
round of its own, with a barrier of its own and the phases from
[`ROUND_ELECT`](../../../crates/relspill/src/phases.rs) to
[`ROUND_FREE`](../../../crates/relspill/src/phases.rs), stepped by
[`Participant::round_step`](../../../crates/relspill/src/phases.rs) as
a build is. A participant that attaches late SHALL load the files still
left, probe, or leave at once at the free, and an attach that answers
a phase past the round's last SHALL be a wrong call. The words of the
spill SHALL give each participant the partition to start its rounds at,
[`Spill::start`](../../../crates/relspill/src/shared_spill.rs), each
number of a partition's files to one participant,
[`Spill::take_file`](../../../crates/relspill/src/shared_spill.rs), and
a partition whole to one participant only,
[`Spill::take_alone`](../../../crates/relspill/src/shared_spill.rs). A
partition past the partitions in force SHALL be refused, by a count of
records also when it adds none and reads none, and so SHALL a file
number past 2^32 - 1.

#### Scenario: Rounds of several participants
- **WHEN** two or three participants join a round at any phase, load
  its files and probe it
- **THEN** every file is loaded once, every key is found, and the round
  is freed once
- **Verified by:**
  `crates/relspill/src/loom.rs::two_participants_load_a_partition_and_probe_it`;
  `crates/relspill/src/loom.rs::three_participants_join_a_round_at_any_phase`

#### Scenario: A round alone and late
- **WHEN** one participant attaches at each phase of a round, and at a
  phase past the last
- **THEN** its actions follow the phases from the one it attached at,
  and the phase past the last is a wrong call
- **Verified by:**
  `crates/relspill/src/phases.rs::a_round_elects_allocates_loads_probes_and_frees`;
  `crates/relspill/src/phases.rs::an_attach_past_the_last_phase_is_an_error`

#### Scenario: Taken once
- **WHEN** two participants take the files of a partition, and take it
  whole, at once, and participants ask where to start their rounds
- **THEN** each file goes to one of them, the partition to one, and the
  starts go round the partitions; a partition past those in force, by a
  count of records too that adds none and reads none, or a file number
  past 2^32 - 1, is refused
- **Verified by:**
  `crates/relspill/src/loom.rs::files_and_a_whole_partition_go_to_one_participant_each`;
  `crates/relspill/src/shared_spill.rs::files_and_partitions_are_taken_once`;
  `crates/relspill/src/shared_spill.rs::counts_past_their_words_are_refused`

### Requirement: The spill of a join
The crate SHALL provide a state machine for the spill of a hash join,
[`Join`](../../../crates/relspill/src/join.rs).
A build that goes over its memory limit sends partitions to disk. The
partitions kept in memory are probed first. Then each partition on disk
is split into a level below, joined whole, or joined in pieces over
several passes of its outer rows. With a host that does what its
actions say, the machine SHALL give the rows that a join in memory
gives, at any memory limit: every matching pair once; a row of a SEMI
join once; an outer row without a match, in a LEFT, FULL or ANTI join,
once, after the last piece of its partition; and an inner row without
a match, in a RIGHT or FULL join, once.

#### Scenario: A join under any memory limit
- **WHEN** a property test joins random rows through the machine under
  a random memory limit, with the files and the memory kept by the test
- **THEN** the pairs equal those of a join in memory, each as many
  times, and the files are empty at the end
- **Verified by:**
  `crates/relspill/src/join/tests.rs::a_join_through_the_machine_gives_the_pairs_of_one_in_memory`

#### Scenario: A partition joined in pieces
- **WHEN** a partition read back does not fit in the room left, and its
  hash has no bits left or its keys do not split
- **THEN** it is joined in pieces: each matching pair comes once, and
  an outer row without a match comes once, after the last piece
- **Verified by:**
  `crates/relspill/src/join/tests.rs::one_key_too_large_is_joined_in_pieces`;
  `crates/relspill/src/join/tests.rs::a_piece_ends_where_a_group_of_values_starts`

### Requirement: The spill of a grouping
The crate SHALL provide a state machine for the spill of a grouping,
[`Group`](../../../crates/relspill/src/group.rs), and one for the
rows of its generic aggregates,
[`Rows`](../../../crates/relspill/src/group.rs).
Groups are sent to disk by partition and merged when they are read
back. A partition that does not fit is split into a level below. The
rows of aggregates that cannot merge their states are kept on disk and
replayed. With a host that does what its actions say, the machine SHALL
give the groups that a grouping in memory gives, at any memory limit.

#### Scenario: A grouping under any memory limit
- **WHEN** a property test groups random rows through the machine under
  a random memory limit
- **THEN** the groups and their states equal those of a grouping in
  memory
- **Verified by:**
  `crates/relspill/src/group/tests.rs::a_grouping_through_the_machine_gives_the_groups_of_one_in_memory`

#### Scenario: Rows of generic aggregates under any memory limit
- **WHEN** a property test counts random rows through frozen tables
  and their levels of rows under a random memory limit
- **THEN** the groups and their counts equal those of a grouping in
  memory
- **Verified by:**
  `crates/relspill/src/group/tests.rs::rows_through_frozen_tables_give_the_groups_of_one_in_memory`;
  `crates/tessera-capi/tests/group.rs::a_grouping_s_rows_freeze_and_are_read_by_the_entry_points`

### Requirement: The runs of a sort
The crate SHALL provide a state machine for an external sort,
[`Runs`](../../../crates/relspill/src/sort.rs): it says
when a run goes to disk, which sets hold the runs, and which passes
merge them. With a host that does what its actions say, the machine
SHALL give every item once, each no greater than the next by the host's
comparison; items that compare equal MAY come in any order. A sort that
is read again SHALL read its runs again, without sorting its input
again.

#### Scenario: A sort under any memory limit
- **WHEN** a property test sorts random rows through the machine under
  a random memory limit
- **THEN** the items are those given, each once, and each is no
  greater than the next
- **Verified by:**
  `crates/relspill/src/sort/tests.rs::a_sort_through_its_runs_gives_the_rows_of_one_in_memory`;
  `crates/tessera-capi/tests/runs.rs::a_sort_spills_its_runs_step_by_step_by_the_entry_points`

#### Scenario: A sort read again
- **WHEN** the host asks to read the sorted items again from the start
- **THEN** the machine reads its runs again, and the items come in the
  same order
- **Verified by:**
  `crates/relspill/src/sort/tests.rs::passes_merge_until_one_merge_reads_every_run`

### Requirement: A build by several processes and its rounds
The crate SHALL provide a state machine for a hash table that several
processes build together and that may spill,
[`Build`](../../../crates/relspill/src/shared.rs), with its phases and
its rounds over the partitions on disk. A participant MAY attach at any
phase of the build, or to a round in progress, and MAY leave early. A
participant that comes after the in-memory part of the build is freed
SHALL find no work. Each partition on disk SHALL be joined once, by one
participant alone or by all of them together in its round. Every
region SHALL be freed once, and the participants SHALL agree on one
split.

#### Scenario: Participants that attach at any phase
- **WHEN** loom models run two or three participants that attach at
  any phase of a build, of a build that spills or of a round, and two
  that go round the partitions on disk of a table that split
- **THEN** every key is found, every partition on disk is joined once,
  and every region is freed once
- **Verified by:**
  `crates/relspill/src/loom.rs::three_participants_attach_at_any_phase`;
  `crates/relspill/src/loom.rs::participants_write_every_partition_another_sent_to_disk`;
  `crates/relspill/src/loom.rs::three_participants_join_a_round_at_any_phase`;
  `crates/relspill/src/loom.rs::participants_take_each_partition_alone_once_and_join_the_rounds`

#### Scenario: Participants that send partitions to disk as they build
- **WHEN** a loom model runs two participants that append to a table
  split past its budget, each attaching at any phase
- **THEN** a partition one sends to disk the other writes too, and at
  the flush each writes the partitions on disk and keeps the others
- **Verified by:**
  `crates/relspill/src/loom.rs::participants_write_every_partition_another_sent_to_disk`;
  `crates/relspill/src/shared/tests.rs::a_batch_writes_what_others_sent_then_sends_the_largest`

#### Scenario: Participants that pass the budget at once
- **WHEN** a loom model runs two participants whose counts each pass
  the budget of a table not split yet
- **THEN** one split holds, both make their levels of it, and neither
  sees the split before what it reads of it is published
- **Verified by:**
  `crates/relspill/src/loom.rs::participants_past_the_budget_split_once_their_filter_is_published`;
  `crates/relspill/src/shared/tests.rs::a_table_past_its_budget_splits_once_its_plan_is_published`

#### Scenario: A participant that attaches after the split
- **WHEN** a participant attaches after the others agreed on a split
- **THEN** it takes the level the others made before it reads the
  table
- **Verified by:**
  `crates/relspill/src/shared/tests.rs::a_late_participant_catches_up_at_the_flush`;
  `crates/relspill/src/shared/tests.rs::a_late_participant_catches_up_at_the_outer_side_and_the_probe`;
  `crates/relspill/src/loom.rs::participants_write_every_partition_another_sent_to_disk`

#### Scenario: A participant that comes after the build
- **WHEN** a participant attaches after the in-memory part of the build
  is freed
- **THEN** the machine tells it that it has no work, and the rounds of
  the others go on
- **Verified by:**
  `crates/relspill/src/shared/tests.rs::a_late_participant_skips_the_writes_of_a_table_that_never_split`;
  `crates/relspill/src/loom.rs::participants_take_each_partition_alone_once_and_join_the_rounds`

#### Scenario: A partition taken alone
- **WHEN** one participant takes a whole partition on disk
- **THEN** it joins the partition through its join machine, in pieces
  if it does not fit, and no other participant joins it
- **Verified by:**
  `crates/relspill/src/shared/tests.rs::the_partitions_on_disk_are_joined_together_alone_or_released`;
  `crates/relspill/src/loom.rs::participants_take_each_partition_alone_once_and_join_the_rounds`;
  `test/sql/join.sql::-- spec: relspill/A partition taken alone`

### Requirement: Any bytes, a bounded read
Whatever bytes a file holds, a read of the crate SHALL return the data
or an error. It SHALL NOT panic, loop without end, or ask for more than
its caller allows: one block within the larger of its caller's bounds,
with its header and the slack of packing, or one page of a list, and
never more than the file holds. A read SHALL give its caller a chunk
only of the kind and the shape the caller expects, as "The pair of a
chunk of columns" says of a chunk of columns. The promise holds for a
file that nobody writes while it is read. Bytes damaged into other
bytes that pass every check read as other data, since a block has no
checksum.

#### Scenario: Every damage against every read
- **WHEN** each field of a header, a trailer, a list or a packed body
  of a file is damaged at and around its limits, and every read runs
  over the file
- **THEN** each read returns an error that names the rule that refused
  the bytes, or blocks that pass every check
- **Verified by:** pending

#### Scenario: Any bytes as a file
- **WHEN** a fuzzer gives any bytes as a file
- **THEN** no read panics, and none asks for more than its bounds
- **Verified by:** pending

#### Scenario: A chunk of records of another kind
- **WHEN** a block of another kind is read where the caller expects a
  chunk of records
- **THEN** the read is refused as damaged bytes
- **Verified by:** pending

### Requirement: Errors of two kinds
An error of the crate, an
[`Error`](../../../crates/relspill/src/error.rs), SHALL be either a
wrong call or damaged bytes, SHALL name the rule it broke, and SHALL
fit in 8 bytes. A machine given an event it does not expect in its
state SHALL return a wrong call and stay as it was. A call that needs
more memory, and a read or a write a machine asked of the host that the
host could not do, SHALL NOT be errors of the crate. A source of chunks
or of values that the host gives and that cannot give one SHALL be a
wrong call naming the chunk, or the column and the row, so that the
host can report its own reason.

#### Scenario: The kind follows what was wrong
- **WHEN** one call is given a partition past the level's count, and
  another reads a block whose header is damaged
- **THEN** the first is a wrong call and the second is damaged bytes,
  and each names its rule
- **Verified by:**
  `crates/relspill/src/shared_spill.rs::files_and_partitions_are_taken_once`;
  `crates/relspill/src/lib.rs::every_damaged_field_is_refused`;
  `crates/relspill/src/error.rs::an_error_keeps_its_kind_rule_and_numbers`

#### Scenario: An event out of turn
- **WHEN** a machine is given an event its state does not expect
- **THEN** it returns a wrong call, and its next step goes as if the
  event had not come
- **Verified by:**
  `crates/relspill/src/phases.rs::an_attach_past_the_last_phase_is_an_error`;
  `crates/relspill/src/join/tests.rs::an_event_out_of_turn_is_refused_and_changes_nothing`

#### Scenario: A source of the host that cannot give
- **WHEN** a source of chunks cannot give a chunk, or a source of rows
  a value, during an append
- **THEN** the append fails with a wrong call that names the chunk, or
  the column and the row, and the host reports its own reason
- **Verified by:**
  `crates/relspill/src/spill_columns.rs::an_append_names_the_rule_it_refuses_by`;
  `crates/relspill/src/spill_columns.rs::mismatched_shapes_are_refused`;
  `crates/tessera-capi/tests/spill.rs::rows_of_datums_append_to_a_chunk_of_columns`;
  `crates/tessera-capi/tests/table.rs::rows_of_70_words_append_by_partition`

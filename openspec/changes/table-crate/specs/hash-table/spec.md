## ADDED Requirements

### Requirement: The used mark of a chunk
`tess_table_chunk_used` SHALL give the used mark of a chunk the caller
holds, from its base, its length and the size of the table's records.
It SHALL check the mark first: the block can be a chunk, the record
size is one a table's records have, a multiple of 8 of 24 bytes, a
record's header and one key, to 8 bytes less than the longest chunk,
and the mark is at least 8, at most the chunk's length, and 8 more
than a whole number of records. A mark that fails is refused, and
`used` is left as it was. The call reads the first 8 bytes of the
chunk and nothing else.

#### Scenario: The mark of a chunk
- **WHEN** an empty chunk of 64 bytes is asked for its mark, and then
  a chunk of 128 bytes for records of 32 bytes that holds three of them
- **THEN** the first mark is 8 and the second 104
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_mark_of_a_chunk`

#### Scenario: A mark that ends no record
- **WHEN** a chunk of 64 bytes for records of 32 bytes holds a mark of
  0, 20 or 72; or an empty chunk is given a record size of 0, 12, 16
  or the longest chunk's length; or a block that is not aligned to 8
- **THEN** the call fails, and `used` keeps its value; record sizes of
  24 and of 8 bytes less than the longest chunk give the empty chunk's
  mark
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::a_mark_that_ends_no_record`

### Requirement: A lookup in place
`tess_table_probe`, `tess_table_next_match`, `tess_table_next_in_group`,
`tess_bloom_probe` and `tess_bloom_shared_probe` SHALL take as their
mask of results the mask of rows itself, or another mask of the same
words, and SHALL then narrow it in place: it keeps the rows the call
keeps, with the references a call with two masks gives. A mask of the
same words with another row count SHALL be refused. A call refused
SHALL leave the words as they were. Two masks of other words SHALL
share no word. `tess_table_next_match` and `tess_table_next_in_group`
SHALL take the count of steps the lookup has taken, which the caller
sets to 0 with its probe and gives back at each step, and SHALL fail
at a step past the table's count of records.

#### Scenario: The lookups in place
- **WHEN** a batch of 64 rows, of which 16 have keys of the table, is
  probed, stepped through its keys' records by group and by chain, and
  checked against a filter and a shared filter, each call given one
  mask as its rows and results, or two masks of the same words
- **THEN** each call keeps the rows, and gives the references, that a
  call with two masks keeps and gives
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_lookups_narrow_a_mask_in_place`

#### Scenario: A cycle of matches through C
- **WHEN** a record's next names itself, and a caller steps to its next
  match again and again, giving back the count of steps
- **THEN** a step past the count of records fails with
  `TESS_ERROR_INVALID_ARGUMENT`
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_lookups_narrow_a_mask_in_place`

#### Scenario: A lookup in place refused
- **WHEN** a probe in place is given two keys for a table of one, or a
  mask of 63 rows over the words of its mask of 64
- **THEN** the call fails, and the words are as they were
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_lookups_narrow_a_mask_in_place`

## MODIFIED Requirements

### Requirement: Errors of a call
A call SHALL return an error status, and never crash or loop without
end, when the bytes it is given are not a table: a header that breaks
a rule of "The index of a table", an index length below the header's,
a reference outside its chunk or to a byte where no record starts, a
record whose size is not the table's, a used mark that ends no record,
or a chain with a loop. A walk down a chain SHALL take no more steps
than the header's count of records, and no more than the places a
reference can name in the chunks it is given, 2^17 a chunk. Such an
error, and a wrong argument, SHALL be status
`TESS_ERROR_INVALID_ARGUMENT` with SQLSTATE `XX000`. A call SHALL check
its arguments, the header and the count of chunks before it changes
anything, and a chunk before it reads or writes it.

#### Scenario: A damaged header
- **WHEN** one field of a valid header is changed: the magic, the
  version, a length or offset, the buckets or their shift, the record
  size, the keys, the flags or a byte of a reserved word
- **THEN** every call over the table fails, and the SQLSTATE is `XX000`
- **Verified by:**
  `crates/relhash/src/table/tests/format.rs::a_corrupt_header_is_refused`;
  `test/sql/table.sql::tessera_test_table_errors`

#### Scenario: A damaged reference or chain
- **WHEN** a bucket points into a missing chunk, past its chunk or into
  a record, a record's next points to itself, or a used mark ends no
  record, also with a count of records far past what the chunks hold
- **THEN** the probe, the walk or the link fails
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_corrupt_reference_chain_or_used_mark_is_an_error`;
  `crates/relhash/src/table/tests/lookup.rs::a_word_probed_at_once_detects_a_cycle`;
  `crates/relhash/src/table/tests.rs::a_damaged_count_is_bounded_by_the_places_of_the_chunks`

#### Scenario: A gather of many words refused late
- **WHEN** a gather of 17 words is asked of a payload of 16
- **THEN** it fails, and every output is as it was, the first 16 too
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::a_gather_of_many_words_past_the_payload_writes_nothing`

#### Scenario: Arguments are checked first
- **WHEN** a call is given arrays of the wrong length, a missing output
  or a wrong key count
- **THEN** it fails, and the table, the chunks, the masks and the cursor
  are as they were
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::dimension_errors_come_before_any_change`;
  `crates/tessera-capi/tests/table.rs::outputs_are_checked_before_anything_changes`

### Requirement: Appending rows by partition
`tess_table_append_partitioned_columns` SHALL append the rows of the
pending mask, in row order, each as a record to the chunk of its
partition, as `tess_table_append_columns` appends to one chunk. A row
whose partition's chunk has no room SHALL stay pending while the rows
after it go on; the caller gives that partition a new chunk and calls
again. The payload SHALL be a word of NULL bits, bit `c` for column
`c`, then a word for each column, 0 for a NULL, so the call SHALL take
at most 64 columns and a table whose payload is `8 * (1 + columns)`
bytes. Each row appended SHALL add one to its partition's count in
`rows`, which the call adds to and never clears, and SHALL OR its NULL
bits into `*nulls`. As an append does, the call SHALL not need the
index and SHALL not link the records; when the table has an index, the
call SHALL check it and refuse keys or a payload other than the table's
before it writes anything.

#### Scenario: Rows go to the chunks of their partitions
- **WHEN** 70 rows go to 4 partitions at shift 3, the chunk of
  partition 3 having room for 5 records
- **THEN** each row's record lies in its partition's chunk with its
  payload words, the counts are those of the rows appended, `*nulls`
  has the bit of the column with a NULL, and exactly the rows of
  partition 3 past the first 5 stay pending
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_partitioned_columns_entry_point_counts_each_partition`

#### Scenario: An append by partition checks what it is given
- **WHEN** rows are appended by partition with 65 columns, with a table
  whose payload is not a word of NULL bits and a word a column, or to a
  table with an index of another count of keys, and then to that table
  with its own
- **THEN** the wrong ones fail, write nothing and leave every row
  pending, and the table's own go in
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::an_append_by_partition_checks_what_it_is_given`

### Requirement: Splitting a chunk by partition
`tess_table_split` SHALL copy the records of chunk `source` from byte
`*from` on, whole and in the order appended, each to the chunk of its
partition, and SHALL move `*from` past each record copied. It SHALL
copy at most `capacity` records, at least one a call, and give the
reference and the hash of each copy. It SHALL stop before a record
whose partition's chunk has no room and name that partition in
`*full`, -1 otherwise; a count of 0 with `*full` of -1 SHALL mean that
the source is done. A copy's next SHALL be 0: the copies are not
linked. The records SHALL have the size of the keys and the payload
the caller gives, and the source SHALL not be the chunk of a
partition.

#### Scenario: A chunk splits into its partitions
- **WHEN** a chunk of 200 records is split 16 at a time into partitions
  whose chunks hold 8, and each partition named full gets a new chunk
- **THEN** every record is copied once, whole, with a next of 0, to its
  partition, and `*from` ends at the source's used mark
- **Verified by:**
  `crates/relhash/src/table/tests.rs::a_chunk_splits_into_the_chunks_of_its_partitions`;
  `test/sql/table.sql::tessera_test_table_partitions`

#### Scenario: A split that cannot be done is refused
- **WHEN** a split is asked for no records, into one of its partitions'
  chunks, or with a record size other than the source's records
- **THEN** it fails, and `*from` and the chunks are as they were
- **Verified by:**
  `crates/relhash/src/table/tests.rs::a_split_that_cannot_be_done_is_refused`

### Requirement: Groups by partition
`tess_table_find_or_insert_partitioned` SHALL give each row of the
pending mask the record of its keys as `tess_table_find_or_insert`
does, but SHALL write a new record to the chunk of its partition. A row
whose partition's chunk has no room SHALL stay pending while the rows
after it go on. Once the linked records reach half the buckets, the call
SHALL resolve no more rows: the rows not done stay pending, those of a
known key too, until the caller moves the table to a larger index. The
call SHALL add to the mask of inserted rows the rows whose record it
made, as `tess_table_find_or_insert` does. The caller is the one writer
of the table.

#### Scenario: Groups go to the chunks of their partitions
- **WHEN** three batches of keys go to 4 partitions whose chunks hold 8
  records, each partition with pending rows getting a new chunk
- **THEN** each row's record has its key and lies in its partition, a
  key keeps one record across the batches, and the inserted rows are
  the first row of each new key
- **Verified by:**
  `crates/relhash/src/table/tests.rs::groups_go_to_the_chunks_of_their_partitions`;
  `test/sql/table.sql::tessera_test_table_combine`

#### Scenario: Half the buckets stops every row
- **WHEN** a batch brings more new keys than half the buckets can take
- **THEN** the call stops at the first row that needs a new record past
  that count, and that row and every row after it stay pending,
  including rows of keys the table holds
- **Verified by:**
  `crates/relhash/src/table/tests.rs::half_the_buckets_stops_every_row_of_a_lookup_by_partition`

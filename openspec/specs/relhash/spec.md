# relhash Specification

## Purpose
`relhash` is a hash table for the engines of relational databases: the
place where a hash join or a grouping keeps its rows, kept in memory
the caller gives, with no address inside, so that the same bytes serve
one process, several processes over shared memory, and a file on disk.

A table is an **index** and **chunks**, blocks of memory the caller
gives. The index holds a header, which describes the table, and the
buckets. A chunk holds **records** one after another, after its used
mark, the count of its bytes taken. A record is one row: its hash, the
reference of the next record of its chain, its keys and its payload. A
**reference** is a number of 32 bits that names a chunk and a place in
it. The **one writer** of a table is a caller that has it to itself
while it works. A **call** is a call of the crate that takes the index
or the chunks of a table.

A caller whose table spills keeps the records in **partitions**, chosen
by the bits of the hash; a join may check its rows against a **Bloom
filter** of the keys before it probes; and a RIGHT or FULL join keeps
a **mark** for each record a pair matched. The requirements start with
the crate as a whole. Then the table: its index, chunks and records,
the masks and the batches its calls take, its calls of a batch and of
one writer, a caller's records and payloads, the owning table, and
calls at the same time. Then partitions and the calls by partition,
the Bloom filter, alone and shared, and the marks. Last, a damaged
table, the promise over any bytes, the errors and the memory a call
takes. [design.md](design.md) explains the whole and the reasons.

## Requirements

### Requirement: A crate of its own
The table SHALL be the Rust crate `relhash`, which depends on no crate
of Tessera, and SHALL build, pass its tests and pack alone on targets
whose pointers are 64 bits wide; on any other target it SHALL fail to
build, with an error that says why.

Its documentation SHALL be complete by itself, as [the crate's
page](../../../crates/relhash/src/lib.rs) maps it: every public item
SHALL say what it is for, what it takes and gives, who owns what and for
how long, the errors it returns, and the rules, the layouts and the
limits of the API. The documentation SHALL build without warnings, and
every example in it SHALL run as a test.

#### Scenario: The crate packs alone
- **WHEN** the crate is packed, as for publishing
- **THEN** it builds from the packed files and the crates it names,
  none of them Tessera's
- **Verified by:**
  `.github/workflows/ci.yml::cargo publish --dry-run --locked -p relhash`

#### Scenario: The crate's tests run alone
- **WHEN** the tests of the crate run as the only package
- **THEN** they pass
- **Verified by:**
  `.github/workflows/ci.yml::cargo test --locked -p relhash`

#### Scenario: A target of 32 bits
- **WHEN** the crate is built for a target whose pointers are 32 bits
  wide
- **THEN** the build fails with an error that names 64-bit targets
- **Verified by:**
  `.github/workflows/ci.yml::relhash needs a target whose pointers
  are 64 bits wide`

#### Scenario: Documentation built with warnings as errors
- **WHEN** the documentation of the crate is built with every warning
  an error, and with a lint that refuses a public item without a text
- **THEN** it builds
- **Verified by:**
  `.github/workflows/ci.yml::RUSTDOCFLAGS="-D warnings" cargo doc
  --locked -p relhash --no-deps`;
  `crates/relhash/src/lib.rs::#![deny(missing_docs)]`

#### Scenario: The examples run
- **WHEN** the tests of the documentation run
- **THEN** every example passes
- **Verified by:**
  `.github/workflows/ci.yml::cargo test --locked -p relhash --doc`

### Requirement: The index of a table
A table SHALL have one index, a header and its buckets, laid out as
[`HEADER_SIZE`](../../../crates/relhash/src/table/header.rs) draws them.
[`index_size`](../../../crates/relhash/src/table/header.rs) SHALL give
the bytes of an index for a capacity, `TableMut::create` SHALL make an
empty table in them, and `Table::stats` SHALL report what the index
holds.

#### Scenario: The size of an index
- **WHEN** an index is sized for capacities from 0 to 100000
- **THEN** it takes the header and a power of two of buckets, at least
  1024 and at least twice the capacity
- **Verified by:**
  `crates/relhash/src/table/tests/format.rs::index_size_covers_header_and_buckets`;
  `test/sql/table.sql::tessera_test_table_regrow`

#### Scenario: A new index is an empty table
- **WHEN** an index is created and attached again
- **THEN** its stats show no record, the buckets of its capacity and
  its length, and its keys and payload are those it was created with
- **Verified by:**
  `crates/relhash/src/table/tests/format.rs::a_created_table_is_empty_and_attaches_again`;
  `crates/tessera-capi/tests/table.rs::the_entry_points_round_trip`

#### Scenario: An index in the wrong block is refused
- **WHEN** an index is created at an address not aligned to 8, in a
  length that is not a multiple of 8, or in fewer bytes than its size
- **THEN** the call fails and writes nothing
- **Verified by:**
  `crates/relhash/src/table/tests.rs::misaligned_or_odd_blocks_are_refused`

### Requirement: Chunks and references
The records of a table SHALL lie in chunks, and a reference of 32 bits
SHALL name a record by its chunk and its place there, the same in every
process, as [`Chunks`](../../../crates/relhash/src/table/chunks.rs)
draws them. A call SHALL refuse more chunks than the limit, and a chunk
it writes or walks that is not one before it reads it.

#### Scenario: Chunks past the limits are refused
- **WHEN** a call appends to or walks a chunk that is null, not aligned
  to 8, of a length that is not a multiple of 8, shorter than 8 bytes or
  longer than 1 MiB, whose used mark reads as an empty chunk's; or a
  call is given more than 32768 chunks
- **THEN** the call fails
- **Verified by:**
  `crates/relhash/src/table/tests.rs::chunks_past_the_limits_are_refused`;
  `crates/relhash/src/table/tests.rs::misaligned_or_odd_blocks_are_refused`;
  `crates/relhash/src/table/marks.rs::a_walk_refuses_a_chunk_that_cannot_be_one`

#### Scenario: A reference names its chunk and its place
- **WHEN** rows fill one chunk and go on into the next
- **THEN** each row's reference names the chunk and the byte of its
  record, and splits back into them
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::inserted_rows_are_found_and_absent_keys_are_not`;
  `crates/relhash/src/table/tests/records.rs::a_batch_goes_through_two_chunks`;
  `test/sql/table.sql::tessera_test_table_cycle`

#### Scenario: Reference 0 names no record
- **WHEN** a call is asked for the record at reference 0
- **THEN** the call fails, and an empty bucket or the end of a chain
  holds 0
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::reference_zero_names_no_record`

### Requirement: A record
Every record of a table SHALL have the layout that
[`Record`](../../../crates/relhash/src/table/record.rs) draws and the
size that [`record_bytes`](../../../crates/relhash/src/table/header.rs)
gives: 1 to 16 keys of a word each, which the table compares bit for
bit, and a NULL key 0 in its slot with its NULL bit set. A build with
debug assertions SHALL check that a source of keys gives 0 for a NULL.

#### Scenario: The size of a record
- **WHEN** the size of a record is asked for 1 to 16 keys and payloads
  of 0 to 1024 bytes
- **THEN** it is 16 bytes, 8 a key and the payload, rounded up to 8;
  0 and 17 keys are refused
- **Verified by:**
  `crates/relhash/src/table/tests/format.rs::a_record_takes_its_header_its_key_slots_and_its_payload`

#### Scenario: A record as a call reads it
- **WHEN** a record is read through `Table::record`
- **THEN** it has the row's hash, its keys' words, a NULL key as 0 with
  its bit set, and the row's payload
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_null_key_groups_apart_from_the_value_it_hashes_like`;
  `crates/relhash/src/table/tests/lookup.rs::inserted_rows_are_found_and_absent_keys_are_not`;
  `crates/tessera-capi/tests/table.rs::the_entry_points_round_trip`

#### Scenario: A record larger than a chunk
- **WHEN** a table is sized or created for a record that does not fit
  in a chunk of 1 MiB
- **THEN** the call fails, and the largest record that fits is accepted
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_record_larger_than_a_chunk_is_refused`

#### Scenario: A value left in a NULL slot
- **WHEN** a source marks keys NULL but leaves values in their slots,
  in a build with debug assertions
- **THEN** the call stops on the assertion
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::a_value_a_source_leaves_in_a_null_slot_is_caught`

### Requirement: The masks of rows
A mask of rows SHALL say which rows of a batch a call works on, a bit a
row, in exactly the words its rows need, as
[`RowMaskView`](../../../crates/relhash/src/row_mask.rs) draws it. A
mask of other words SHALL be refused, its words left as they were, and a
mask that a call writes SHALL keep every bit past its last row 0.

#### Scenario: Words of the wrong size or with a bit past the rows
- **WHEN** a mask of 70 rows is made of one word or of three, or of two
  words with bit 6 of the second set
- **THEN** it is refused, and its words are not changed
- **Verified by:**
  `crates/relhash/src/row_mask.rs::a_mask_of_wrong_words_is_refused_and_left_as_it_was`

#### Scenario: The bits past the rows of a mask a call writes
- **WHEN** a probe, a find or insert, a filter or an append writes a
  mask of 70 rows that held every row before the call
- **THEN** bits 6 to 63 of its second word are 0
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::the_masks_a_call_writes_keep_the_bits_past_their_rows_clear`

### Requirement: A batch to place
A batch to place, [`Batch`](../../../crates/relhash/src/table/batch.rs),
SHALL hold the hashes and the keys of its rows, a mask of the rows still
to place and a slot for each row's reference, and SHALL be refused when
their row counts differ. A call that places rows SHALL take each row it
places out of the mask and write the reference of its record, and no
other slot; the calls of the owning table SHALL place every row of a
mask they do not change.

#### Scenario: A batch over two chunks
- **WHEN** a batch of four rows, one of them left out of its mask, is
  appended to chunks with room for two records each
- **THEN** the first call places two rows and leaves one pending, the
  second places it, each placed row has the reference of its record,
  and the row left out keeps what its place held
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::a_batch_goes_through_two_chunks`

#### Scenario: The owning table places every row
- **WHEN** the owning table inserts, and finds or inserts, the rows of
  a mask over more records than its first chunk holds
- **THEN** every row of the mask gets the reference of its record, and
  the mask is as it was
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::the_owning_table_places_every_row_of_its_mask`

#### Scenario: A batch of other row counts
- **WHEN** a batch is made of a mask of 70 rows and 69 references
- **THEN** it is refused
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::dimension_errors_come_before_any_change`

### Requirement: Appending rows
An append, [`Table::append`](../../../crates/relhash/src/table/mod.rs),
SHALL write the pending rows of a batch in row order as records into one
chunk while whole records fit, leave the others pending, and link none
of them; it SHALL refuse keys or a payload size other than the table's
before it writes. `append_columns` SHALL take the payload from columns,
[`PayloadColumns`](../../../crates/relhash/src/table/record.rs), without
allocating memory.

#### Scenario: A full chunk leaves rows pending
- **WHEN** a batch of 100 rows is appended to a chunk with room for 16
- **THEN** the first 16 rows get records and leave the mask, the others
  stay, nothing is found before a link, and the rest go into the next
  chunk from byte 8
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_full_chunk_leaves_the_rest_pending_until_linked_elsewhere`

#### Scenario: The payload of a record
- **WHEN** rows are appended with no payload, or with their payload in
  columns, of 3, 64, 65, 130 and 2048 columns, some of them NULL
- **THEN** the payload is zeros, or the words of NULL bits and a word a
  column, the same as an array of those words gives, and the call
  allocates no memory
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_missing_payload_is_zeros`;
  `crates/relhash/src/table/tests/records.rs::payload_columns_append_the_records_a_payload_array_does`;
  `crates/tessera-capi/tests/table.rs::the_columns_entry_point_writes_each_payload_word`;
  `crates/tessera-capi/tests/table.rs::payload_columns_append_without_allocating`

#### Scenario: An append checks the table's index
- **WHEN** rows are appended to a table with an index, with another
  payload size or another count of keys, from an array or from columns,
  and then with the table's own; and rows are appended without an index
- **THEN** with the index the wrong ones fail, write nothing and leave
  every row pending, and the table's own go in; without an index the
  records that the arguments describe go in
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::an_append_checks_the_table_s_index`;
  `crates/tessera-capi/tests/table.rs::an_append_refuses_records_that_are_not_its_table_s`

### Requirement: A batch that fails part way
When a call that places rows fails at a row, because the source of keys
cannot give a word's keys or a partition's chunk is refused, the rows it
placed before SHALL stay placed, and the same batch given again SHALL
place the rest once, as
[`Batch`](../../../crates/relhash/src/table/batch.rs) says. A call that
fails at damaged bytes keeps no such promise.

#### Scenario: A source fails at the second word
- **WHEN** a batch of 128 rows is appended, or found or inserted, and
  the source of keys cannot give its second word
- **THEN** the call fails with a wrong call and the rule of the source,
  the first word's rows have left the pending mask with their
  references, a walk of the chunk sees their records, and the batch
  given again with a source that gives every word places the second
  word after them
- **Verified by:**
  `crates/relhash/src/table/tests/partial.rs::a_source_that_fails_at_the_second_word_leaves_the_first_placed`

#### Scenario: A partition's chunk is refused
- **WHEN** a call by partition meets, in the middle of a word, a
  partition whose chunk's used mark is damaged
- **THEN** it fails with a damaged table; the rows placed before it
  have left the pending mask and, for a find, are in the mask of
  inserted rows; and once the mark is repaired, the batch given again
  places the rest, each row once
- **Verified by:**
  `crates/relhash/src/table/tests/partial.rs::a_partition_with_a_damaged_mark_leaves_the_rows_before_placed`

#### Scenario: A link fails part way
- **WHEN** a link meets a damaged record after it has linked others
- **THEN** the count of records grows by the records linked, and
  `*from` lies past them
- **Verified by:**
  `crates/relhash/src/table/tests/partial.rs::a_link_that_meets_a_damaged_record_counts_what_it_linked`

### Requirement: Linking records
[`Table::link`](../../../crates/relhash/src/table/mod.rs) SHALL link the
records of a chunk, each first in its bucket's chain and counted before
any can be found; `link_counting` SHALL also count the records whose
keys were linked before, exactly over processes that link at once.
[`TableMut::link_grouped`](../../../crates/relhash/src/table/mod.rs)
SHALL put the records of a key together, and `Table::next_in_group`
SHALL step through them.

#### Scenario: Linking counts what was there
- **WHEN** a chunk is linked in two parts, and then again from its end
- **THEN** each part links its records and counts its duplicates, and
  the last call links none
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::linking_counts_the_records_whose_keys_were_there_already`

#### Scenario: Several processes link at once
- **WHEN** two or three processes link chunks of their own into one
  bucket at once, with keys that repeat within and across them
- **THEN** in every order of their steps every record is linked once,
  the count is the total, and the duplicates add up exactly
- **Verified by:**
  `crates/relhash/src/table/loom/table.rs::three_participants_link_into_one_bucket`;
  `crates/relhash/src/table/loom/table.rs::a_key_shared_by_two_participants_counts_once`;
  `crates/relhash/src/table/loom/table.rs::three_records_of_one_key_count_twice`

#### Scenario: A key's records in a row
- **WHEN** records of ten keys, ten each, are linked grouped
- **THEN** next_in_group steps through exactly the records of each key,
  and the count of duplicates is the records less the keys
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::grouped_records_of_a_key_lie_together`;
  `test/sql/table.sql::tessera_test_table_cycle`

#### Scenario: Grouped chains across chunks and a larger index
- **WHEN** grouped records span several chunks and the table moves to a
  larger index
- **THEN** next_in_group still takes as many steps as next_match
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::grouped_chains_span_chunks_and_survive_a_larger_index`

### Requirement: Looking rows up
[`Table::probe`](../../../crates/relhash/src/table/mod.rs) SHALL find
for each row of a lookup,
[`Matches`](../../../crates/relhash/src/table/batch.rs), the first
record with its hash, NULL bits and keys, and `Table::next_match` the
next one, keeping in the mask the rows that found one; the calls that
read found records SHALL change only the rows of their mask. A lookup
whose mask and references differ in rows SHALL be refused.

#### Scenario: Rows find their records
- **WHEN** rows are probed with their keys, with absent keys, and with
  a NULL key under the grouping's rule
- **THEN** each row finds its own record, absent keys find none, and a
  NULL finds the NULL record and not the value it hashes like
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::inserted_rows_are_found_and_absent_keys_are_not`;
  `crates/relhash/src/table/tests/lookup.rs::a_null_key_groups_apart_from_the_value_it_hashes_like`

#### Scenario: Every record of a key
- **WHEN** a key has ten records and its rows step with next_match
- **THEN** each step finds another record of the key, and the step after
  the last finds none and leaves the reference as it was
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::equal_keys_chain_through_next_match`;
  `crates/relhash/src/table/tests/lookup.rs::a_null_key_groups_apart_from_the_value_it_hashes_like`

#### Scenario: Reading the found records
- **WHEN** a payload word, several words or a key of the found rows is
  read, with rows outside the mask
- **THEN** each found row gets its record's value, the other rows keep
  theirs, and a word past the payload or not at a multiple of 8 fails
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::gather_reads_one_payload_word_of_each_selected_match`;
  `crates/relhash/src/table/tests/lookup.rs::a_scattered_gather_reads_what_a_gather_reads`;
  `crates/relhash/src/table/tests/lookup.rs::gather_key_reads_the_key_of_each_found_row`;
  `crates/tessera-capi/tests/table.rs::the_words_entry_point_gathers_every_column`

#### Scenario: A mask and references of other row counts
- **WHEN** a lookup is made of a mask of 70 rows and 69 references
- **THEN** it is refused
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::dimension_errors_come_before_any_change`

### Requirement: A lookup narrows one mask
A probe, a step to the next match or to the next in a group, and a Bloom
filter's probe SHALL keep in the mask only the rows that find what they
look for, leave the reference of a row that leaves as it was, and return
how many rows are left. A lookup SHALL count its steps and fail as a
damaged table at a cycle of matching records, as
[`Matches`](../../../crates/relhash/src/table/batch.rs) says.

#### Scenario: A walk down the matches of a key
- **WHEN** ten rows, one for each of ten keys of ten records, step with
  the next match until no row is left
- **THEN** each step returns the rows left, and each key's walk reads
  its ten records
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::equal_keys_chain_through_next_match`

#### Scenario: A row without a next record
- **WHEN** two rows whose keys have one record each step with the next
  match
- **THEN** the call returns 0, both rows leave, and their references
  stay
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_null_key_groups_apart_from_the_value_it_hashes_like`

#### Scenario: A filter narrows the rows of a lookup
- **WHEN** a filter of the hashes of 100 rows probes every other row
- **THEN** the same 50 rows stay, and the call returns 50
- **Verified by:**
  `crates/relhash/src/table/tests/filter.rs::a_probe_narrows_its_rows_in_place`

#### Scenario: A cycle of matching records
- **WHEN** a record's next names itself, or two records of equal keys
  name each other, and a lookup steps to its next match, or its next in
  a group, again and again
- **THEN** within as many steps as the table has records, the step
  fails with a damaged table and the rule of a chain
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_cycle_of_matching_records_ends_a_walk_over_the_matches`

#### Scenario: A chain longer than a short walk
- **WHEN** 300 records of one hash are linked, and their keys and an
  absent key are probed row by row and by whole words, their matches
  are followed, and a find or insert looks for a known and a new key
- **THEN** every known key is found at the end of its walk, the absent
  key finds nothing, the matches visit all 300 records, and no call
  takes the chain for a cycle
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_healthy_chain_longer_than_a_short_walk_is_walked_to_its_end`

### Requirement: Calls of one writer
The calls of a table's one writer,
[`TableMut`](../../../crates/relhash/src/table/mod.rs), SHALL run only
when no other call uses the table. `find_or_insert` SHALL give each row
the record of its keys, made when the table has none, until the chunk is
full or the records are half the buckets; `regrow` SHALL move the table
to a larger index and move no record; `scan` SHALL visit every record;
and `clear_key` SHALL clear one key of every record.

#### Scenario: Rows find or make their record
- **WHEN** a batch with repeated keys is resolved, a chunk fills, and
  the records reach half the buckets
- **THEN** each key gets one record with a zero payload, inserted marks
  its first row, a full chunk resolves the known keys only, and half the
  buckets stops the call until a larger index
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::rows_find_or_create_the_record_of_their_keys`;
  `crates/relhash/src/table/tests/records.rs::a_full_chunk_resolves_known_keys_only`;
  `crates/relhash/src/table/tests/records.rs::an_index_takes_records_up_to_half_its_buckets_and_regrows`

#### Scenario: Inserted rows add up over the calls of a batch
- **WHEN** a batch of new keys is resolved by two calls, a full chunk
  between them, with the mask of inserted rows cleared once before the
  first
- **THEN** after the second call the mask holds every row that made a
  record, those of the first call too
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::inserted_rows_add_up_over_the_calls_of_a_batch`;
  `crates/tessera-capi/tests/table.rs::the_writer_entry_points_round_trip`

#### Scenario: A larger index keeps the records
- **WHEN** a table moves to a larger index, or to one with fewer buckets
- **THEN** the walk, the references, the payloads and the chains are as
  before; fewer buckets are refused
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::regrowing_keeps_records_and_their_references`;
  `test/sql/table.sql::tessera_test_table_regrow`

#### Scenario: A walk and a cleared key
- **WHEN** the records are walked a few at a time, a walk of no records
  is asked for, and a key is cleared
- **THEN** the walk visits each record once in the order appended and
  ends with 0, the walk of no records is refused, and the key is 0 in
  every record with its NULL bit kept
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::a_walk_visits_every_record_once_in_appended_order`;
  `crates/tessera-capi/tests/table.rs::the_writer_entry_points_round_trip`;
  `crates/relhash/src/table/tests/lookup.rs::a_cleared_key_is_zero_in_every_record`

### Requirement: Records by reference
A caller SHALL read the record that a reference names through
[`RecordCursor`](../../../crates/relhash/src/table/mod.rs), and the one
writer of a table SHALL change its payload through
[`PayloadCursor`](../../../crates/relhash/src/table/writer.rs): each
reference checked as a lookup checks it, one payload open at a time, and
a reference checked first and its payload opened later.

#### Scenario: Records read by reference
- **WHEN** a cursor reads the record of each row's reference, and of a
  reference inside a record
- **THEN** each record holds its row's keys and hash, and the reference
  inside a record fails
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::a_record_cursor_reads_records_by_reference`

#### Scenario: Payloads changed by reference
- **WHEN** a writer adds a value to the payload of the record of each
  row a probe found
- **THEN** each record's payload holds the sum of the values of its rows
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::a_payload_cursor_changes_payloads_by_reference`

#### Scenario: Checked now, opened later
- **WHEN** a writer checks the references of a batch, then opens their
  payloads one by one
- **THEN** each payload is the one of the record its reference names
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::a_payload_cursor_holds_checked_references_to_open_later`

#### Scenario: Two payloads open at once
- **WHEN** a program keeps one payload of a cursor open and opens
  another
- **THEN** it does not compile
- **Verified by:**
  `crates/relhash/src/table/writer.rs::A program that keeps one payload
  open and opens another`

### Requirement: The payloads of a batch
The one writer of a table SHALL change the payloads of the records of a
batch's rows in one call,
[`TableMut::each_payload`](../../../crates/relhash/src/table/mod.rs), or
get a raw pointer to each,
[`TableMut::payload_ptrs`](../../../crates/relhash/src/table/mod.rs),
usable within Rust's rules of aliasing until the chunk is reset. A
reference that fails its check SHALL fail the call, the payloads before
it changed.

#### Scenario: Pointers to the payload of one record
- **WHEN** two rows of a batch name one record, the call gives their
  pointers, and the payload is written through each of them in turn
  after the call, under Miri, also through the C entry point
- **THEN** Miri reports no undefined behavior, and the payload holds
  the last write
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::pointers_to_the_payload_of_one_record_write_it_in_turn`;
  `crates/tessera-capi/tests/table.rs::the_writer_entry_points_round_trip`

#### Scenario: A damaged record in the middle
- **WHEN** the record that the third of five references names has a
  size that is not the table's
- **THEN** the call fails with a damaged table, and the payloads of the
  first two rows hold their values
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::a_damaged_record_in_the_middle_fails_after_the_payloads_before_it`

### Requirement: A merge by the caller's rule
The one writer of a table SHALL merge the records of a chunk into the
table by a rule it gives,
[`TableMut::merge`](../../../crates/relhash/src/table/mod.rs): a record
of keys the table has goes to the rule with the table's record, and one
of new keys is copied and linked. The merge SHALL stop where a new
record finds no room or the rule fails, with the records merged so far
kept and the byte to go on from.

#### Scenario: Known keys and new keys
- **WHEN** a chunk of records, some of keys the table has and some of
  new keys, is merged by a rule that adds the payloads
- **THEN** the records of known keys hold the sums, and the new keys
  have records equal to the ones merged
- **Verified by:**
  `crates/relhash/src/table/tests.rs::a_merge_by_a_rule_changes_known_keys_and_copies_new_ones`

#### Scenario: A full chunk
- **WHEN** the chunk for new records has room for one, and two records
  of new keys come
- **THEN** the merge stops before the second, and gives the byte where
  it begins
- **Verified by:**
  `crates/relhash/src/table/tests.rs::a_merge_stops_for_a_chunk_or_a_larger_index`

#### Scenario: Half the buckets
- **WHEN** the table's records are half its buckets, and a record of
  new keys comes
- **THEN** the merge stops before it, and gives the byte where it
  begins
- **Verified by:**
  `crates/relhash/src/table/tests.rs::a_merge_stops_for_a_chunk_or_a_larger_index`

#### Scenario: The rule fails
- **WHEN** the rule fails on the second of three records of known keys
- **THEN** the merge returns the rule's error, and the first record is
  merged
- **Verified by:**
  `crates/relhash/src/table/tests.rs::a_failing_rule_stops_the_merge_with_its_own_error`

### Requirement: The owning table
Every order of the calls of
[`OwnedTable`](../../../crates/relhash/src/table/owned.rs) SHALL keep
its table whole, and an insertion that fails part way SHALL leave the
rows it placed linked. The owning table SHALL refuse chunks too small
for one record, and SHALL fail, not grow its index again and again, at
the most records a find or insert may make.

#### Scenario: Calls in any order
- **WHEN** random sequences of insertions, finds or insertions, larger
  indexes and resets run over an owning table
- **THEN** after each call its records, their count and the keys it
  finds are those of a model of the sequence
- **Verified by:**
  `crates/relhash/src/table/tests/owned.rs::the_owning_table_stays_whole_under_any_order_of_its_calls`

#### Scenario: An insertion that fails part way
- **WHEN** the source of an insertion into the owning table cannot give
  its second word
- **THEN** the call fails with the rule of the source, the first word's
  keys are found, the count of records is theirs, and the rows given
  again with a source that gives every word are placed once more
- **Verified by:**
  `crates/relhash/src/table/tests/owned.rs::an_insertion_that_fails_at_the_second_word_links_the_first`

#### Scenario: An owning table at its most records
- **WHEN** a find or insert of the owning table needs a record past the
  most records a table may hold
- **THEN** it fails with a wrong call and the rule of that limit
- **Verified by:**
  `crates/relhash/src/table/tests/owned.rs::the_owning_table_grows_up_to_the_most_records_and_then_fails`

#### Scenario: A chunk too small for a record
- **WHEN** an owning table is made with chunks shorter than a record
- **THEN** it is refused with a wrong call and the rule of a record's
  room
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_record_larger_than_a_chunk_is_refused`

### Requirement: Calls at the same time
Over shared memory several processes SHALL be able to append, link, and
probe and read at once, each through a
[`Table`](../../../crates/relhash/src/table/mod.rs) of its own, in the
steps its documentation lists, and a process that finds a record SHALL
see it whole. The calls that read the used mark of every chunk SHALL run
only once every writer's appends are done, as the contracts of
`Table::attach` and `Chunks::new` say.

#### Scenario: A probe beside a link
- **WHEN** one process probes for a key while another links its record
- **THEN** in every order of their steps the probe finds the record
  whole or not at all, and its chain is never taken for a loop
- **Verified by:**
  `crates/relhash/src/table/loom/table.rs::a_probe_sees_a_published_record_whole`

#### Scenario: Weaker orders are caught
- **WHEN** the model reads the buckets with relaxed order, or raises the
  count of records only after it publishes a record
- **THEN** a probe can read a record that is not written yet, or take a
  sound chain for a loop, and the model reports it
- **Verified by:**
  `crates/relhash/src/table/loom/table.rs::relaxed_heads_let_a_probe_read_an_unwritten_record`;
  `crates/relhash/src/table/loom/table.rs::a_count_raised_after_publishing_makes_a_sound_chain_a_loop`

#### Scenario: A build by several processes
- **WHEN** two or three processes append, make the index, link and
  probe one table, each of them starting at any of these steps
- **THEN** every key is found
- **Verified by:**
  `crates/relhash/src/table/loom/table.rs::two_participants_build_a_table_and_find_every_key`;
  `crates/tessera-kernels/src/loom.rs::two_participants_append_size_link_and_probe`;
  `crates/tessera-kernels/src/loom.rs::three_participants_attach_at_any_phase`

#### Scenario: A filter built after the appends
- **WHEN** a filter is built over a shared table
- **THEN** the contracts of attaching the table and of making its
  chunks name the call and the barrier before it
- **Verified by:** review only — the contract forbids the race it
  names, so no run can show it

### Requirement: Safe calls are sound
No sequence of the crate's safe calls SHALL let a record that a caller
has borrowed change while it is borrowed. A call that writes a chunk or
the index SHALL need the table to itself, or be an `unsafe` call whose
contract names the chunks each writer writes, as those of
[`Table`](../../../crates/relhash/src/table/mod.rs) and
[`ChunkWriter`](../../../crates/relhash/src/table/chunks.rs) do.

#### Scenario: A record borrowed across an append
- **WHEN** a caller borrows a record of the owning table and then
  appends to its chunk
- **THEN** the code does not compile
- **Verified by:**
  `crates/relhash/src/table/owned.rs::A record borrowed across an append`

#### Scenario: A chunk writer made in safe code
- **WHEN** a program makes a writer of chunks outside an unsafe block
- **THEN** the code does not compile
- **Verified by:**
  `crates/relhash/src/table/chunks.rs::A chunk writer made in safe code`

#### Scenario: An old reference after a reset
- **WHEN** an owning table is reset, an old reference is read, and new
  rows are appended, under Miri
- **THEN** Miri reports no undefined behavior
- **Verified by:**
  `crates/relhash/src/table/tests/owned.rs::an_old_reference_after_a_reset_reads_what_its_place_holds`

### Requirement: Threads
A handle attached to a table's blocks SHALL NOT pass from one thread to
another, nor be shared by them; the owning table SHALL pass to another
thread whole; and a shared filter may be shared, as [the crate's
page](../../../crates/relhash/src/lib.rs) says.

#### Scenario: A handle sent to another thread
- **WHEN** a program sends a handle attached to a table to another
  thread, or shares one between threads
- **THEN** it does not compile
- **Verified by:**
  `crates/relhash/src/lib.rs::A handle sent to another thread does not compile`

#### Scenario: An owning table sent to another thread
- **WHEN** an owning table is filled on one thread and moved to another
- **THEN** the other thread finds every key
- **Verified by:**
  `crates/relhash/src/table/tests/owned.rs::an_owning_table_filled_on_one_thread_is_probed_on_another`

#### Scenario: Threads with handles of their own
- **WHEN** two threads, each with its own handle, append, link and
  probe one table in every order of their steps
- **THEN** every key is found
- **Verified by:**
  `crates/relhash/src/table/loom/table.rs::two_participants_build_a_table_and_find_every_key`

### Requirement: Partitions of a table
A table that a caller spills SHALL keep its records in the partitions
that [`Partitions`](../../../crates/relhash/src/table/chunks.rs) chooses
by the bits of a hash, and a call SHALL refuse partitions that break
their rules before it changes anything.
[`ChunkWriter::split`](../../../crates/relhash/src/table/chunks.rs)
SHALL copy the records of a chunk to the chunks of their partitions,
whole, in order and unlinked.

#### Scenario: The partition of a hash
- **WHEN** a chunk of 200 records is split into 4 partitions at shift 9
- **THEN** every record goes to the chunk of partition
  `(hash >> 9) & 3`
- **Verified by:**
  `crates/relhash/src/table/tests.rs::a_chunk_splits_into_the_chunks_of_its_partitions`

#### Scenario: Partitions past the limits are refused
- **WHEN** 0, 3 or 131072 partitions are given, 4 partitions at shift
  31, 1 partition at shift 32, or a chunk number past the table's
  chunks, to a split, an append by partition or a lookup by partition
- **THEN** the call fails, and the chunks and the masks are as they were
- **Verified by:**
  `crates/relhash/src/table/tests.rs::partitions_past_the_hash_or_the_chunks_are_refused`;
  `crates/tessera-capi/tests/table.rs::an_append_by_partition_refuses_partitions_past_the_limits`

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

### Requirement: Appending rows by partition
[`ChunkWriter::append_partitioned_columns`](../../../crates/relhash/src/table/chunks.rs)
SHALL append each pending row to the chunk of its partition, leave
pending a row whose chunk is full while the rows after it go on, and
count each row appended in its partition, with no index and no link.

#### Scenario: Rows go to the chunks of their partitions
- **WHEN** 70 rows go to 4 partitions at shift 3, the chunk of
  partition 3 having room for 5 records
- **THEN** each row's record lies in its partition's chunk with its
  payload words, the counts are those of the rows appended, the NULL
  bits have the bit of the column with a NULL, and exactly the rows of
  partition 3 past the first 5 stay pending
- **Verified by:**
  `crates/relhash/src/table/tests.rs::rows_go_to_the_chunks_of_their_partitions`;
  `crates/tessera-capi/tests/table.rs::the_partitioned_columns_entry_point_counts_each_partition`

#### Scenario: An append by partition checks its columns
- **WHEN** rows are appended by partition with 65 columns, or with a
  table whose payload is not a word of NULL bits and a word a column
- **THEN** the call fails, writes nothing and leaves every row pending
- **Verified by:**
  `crates/relhash/src/table/tests.rs::an_append_by_partition_checks_its_columns`;
  `crates/tessera-capi/tests/table.rs::an_append_by_partition_checks_what_it_is_given`

### Requirement: Groups by partition
[`TableMut::find_or_insert_partitioned`](../../../crates/relhash/src/table/mod.rs)
SHALL give each pending row the record of its keys as `find_or_insert`
does, write a new record to the chunk of its partition, leave pending a
row whose chunk is full, and resolve no more rows once the records reach
half the buckets.

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

### Requirement: A Bloom filter of the keys
A Bloom filter SHALL be a power of two of words the caller owns, where a
hash sets the bits that [the module
`bloom`](../../../crates/relhash/src/table/bloom.rs) draws, so that a
row with the hash of a record always passes; at the size
[`bloom::words_for`](../../../crates/relhash/src/table/bloom.rs) gives,
fewer than one in 100 absent keys SHALL pass.

#### Scenario: The bits of a hash
- **WHEN** one hash is added to an empty filter of 1, 2 and 1024 words
- **THEN** exactly the word and the bits the rule gives are set; the
  hash passes, and a hash with any of those bits clear does not
- **Verified by:**
  `crates/relhash/src/table/tests/filter.rs::a_hash_sets_the_bits_the_rule_gives`

#### Scenario: Every key of the table passes
- **WHEN** a filter is filled from a table of 5000 keys over several
  chunks, of int8 keys, or of a NULL key under the rule of a grouping,
  or by adding rows, and its rows are probed with some rows not selected
- **THEN** every key passes, and the rows not selected stay out of the
  mask
- **Verified by:**
  `crates/relhash/src/table/tests/filter.rs::a_filter_lets_every_key_through_and_few_others`;
  `crates/relhash/src/table/tests/filter.rs::a_null_group_key_and_int8_keys_pass_their_filter`;
  `crates/relhash/src/table/tests/filter.rs::a_probe_narrows_its_rows_in_place`;
  `crates/tessera-capi/tests/table.rs::the_filter_entry_points_size_fill_and_probe`

#### Scenario: A filter of the wrong size is refused
- **WHEN** a filter of 0 or 3 words, or hashes of another row count than
  the mask, are given to fill, add or probe
- **THEN** the call fails, and the words and the mask are as they were
- **Verified by:**
  `crates/relhash/src/table/tests/filter.rs::a_filter_of_the_wrong_size_is_refused`;
  `crates/tessera-capi/tests/table.rs::the_filter_entry_points_size_fill_and_probe`

#### Scenario: The words for a count of records
- **WHEN** filters are sized for 0, 4, 5 and 5000 records, within limits
  from 0 to a gigabyte, and for 2^64 - 1 records
- **THEN** they have 1, 1, 2 and 2048 words, halved within a limit while
  more than an eighth of it, and the last is refused
- **Verified by:**
  `crates/relhash/src/table/tests/filter.rs::the_words_of_a_filter_for_its_records`;
  `crates/relhash/src/table/tests/filter.rs::a_spill_filter_stays_within_an_eighth_of_its_memory`

#### Scenario: Few absent keys pass
- **WHEN** a filter of 1024 words holds 4096 keys, 16 bits a key, and
  100000 other keys are probed
- **THEN** fewer than 1000 of them pass
- **Verified by:**
  `crates/relhash/src/table/tests/filter.rs::few_absent_keys_pass_a_filter_of_16_bits_a_key`

### Requirement: A shared Bloom filter
A shared filter,
[`SharedFilter`](../../../crates/relhash/src/table/bloom.rs), SHALL be a
state word and the words of a filter, built by one participant while the
others go on without it and nobody waits, and SHALL be probed only once
ready. `bloom::add_atomic` SHALL let several participants fill one
filter at once.

#### Scenario: One participant builds the filter
- **WHEN** two participants in the model, four threads, or two calls of
  one process try to build a shared filter
- **THEN** exactly one builds it, a probe before it is ready fails, and
  once it is ready every key passes
- **Verified by:**
  `crates/relhash/src/table/loom/filter.rs::two_participants_race_to_build_the_filter_and_one_does`;
  `crates/relhash/src/table/tests.rs::one_of_four_threads_builds_a_shared_filter_every_key_passes`;
  `crates/relhash/src/table/tests/filter.rs::two_calls_of_one_process_build_a_shared_filter_once`;
  `test/sql/table.sql::tessera_test_table_groups`

#### Scenario: A filter that reads ready is whole
- **WHEN** one participant builds the filter while another reads its
  state and probes, in every order of their steps, and when the model
  stores the state relaxed
- **THEN** the reader either finds the filter not ready or sees every
  key pass, and with a relaxed state the model reports a reader that
  sees an unfilled filter
- **Verified by:**
  `crates/relhash/src/table/loom/filter.rs::a_reader_sees_the_filter_whole_once_it_is_ready`;
  `crates/relhash/src/table/loom/filter.rs::a_relaxed_state_lets_a_reader_see_an_unfilled_filter`

#### Scenario: A shared filter of the wrong shape is refused
- **WHEN** a shared filter is not aligned to 8, or has 1 or 4 words
- **THEN** every call over it fails, and its words are as they were
- **Verified by:**
  `crates/relhash/src/table/tests/filter.rs::a_shared_filter_of_the_wrong_shape_is_refused`;
  `crates/tessera-capi/tests/table.rs::the_filter_entry_points_size_fill_and_probe`

#### Scenario: Participants add at once
- **WHEN** two participants add keys of one word at the same time, in
  every order of their steps, and then probe after both are done; and
  when the model adds by a plain read and write
- **THEN** every key passes; with the plain read and write the model
  reports a key whose bits were lost
- **Verified by:**
  `crates/relhash/src/table/loom/words.rs::participants_add_to_one_word_and_lose_no_bit`;
  `crates/relhash/src/table/loom/words.rs::a_plain_read_and_write_loses_the_bits_of_a_filter`

#### Scenario: A filter filled by one participant
- **WHEN** one participant adds the valid rows of a batch and probes
  them back
- **THEN** exactly the valid rows pass
- **Verified by:**
  `crates/relhash/src/table/tests/filter.rs::a_filter_filled_by_one_participant_passes_its_rows`;
  `test/sql/table.sql::tessera_test_table_shared_spill`

### Requirement: The marks of RIGHT and FULL joins
A RIGHT or FULL join SHALL be able to mark each record a pair matched,
in words the caller keeps and the crate reaches through
[`Marks`](../../../crates/relhash/src/table/marks.rs), laid out as its
documentation draws them, and to walk the records without a mark,
[`scan_unmarked`](../../../crates/relhash/src/table/marks.rs), once
every participant's marks are done and while nothing marks or appends.

#### Scenario: The marks of a chunk
- **WHEN** the words of chunks of 8, 40, 2056 and 2088 bytes are asked
  for records of 32 bytes, and the record 70 of chunk 1 is marked
- **THEN** they are 0, 1, 1 and 2, and bit 6 of word 1 of chunk 1's
  run is the only bit set
- **Verified by:**
  `crates/relhash/src/table/marks.rs::a_chunk_has_a_word_of_marks_per_64_records`;
  `crates/relhash/src/table/tests/records.rs::a_record_s_mark_is_its_bit_of_its_chunk_s_run`

#### Scenario: Marked records are left out of the walk
- **WHEN** some records of several chunks are marked, alone or shared,
  and the records without a mark are walked a few at a time, with and
  without marks
- **THEN** the walk gives exactly the records without a mark, each once
  in the order appended, or every record without marks, and ends with 0
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::marked_records_are_left_out_of_the_walk`;
  `crates/relhash/src/table/tests/records.rs::records_marked_by_threads_at_once_are_left_out_of_the_walk`;
  `crates/tessera-capi/tests/table.rs::the_mark_entry_points_leave_marked_records_out`

#### Scenario: Marks and walks that cannot be done are refused
- **WHEN** a mark names a byte before a chunk's records, off a record's
  boundary, past its chunk's length or in no chunk; a record size is 0,
  12 or larger than a chunk; a cursor is off a record or past its
  chunk's used mark; or a walk asks for no records
- **THEN** the call fails and the cursor is as it was
- **Verified by:**
  `crates/relhash/src/table/marks.rs::a_walk_refuses_a_used_mark_or_a_cursor_off_the_records`;
  `crates/relhash/src/table/tests/records.rs::a_mark_or_a_walk_that_cannot_be_done_changes_nothing`;
  `crates/tessera-capi/tests/table.rs::marks_and_walks_that_cannot_be_done_are_refused`

#### Scenario: Participants mark at once
- **WHEN** two participants mark records of one word at the same time,
  in every order of their steps, and the records are walked after both
  are done; and when the model marks by a plain read and write
- **THEN** the walk gives no marked record; with the plain read and
  write the model reports a mark that was lost
- **Verified by:**
  `crates/relhash/src/table/loom/words.rs::participants_mark_one_word_and_lose_no_mark`;
  `crates/relhash/src/table/loom/words.rs::a_plain_read_and_write_loses_marks`

### Requirement: A damaged table
A call SHALL return an error of a damaged table, and never panic or loop
without end, when the bytes it reads are not a table, as
[`Kind::Damaged`](../../../crates/relhash/src/error.rs) lists. A walk
down a chain SHALL take no more steps than the count of records and the
places of its chunks, and a call SHALL check its arguments, the header
and the count of chunks before it changes anything, and a chunk before
it reads or writes it.

#### Scenario: A damaged header
- **WHEN** one field of a valid header is changed: the magic, the
  version, a length or offset, the buckets or their shift, the record
  size, the keys, the flags or a byte of a reserved word
- **THEN** every call over the table fails
- **Verified by:**
  `crates/relhash/src/table/tests/format.rs::a_corrupt_header_is_refused`

#### Scenario: A damaged reference or chain
- **WHEN** a bucket points into a missing chunk, past its chunk or into
  a record, a record's next points to itself, or a used mark ends no
  record, also with a count of records far past what the chunks hold
- **THEN** the probe, the walk or the link fails
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_corrupt_reference_chain_or_used_mark_is_an_error`;
  `crates/relhash/src/table/tests/lookup.rs::a_word_probed_at_once_detects_a_cycle`;
  `crates/relhash/src/table/tests.rs::a_damaged_count_is_bounded_by_the_places_of_the_chunks`

#### Scenario: Arguments are checked first
- **WHEN** a call is given arrays of the wrong length or a wrong key
  count
- **THEN** it fails, and the table, the chunks, the masks and the cursor
  are as they were
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::dimension_errors_come_before_any_change`

### Requirement: Any bytes, a bounded call
Under the caller's promise, which [the crate's
README](../../../crates/relhash/README.md) states, a call SHALL end
after a number of steps bounded by the lengths of its blocks and its
arguments, whatever bytes the blocks hold and after a call that failed
as well: it SHALL return a result or an error, SHALL NOT panic, and
SHALL read and write no memory but its blocks and arguments. Calls in
one process over the same bytes, with the same arguments and in the same
order, SHALL leave the same bytes and give the same answers.

#### Scenario: Every damage against every call
- **WHEN** a valid table is damaged in one place, from a list that
  covers every field of the header, a bucket, the next, hash, NULL bits
  and length of a record, a used mark and the count of records, each
  with the values at and around its limits; then every call of the
  crate runs over it, and more calls after the first error
- **THEN** each call ends with a result or the error of the rule the
  damage breaks, and the bytes around every block are as they were
- **Verified by:**
  `crates/relhash/src/table/tests/damage.rs::every_damage_against_every_call`

#### Scenario: Bytes a fuzzer chose
- **WHEN** a fuzzer builds tables from its input, damages them in one
  or more places, or gives the calls any arguments, and runs calls over
  them
- **THEN** no call panics, runs without end, or reads or writes outside
  its blocks
- **Verified by:** `crates/relhash/fuzz/fuzz_targets/damaged.rs::fuzz_target!`;
  `crates/relhash/fuzz/fuzz_targets/arguments.rs::fuzz_target!`

#### Scenario: The checks hold for all values
- **WHEN** a header passes its checks, a reference passes its check, a
  walk starts over a chain, a chunk or its marks, or a hash picks a
  partition or a word of a filter, for all values of the fields,
  references, used marks, counts and sizes the table and the call may
  hold
- **THEN** every bucket lies inside the index, every record inside its
  chunk, a partition, a word of a filter and a word of marks inside
  their arrays, every walk ends within the bound its blocks give, and
  nothing panics
- **Verified by:**
  `crates/relhash/src/table/proofs.rs::a_header_that_passes_puts_every_bucket_inside_the_index`;
  `crates/relhash/src/table/proofs.rs::the_record_count_is_bounded_by_the_chunks`;
  `crates/relhash/src/table/proofs.rs::a_reference_that_passes_names_a_record_inside_its_chunk`;
  `crates/relhash/src/table/proofs.rs::a_used_mark_that_passes_ends_a_record`;
  `crates/relhash/src/table/proofs.rs::a_walk_over_a_chunk_stays_inside_it`;
  `crates/relhash/src/table/proofs.rs::a_walk_down_a_chain_ends_within_the_record_count`;
  `crates/relhash/src/table/proofs.rs::a_partition_lies_within_the_partitions`;
  `crates/relhash/src/table/proofs.rs::the_partitions_that_pass_name_chunks_of_the_table`;
  `crates/relhash/src/table/proofs.rs::a_word_of_the_filter_lies_within_it`;
  `crates/relhash/src/table/proofs.rs::the_words_of_a_filter_within_a_limit_are_a_filter`;
  `crates/relhash/src/table/proofs.rs::a_mark_sets_the_bit_of_its_record`;
  `crates/relhash/src/table/proofs.rs::a_walk_over_marks_reads_words_within_them`

#### Scenario: One sequence, twice
- **WHEN** the same sequence of calls runs twice, over two copies of the
  same bytes
- **THEN** the blocks and the answers of the two runs are equal
- **Verified by:**
  `crates/relhash/src/table/tests/damage.rs::one_sequence_twice_gives_the_same_blocks_and_answers`

### Requirement: Errors of two kinds
An error of a call SHALL name the rule that was broken and be of one of
two kinds, a wrong call or a damaged table, which what the call read
decides, as [`Kind`](../../../crates/relhash/src/error.rs) lists them.

#### Scenario: Each damage names its rule
- **WHEN** one rule of the format is broken in a valid table, and every
  call that reads the damaged bytes runs over it
- **THEN** each of them fails with a damaged table and that rule
- **Verified by:**
  `crates/relhash/src/table/tests/damage.rs::every_damage_against_every_call`

#### Scenario: Each wrong argument names its rule
- **WHEN** a call is given one argument that breaks one rule of a wrong
  call
- **THEN** it fails with a wrong call and that rule; when the argument
  is a size, a count or a block, which a call checks before its first
  row, it fails before it changes anything
- **Verified by:**
  `crates/relhash/src/table/tests/arguments.rs::each_wrong_argument_names_its_rule`;
  `crates/relhash/src/table/tests/arguments.rs::every_rule_has_its_case`

#### Scenario: A wrong chunk given to a regrow or a cleared key
- **WHEN** a larger index or a cleared key is given chunks of which the
  last cannot be a chunk
- **THEN** the call fails with a wrong call and the rule of a chunk,
  and the index, the chunks and the keys are as they were
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::a_wrong_last_chunk_fails_the_calls_over_every_chunk_before_a_change`

### Requirement: No memory taken
A call over blocks the caller gives SHALL allocate no memory, also when
it fails. [`OwnedTable`](../../../crates/relhash/src/table/owned.rs),
which allocates its blocks, is outside this promise, and a build without
the crate's feature `alloc` leaves it out.

#### Scenario: Calls under a counting allocator
- **WHEN** every call of the crate runs over the caller's blocks, with
  success and with each kind of error, under an allocator that counts
- **THEN** the count does not change
- **Verified by:**
  `crates/relhash/src/table/tests/damage.rs::every_damage_against_every_call`;
  `crates/relhash/src/table/tests/arguments.rs::each_wrong_argument_names_its_rule`

#### Scenario: A build without the owning table
- **WHEN** the crate is built and tested without its feature `alloc`
- **THEN** it builds, its tests that need no owning table run, and it
  has no table that owns its memory
- **Verified by:**
  `Makefile::test --locked -p relhash --no-default-features`

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

### Requirement: The C API over the crate
Each call of `tessera/table.h` over a table SHALL do what the calls of
the crate `relhash` do, as the requirement of the spec of `relhash`
named here says, and SHALL return the crate's error as a status, as
"The status of an error" says:

- `tess_table_format_version`, `tess_table_size`, `tess_table_create`
  and `tess_table_stats`: "The index of a table";
- `tess_table_chunk_init`, `tess_table_ref`, `tess_table_ref_chunk`
  and `tess_table_ref_byte`, which make a reference and split it:
  "Chunks and references";
- `tess_table_record_size`, `tess_table_record_bytes` and
  `tess_table_record`: "A record";
- `tess_table_append` and `tess_table_append_columns`, which take the
  index or none and without one append as a chunk writer does:
  "Appending rows";
- `tess_table_link`, which counts duplicates when asked: "Linking
  records";
- `tess_table_probe`, `tess_table_next_match`, `tess_table_gather`,
  `tess_table_gather_scattered`, `tess_table_gather_words`,
  `tess_table_gather_key` and `tess_table_payloads`: "Looking rows up";
- `tess_table_link_grouped` and `tess_table_next_in_group`: "Linking
  records";
- `tess_table_find_or_insert`, `tess_table_regrow`, `tess_table_scan`
  and `tess_table_clear_key`: "Calls of one writer";
- `tess_table_partition`, which gives the partition of a hash:
  "Partitions of a table";
- `tess_table_append_partitioned_columns`, which takes the index or
  none and appends as a chunk writer does: "Appending rows by
  partition";
- `tess_table_split`: "Partitions of a table";
- `tess_table_find_or_insert_partitioned`: "Groups by partition";
- `tess_table_bloom`, `tess_bloom_add` and `tess_bloom_probe`: "A Bloom
  filter of the keys";
- `tess_table_bloom_words` and `tess_table_bloom_words_within`: "A
  Bloom filter of the keys";
- `tess_bloom_shared_words`, `tess_bloom_shared_init`,
  `tess_table_try_build_bloom`, `tess_bloom_shared_ready` and
  `tess_bloom_shared_probe`: "A shared Bloom filter";
- `tess_bloom_add_atomic`: "A shared Bloom filter";
- `tess_table_mark_words`, `tess_table_mark` and
  `tess_table_next_unmarked`, which take the index or none, and whose
  cursor starts at 0: "The marks of RIGHT and FULL joins".

Beside them the C API SHALL do what the crate leaves to its caller:

- the key columns of a call SHALL put each value into its word, an
  `int4` widened with its sign and an `int8` as it is, so that an
  `int4` key and an `int8` key of the same value match;
  `tess_table_gather_key` SHALL give a key's word as a Datum with its
  NULL flag;
- a lookup and a check against a Bloom filter SHALL take a mask of rows
  and a mask of results, and SHALL fill the mask of results whole with
  the rows the crate's call keeps;
- a call over chunks that is given the index SHALL check it before it
  changes anything: an append by partition SHALL refuse keys or a
  payload other than the index's, and a mark or a walk of marks a
  record size other than the index's;
- a debug build of the entry points SHALL check every chunk it is
  given;
- `tess_table_memory_limit` SHALL return a limit of memory as it is up
  to `TESS_TABLE_MEMORY_MAX`, 16 GiB, half of what the most chunks of
  the longest length hold, and that maximum for a larger limit;
- over a shared table the counters of a build, the words of a spill
  and the marks SHALL change by atomic operations too, and a barrier
  SHALL order what one participant did before the next phase of the
  others.

#### Scenario: Keys of int4 and int8 through C
- **WHEN** a record is read through `tess_table_record`, and rows with
  `int4` keys probe the records of `int8` keys and the other way round
- **THEN** the record has its `int4` key widened with its sign, and each
  row finds the record of its value
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_entry_points_round_trip`;
  `crates/tessera-capi/tests/table.rs::int4_keys_find_the_records_of_int8_keys`;
  `crates/tessera-capi/tests/table.rs::int8_keys_find_the_records_of_int4_keys`

#### Scenario: The index a call over chunks checks
- **WHEN** rows are appended by partition to a table with an index of
  another count of keys, and then to that table with its own; and a
  mark or a walk is given a record size other than the index's
- **THEN** the wrong ones fail and change nothing, and the table's own
  rows go in
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::an_append_by_partition_checks_what_it_is_given`;
  `crates/tessera-capi/tests/table.rs::marks_and_walks_that_cannot_be_done_are_refused`

#### Scenario: A debug build checks every chunk
- **WHEN** a probe that reads no chunk is given a chunk longer than
  1 MiB
- **THEN** in a debug build of the entry points it fails, and in a
  release build it goes on
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::a_debug_build_checks_every_chunk`

#### Scenario: The most memory of a table
- **WHEN** limits of 0, 8 MiB and one byte under 16 GiB are given, and
  of 16 GiB, one byte over it, 64 GiB and the largest size
- **THEN** the first three come back as they are and the others as
  16 GiB, which fills half of the 32768 chunks of 1 MiB
- **Verified by:** `test/sql/table.sql::tessera_test_table_memory_limit`

#### Scenario: Sizes and partitions through C
- **WHEN** the size of a record is asked through the C API for 1 to 16
  keys and payloads of 0 to 1024 bytes, and for 0 and 17 keys; and a
  chunk of records is split into partitions through the C API
- **THEN** each size is the one `tess_table_record_bytes` gives, 0 and
  17 keys are refused, and every record goes to the chunk of the
  partition `tess_table_partition` gives for its hash
- **Verified by:**
  `test/sql/table.sql::tessera_test_table_record_size`;
  `test/sql/table.sql::tessera_test_table_partitions`

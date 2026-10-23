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
- `tess_table_link_grouped` and `tess_table_next_in_group`: "The
  records of a key together";
- `tess_table_find_or_insert`, `tess_table_regrow`, `tess_table_scan`
  and `tess_table_clear_key`: "Calls of one writer";
- `tess_table_partition`, which gives the partition of a hash:
  "Partitions of a table";
- `tess_table_append_partitioned_columns`, which takes the index or
  none and appends as a chunk writer does: "Appending rows by
  partition";
- `tess_table_split`: "Splitting a chunk by partition";
- `tess_table_find_or_insert_partitioned`: "Groups by partition";
- `tess_table_bloom`, `tess_bloom_add` and `tess_bloom_probe`: "A Bloom
  filter of the keys";
- `tess_table_bloom_words` and `tess_table_bloom_words_within`: "The
  size of a Bloom filter";
- `tess_bloom_shared_words`, `tess_bloom_shared_init`,
  `tess_table_try_build_bloom`, `tess_bloom_shared_ready` and
  `tess_bloom_shared_probe`: "A shared Bloom filter";
- `tess_bloom_add_atomic`: "A filter filled together";
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

## ADDED Requirements

### Requirement: The status of an error
A call SHALL return an error status, and never crash or loop without
end, when the bytes it is given are not a table, as "A damaged table"
of the spec of `relhash` says. Such an error, and a wrong argument,
SHALL be status `TESS_ERROR_INVALID_ARGUMENT` with SQLSTATE `XX000`. A
call SHALL check its arguments and its outputs before it changes
anything.

#### Scenario: A damaged table through C
- **WHEN** a call is given an index of zeros, an index whose version is
  changed, or one moved by 4 bytes or cut by 8
- **THEN** it fails with status `TESS_ERROR_INVALID_ARGUMENT`, and the
  SQLSTATE is `XX000`
- **Verified by:** `test/sql/table.sql::tessera_test_table_errors`

#### Scenario: A gather of many words refused late
- **WHEN** a gather of 17 words is asked of a payload of 16
- **THEN** it fails, and every output is as it was, the first 16 too
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::a_gather_of_many_words_past_the_payload_writes_nothing`

#### Scenario: Arguments are checked first
- **WHEN** a call is given a missing output
- **THEN** it fails, and the table, the chunks, the masks and the cursor
  are as they were
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::outputs_are_checked_before_anything_changes`

### Requirement: A participant that stops while it probes
Over a shared table, the last participant to leave walks the records
without a mark, as "The marks of RIGHT and FULL joins" of the spec of
`relhash` says. A participant that leaves while it probes, its share of
the outer side not done, SHALL first mark a word that the participants
of the table, or of the round over a partition, share, with
`tess_build_stop`, which SHALL mark it only while the participant
probes. The last participant to leave SHALL read the word with
`tess_build_stopped` and, when it is marked, return no record without a
mark: the pairs of the rows left unprobed were never marked, and a
participant stops so only when its plan wants no more rows.

#### Scenario: A participant that stops while it probes
- **WHEN** one of two or three participants of a shared table stops
  while it probes, in every order of their steps; when the model marks
  the word only after that participant left; when a participant asks to
  stop before it probes or after it left; and when a worker under the
  core's `Gather` stops at the `LIMIT` of a RIGHT join
- **THEN** the last participant to leave sees the mark, and with the
  late mark the model reports a last one that missed it; out of the
  probe nothing is marked; the join returns no row without a pair where
  every inner row has one
- **Verified by:**
  `crates/tessera-kernels/src/loom.rs::the_last_participant_sees_one_that_stopped_while_it_probed`;
  `crates/tessera-kernels/src/loom.rs::a_stop_marked_after_leaving_goes_unseen`;
  `crates/tessera-kernels/src/phases.rs::a_participant_stops_only_while_it_probes`;
  `crates/tessera-capi/tests/table.rs::a_participant_marks_its_stop_only_while_it_probes`;
  `test/sql/join.sql::worker that has its share stops while it probes`

## REMOVED Requirements

### Requirement: Errors of a call
**Reason**: What makes the bytes of a table not a table, the bound of
a walk and the order of the checks are the crate's; they move to the
spec of `relhash`. The status and the SQLSTATE of such an error stay,
under a name that says so.
**Migration**: "A damaged table" of `relhash`; "The status of an error"
of `hash-table`.

### Requirement: The marks of RIGHT and FULL joins
**Reason**: The marks are the crate's, `Marks`, `mark` and
`scan_unmarked`; they move to `relhash`. What Tessera does around them
when a participant stops while it probes stays, under a name that says
so.
**Migration**: "The marks of RIGHT and FULL joins" of `relhash`; "A
participant that stops while it probes" of `hash-table`.

### Requirement: Partitions of a table
**Reason**: The rule is the crate's, `Partitions`; it moves to the spec
of `relhash`.
**Migration**: "Partitions of a table" of `relhash`; "The C API over
the crate" names `tess_table_partition`.

### Requirement: Appending rows by partition
**Reason**: The call is the crate's,
`ChunkWriter::append_partitioned_columns`; it moves to `relhash`.
**Migration**: "Appending rows by partition" of `relhash`; the index
the C call checks is in "The C API over the crate".

### Requirement: Splitting a chunk by partition
**Reason**: The call is the crate's, `ChunkWriter::split`; it moves to
`relhash`.
**Migration**: "Splitting a chunk by partition" of `relhash`.

### Requirement: Groups by partition
**Reason**: The call is the crate's,
`TableMut::find_or_insert_partitioned`; it moves to `relhash`.
**Migration**: "Groups by partition" of `relhash`.

### Requirement: A Bloom filter of the keys
**Reason**: The filter is the crate's, its module `bloom`; it moves to
`relhash`.
**Migration**: "A Bloom filter of the keys" of `relhash`; the mask of
results of a C check is in "The C API over the crate".

### Requirement: The size of a Bloom filter
**Reason**: The sizes are the crate's, `bloom::words_for` and
`bloom::words_within`; they move to `relhash`.
**Migration**: "The size of a Bloom filter" of `relhash`.

### Requirement: A shared Bloom filter
**Reason**: The shared filter is the crate's, `SharedFilter`; it moves
to `relhash`.
**Migration**: "A shared Bloom filter" of `relhash`.

### Requirement: A filter filled together
**Reason**: The call is the crate's, `bloom::add_atomic`; it moves to
`relhash`.
**Migration**: "A filter filled together" of `relhash`.

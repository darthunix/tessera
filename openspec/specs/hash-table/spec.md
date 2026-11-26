# hash-table Specification

## Purpose
The C API over the hash table of the crate `relhash`, and what joins,
groupings and sorts keep beside the table: the forms of their keys,
the status of an error, the merge of groups read back, and the C calls
over the steps and the words of a build by several processes, which the
crate `relspill` decides. When a node builds a table, how large it
makes its chunks and when it spills belong to the node's own
capability.

The table itself, its index, chunks and records, its partitions, its
Bloom filter and its marks, and what each of its calls promises, is in
the spec of [relhash](../relhash/spec.md); the phases of a build by
several processes, its rounds over partitions on disk, the words of its
spill, the partitions it sends to disk and the plan of a level are in
the spec of [relspill](../relspill/spec.md). The requirements here
start with the C calls over the table, the used mark of a chunk and a
lookup in place, then the SQL types of a key and the status of an
error. Then the merge of groups read back. Last, the C calls over the
spill of the crate `relspill`. [design.md](design.md) explains the
whole and the reasons.

## Requirements

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
- `TESS_TABLE_CHUNK_HEADER`, `TESS_TABLE_RECORD_HASH_OFFSET`,
  `TESS_TABLE_RECORD_NEXT_OFFSET`, `TESS_TABLE_RECORD_NULL_BITS_OFFSET`,
  `TESS_TABLE_RECORD_SIZE_OFFSET`, `TESS_TABLE_RECORD_HEADER` and
  `TESS_TABLE_KEY_SLOT` SHALL hold the crate's constants of a chunk and
  of a record, which `tess_table_layout` gives, and the kernels' module
  SHALL refuse a library whose constants differ;
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
  keys and payloads of 0 to 1024 bytes, and for 0 and 17 keys; a chunk
  of records is split into partitions through the C API; and the
  kernels are asked the bytes of a chunk's used mark and the offsets
  of a record's fields
- **THEN** each size is the one `tess_table_record_bytes` gives, 0 and
  17 keys are refused, every record goes to the chunk of the partition
  `tess_table_partition` gives for its hash, and the kernels give the
  crate's constants
- **Verified by:**
  `test/sql/table.sql::tessera_test_table_record_size`;
  `test/sql/table.sql::tessera_test_table_partitions`;
  `crates/tessera-capi/tests/table.rs::the_layout_probes_match_the_types`;
  `test/sql/table.sql::tessera_test_table_layout`

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

### Requirement: The SQL types of a key
A join and a grouping SHALL put each column of a key into the table in
one of two forms, by the column's type, and SHALL leave a key of any
other type to the core:

- the value itself: `int2`, `int4`, `date` and `bool` as kind 1, and
  `int8`, `timestamp`, `timestamptz`, `time` and `money` as kind 2,
  since their values are equal exactly when their Datums are;
- a number of 64 bits that stands for the value, as kind 2, for any
  other type whose equality is the type's default one and has a hash
  function, such as `text`, `numeric`, `float8` or `interval`: in a
  grouping, the value's number in a dictionary of the node, which finds
  equal values by the type's own hash and equality; in a join, the
  value's hash from the type's own function, and the type's equality
  then decides every pair the table finds.

A key of a type without a hash function, such as `bit varying`, a join
by `money`, whose equality PostgreSQL does not let a hash join use, and
a join by the equality of two types other than the integers, such as
`float4` with `float8`, SHALL leave the grouping or the join to the
core's node.

#### Scenario: Keys whose values a word holds
- **WHEN** a join or a grouping has keys of `int2`, `date`, `bool`,
  `timestamp` or `timestamptz`, negative and NULL among them, or joins
  `int2` with `int4` and `int8`
- **THEN** Tessera's node runs it, and its rows are the core's
- **Verified by:**
  `test/sql/types.sql::Keys the table keeps in a word besides int4 and int8`

#### Scenario: Keys of time and money in a word
- **WHEN** a grouping or a join has keys of `time`, or a grouping has
  keys of `money`, of both signs, the edges of their ranges and NULL
  among them
- **THEN** Tessera's node runs it, a join's plan shows no join filter
  for the key, and the rows are the core's
- **Verified by:**
  `test/sql/types.sql::Keys of time and money in a word`

#### Scenario: Keys through a number for the value
- **WHEN** a grouping or a join has keys of `text` or `numeric`, with
  numeric 1.0 and 1.00, float8 -0 and 0, and text of either case under
  a case-insensitive collation
- **THEN** Tessera's node runs it, a join's plan shows the equality as
  its join filter, equal values meet though their bytes differ, and the
  rows are the core's
- **Verified by:**
  `test/sql/types.sql::Keys a word does not hold: text, numeric`;
  `test/sql/types.sql::Hash joins by keys a word does not hold`;
  `test/sql/types.sql::Equal values of other forms`

#### Scenario: A key through a number, on disk
- **WHEN** a join by a key of `text` that is an expression its child
  computes, such as `lower(t)`, spills: in one process, in each
  participant's table, and in a shared table
- **THEN** Tessera's node runs it, its plan shows the spill, and the
  rows are the core's
- **Verified by:**
  `test/sql/join.sql::A hashed key that is an expression its child computes`

#### Scenario: Keys left to the core
- **WHEN** a grouping or a join is by a key of `bit varying`, a join by
  `money`, or a join by `float4` equal to `float8`
- **THEN** the core's node runs it
- **Verified by:** `test/sql/types.sql::Keys left to the core`

### Requirement: The status of an error
A call SHALL return an error status, and never crash or loop without
end, when the bytes it is given are not a table, as "A damaged table"
of the spec of `relhash` says. Such an error, and a wrong argument,
SHALL be status `TESS_ERROR_INVALID_ARGUMENT` with SQLSTATE `XX000`. A
call SHALL check its arguments and its outputs before it changes
anything. A node that gives the table chunks it read back from its
files reports such an error, or a wrong argument of a call over them,
as damaged data instead, as "Damaged and foreign blocks are refused" of
the spec of `spill-format` says.

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

### Requirement: Merging groups read back
`tess_table_combine` SHALL merge the records of chunk `source`, from
byte `*from` on, into the table. Each record is a group: its keys, then
a payload of a word of flags, bit `i` set once aggregate `i` has a
value, and a word for each aggregate, at most 64 of them within the
table's payload. The record of the same keys in the table SHALL take
each state in as `combines[i]` says:

- `TESS_TABLE_COMBINE_COUNT` (1): the two add, whatever the flags;
- `TESS_TABLE_COMBINE_SUM` (2): the two add when both have a value; when
  only the incoming record has one, its value is taken; when only the
  table's has one, it stays;
- `TESS_TABLE_COMBINE_MIN` (3) and `TESS_TABLE_COMBINE_MAX` (4): the
  least or the greatest of two signed 64-bit values, by the flags as for
  a sum;
- the flag of each aggregate but a count is set when the incoming
  record has a value.

A count or a sum past the int8 range SHALL fail with status
`TESS_ERROR_INTEGER_OUT_OF_RANGE`, SQLSTATE `22003`, "bigint out of
range". A group the table lacks SHALL be copied whole to chunk `chunk`
and linked. `*from` SHALL move past each record merged or copied, and
`*merged` SHALL count them. The call SHALL stop before a new group when
`chunk` has no room, `*stop` being `TESS_TABLE_COMBINE_CHUNK_FULL` (1),
or when the linked records reach half the buckets,
`TESS_TABLE_COMBINE_INDEX_FULL` (2), and otherwise at the source's end,
`TESS_TABLE_COMBINE_DONE` (0). More than 64 aggregates, a payload too
small for them, an unknown kind of merge and a source that is `chunk`
SHALL be refused before any change. A merge that fails after these
checks leaves some states merged and some groups copied, and the caller
uses the table no more. The source is not linked: the caller keeps it
out of a regrow, which links every record of every chunk it is given.

#### Scenario: The states of a group merge
- **WHEN** groups of a count, a sum, a minimum and a maximum, with every
  combination of flags, are merged into a table that holds some of them
- **THEN** each state is as the rules give, and the groups the table
  lacked are copied and found
- **Verified by:**
  `crates/tessera-kernels/src/accumulate.rs::states_of_a_group_merge`;
  `test/sql/table.sql::tessera_test_table_combine`

#### Scenario: A merge stops for a chunk or a larger index
- **WHEN** the new groups of a source need more room than `chunk` has,
  and then more records than half the buckets
- **THEN** the call stops with `CHUNK_FULL`, then with `INDEX_FULL`, its
  `*from` on the first group not merged, and goes on from there with a
  new chunk and a larger index to `DONE`
- **Verified by:**
  `crates/relhash/src/table/tests.rs::a_merge_stops_for_a_chunk_or_a_larger_index`

#### Scenario: A merge that cannot be done fails
- **WHEN** a count or a sum would pass the int8 range, or a merge has 65
  aggregates, a payload too small, an unknown kind or a source that is
  `chunk`
- **THEN** the first fails with status
  `TESS_ERROR_INTEGER_OUT_OF_RANGE` and SQLSTATE `22003`, the others
  with `TESS_ERROR_INVALID_ARGUMENT` before any change
- **Verified by:**
  `crates/tessera-kernels/src/accumulate.rs::a_merged_count_past_the_int8_range_fails`;
  `crates/tessera-capi/tests/table.rs::a_merge_that_cannot_be_done_fails`

### Requirement: The C API over the spill of the crate
Each call of `tessera/table.h` over a build by several processes or over
the words of a spill SHALL do what the calls of the crate `relspill` do,
as the requirement of the spec of `relspill` named here says, and SHALL
return the crate's error as a status, as "The status of an error" says:

- `tess_build_counters_init`, which clears the counters before any
  participant attaches, `tess_build_take_chunk`, `tess_build_report`,
  `tess_build_add_duplicates` and `tess_build_totals`, which reads the
  duplicates only when asked for; `tess_build_step`, `tess_build_stop`
  and `tess_build_stopped`: "The phases of a shared build";
- `tess_round_step`, `tess_table_spill_records`,
  `tess_table_spill_start`, `tess_table_spill_take_file`,
  `tess_table_spill_take_alone` and `tess_table_spill_flags`, which
  reads whether a partition is on disk and whether one took it whole:
  "The rounds over a partition on disk";
- `tess_table_spill_words`, `tess_table_spill_init`, which clears the
  words and sets the budget in bytes, `tess_table_spill_partitions`,
  `tess_table_spill_split` and `tess_table_spill_add_bytes`, which adds
  bytes of no partition for a negative one: "The words of a spill";
- `tess_table_spill_evict`, `tess_table_spill_evictions` and
  `tess_table_spill_splits`: "Sending partitions to disk".

Beside them the C API SHALL check what the crate leaves to its caller.
Counters, words of a spill and stop words that are null or not aligned
to 8, words of a spill of another count than `tess_table_spill_words`
gives, and weights that are negative or not finite SHALL be refused,
and a call SHALL check every output before it writes any. A participant
is three 4-byte words of the caller's memory, zeroed before its first
step. A step's reply SHALL be the phase the barrier's attach gave,
after an attach; 1 when the barrier elected the participant or found
it the last to leave, else 0, after a wait or a leave. A participant
that leaves a shared table, or a round over a partition, while it
probes SHALL first mark the word their participants share with
`tess_build_stop`, and the last participant to leave SHALL read it with
`tess_build_stopped` and, when it is marked, return no record without a
mark.

#### Scenario: A build alone through C
- **WHEN** one participant numbers two chunks, reports three records
  with the NULL bits 0x5, adds two duplicates, and steps from its attach
  at the first phase to the free
- **THEN** the chunks are 0 and 1, the totals are three records, 0x5,
  two chunks and two duplicates, and its actions are those of a
  participant alone
- **Verified by:** `test/tessera_table_test.c::build_alone`

#### Scenario: Wrong words and outputs
- **WHEN** a call gets counters or words of a spill that are null or
  misaligned, a null output, weights that are not finite, or a phase or
  a state no step knows
- **THEN** it fails, SQLSTATE `XX000`, and no output and no word has
  changed
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_build_counters_refuse_null_and_misaligned_words`;
  `crates/tessera-capi/tests/table.rs::outputs_are_checked_before_anything_changes`;
  `crates/tessera-capi/tests/spill.rs::the_spill_words_refuse_null_and_misaligned_words`;
  `crates/tessera-capi/tests/spill.rs::weights_that_are_not_finite_are_refused`;
  `crates/tessera-capi/tests/table.rs::a_step_refuses_a_phase_or_a_state_it_does_not_know`

#### Scenario: The words through C
- **WHEN** the entry points choose partitions by weights, answer
  whether partitions split by room, bits and keys, and count the
  records of a partition past the split
- **THEN** they answer as the crate does, and the partition past the
  split is refused
- **Verified by:**
  `crates/tessera-capi/tests/spill.rs::the_spill_entry_points_choose_by_the_weights`;
  `crates/tessera-capi/tests/spill.rs::the_split_entry_point_weighs_room_bits_and_keys`;
  `crates/tessera-capi/tests/spill.rs::the_records_refuse_a_partition_past_the_split`

#### Scenario: A stop through C and under a node
- **WHEN** a participant asks to stop before it probes, while it probes
  and after it left; and a worker under the core's `Gather` stops at the
  `LIMIT` of a RIGHT join
- **THEN** only the stop while it probes is marked, and the join returns
  no row without a pair where every inner row has one
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::a_participant_marks_its_stop_only_while_it_probes`;
  `test/sql/join.sql::worker that has its share stops while it probes`

#### Scenario: A partition chosen again under a join
- **WHEN** a join, alone and with a shared table, spills with a weight
  of 10 for the partitions on disk and no limit a check
- **THEN** it frees what it holds of a partition chosen again, ends,
  and returns the core's rows
- **Verified by:**
  `test/sql/join.sql::A partition on disk chosen again`;
  `test/sql/join.sql::A shared table's partition on disk chosen again`


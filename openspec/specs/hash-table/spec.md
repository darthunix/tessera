# hash-table Specification

## Purpose
The C API over the hash table of the crate `relhash`, and what joins,
groupings and sorts keep beside the table: the forms of their keys,
the status of an error, the merge of groups read back, and the steps
and words of a build by several processes. When a node builds a table,
how large it makes its chunks and when it spills belong to the node's
own capability.

The table itself, its index, chunks and records, its partitions, its
Bloom filter and its marks, and what each of its calls promises, is in
the spec of [relhash](../relhash/spec.md). The requirements here start
with the C calls over it, the used mark of a chunk and a lookup in
place, then the SQL types of a key and the status of an error. Then the
merge of groups read back. Last, a build by several processes: its
counters, its phases, its rounds over partitions on disk, the words of
its spill, the partitions it sends to disk, and the partitions of a
level. [design.md](design.md) explains the whole and the reasons.

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
  `int8`, `timestamp` and `timestamptz` as kind 2, since their values
  are equal exactly when their Datums are;
- a number of 64 bits that stands for the value, as kind 2, for any
  other type whose equality is the type's default one and has a hash
  function, such as `text`, `numeric`, `float8` or `time`: in a
  grouping, the value's number in a dictionary of the node, which finds
  equal values by the type's own hash and equality; in a join, the
  value's hash from the type's own function, and the type's equality
  then decides every pair the table finds.

A key of a type without a hash function, such as `money`, and a join
by the equality of two types other than the integers, such as `float4`
with `float8`, SHALL leave the grouping or the join to the core's node.

#### Scenario: Keys whose values a word holds
- **WHEN** a join or a grouping has keys of `int2`, `date`, `bool`,
  `timestamp` or `timestamptz`, negative and NULL among them, or joins
  `int2` with `int4` and `int8`
- **THEN** Tessera's node runs it, and its rows are the core's
- **Verified by:**
  `test/sql/types.sql::Keys the table keeps in a word besides int4 and int8`

#### Scenario: Keys through a number for the value
- **WHEN** a grouping or a join has keys of `text`, `numeric` or `time`,
  with numeric 1.0 and 1.00, float8 -0 and 0, and text of either case
  under a case-insensitive collation
- **THEN** Tessera's node runs it, a join's plan shows the equality as
  its join filter, equal values meet though their bytes differ, and the
  rows are the core's
- **Verified by:**
  `test/sql/types.sql::Keys a word does not hold: text, numeric`;
  `test/sql/types.sql::Hash joins by keys a word does not hold`;
  `test/sql/types.sql::Equal values of other forms`;
  `test/sql/types.sql::Keys of other types of 8 bytes`

#### Scenario: A key through a number, on disk
- **WHEN** a join by a key of `text` that is an expression its child
  computes, such as `lower(t)`, spills: in one process, in each
  participant's table, and in a shared table
- **THEN** Tessera's node runs it, its plan shows the spill, and the
  rows are the core's
- **Verified by:**
  `test/sql/join.sql::A hashed key that is an expression its child computes`

#### Scenario: Keys left to the core
- **WHEN** a grouping or a join is by a key of `money`, or a join by
  `float4` equal to `float8`
- **THEN** the core's node runs it
- **Verified by:**
  `test/sql/types.sql::Keys of other types of 8 bytes`

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

### Requirement: The counters of a shared build
The participants of a shared build SHALL count it in four words of
memory they all map, aligned to 8, cleared by
`tess_build_counters_init` before any participant attaches:

```
 byte  0             8             16            24            32
       ┌─────────────┬─────────────┬─────────────┬─────────────┐
       │ records     │ NULL        │ chunks      │ duplicates  │
       │ appended    │ columns, a  │ numbered    │ the links   │
       │             │ bit a word  │             │ found       │
       └─────────────┴─────────────┴─────────────┴─────────────┘
```

`tess_build_take_chunk` SHALL give each call a chunk number no other
call of the build gets, from 0 up. `tess_build_report` SHALL add a
participant's records to word 0 and set its bits of NULL columns in
word 1. `tess_build_add_duplicates` SHALL add to word 3, and
`tess_build_totals` SHALL read the four, the duplicates only when asked
for. A count that would pass 2^64 - 1 SHALL be refused, and so SHALL
counters that are null or not aligned to 8; a call SHALL check every
output before it writes any.

#### Scenario: One participant
- **WHEN** one participant numbers two chunks, reports three records
  with the NULL bits 0x5, and adds two duplicates
- **THEN** the chunks are 0 and 1, and the totals are three records,
  0x5, two chunks and two duplicates
- **Verified by:**
  `test/tessera_table_test.c::build_alone`

#### Scenario: Several participants
- **WHEN** three participants number chunks, report records and NULL
  bits, and add duplicates at once
- **THEN** no number is given twice, the records and the duplicates are
  their sums, and the NULL bits their OR
- **Verified by:**
  `crates/tessera-kernels/src/phases.rs::participants_count_a_build_at_once`;
  `crates/tessera-kernels/src/loom.rs::three_participants_attach_at_any_phase`

#### Scenario: Wrong counters and outputs
- **WHEN** a call gets null or misaligned counters, a null output, or a
  count that passes 2^64 - 1
- **THEN** it fails, SQLSTATE `XX000`, and no output and no counter has
  changed
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_build_counters_refuse_null_and_misaligned_words`;
  `crates/tessera-capi/tests/table.rs::outputs_are_checked_before_anything_changes`;
  `crates/tessera-kernels/src/phases.rs::counts_past_their_words_are_refused`

### Requirement: The phases of a shared build
A participant of a shared build SHALL step through these phases, which
PostgreSQL's barrier separates, with `tess_build_step`, and never wait
itself: each step returns an action, the caller does it, and the
barrier's answer goes into the next step.

```
 0 BUILD   each participant appends its share of the inner rows
 1 FLUSH   each writes its chunks of the partitions on disk
 2 SIZE    the elected one makes the index; the others wait
 3 LINK    each links its own chunks into the index
 4 OUTER   each writes its share of the outer rows on disk
 5 PROBE   each probes, then leaves
 6 FREE    the last to leave frees the table
```

The participant is three 4-byte words of the caller's memory, zeroed
before its first step. A step's reply SHALL be the phase the barrier's
attach gave, after an attach; 1 when the barrier elected the participant
or found it the last to leave, else 0, after a wait or a leave. A
participant that attaches late SHALL join the phase the others are in:
at BUILD it appends what is left, at FLUSH and OUTER it writes its
share, at SIZE it waits, at LINK it links its own chunks, none, at PROBE
it probes, and at FREE it leaves at once. A participant in no known
state, or a phase past FREE after an attach, SHALL be refused.

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

#### Scenario: A participant alone
- **WHEN** one participant attaches at BUILD and is elected at SIZE and
  last at its leave
- **THEN** its actions are attach, build, wait, flush, wait, size,
  wait, link, wait, outer, wait, probe, leave and free
- **Verified by:**
  `crates/tessera-kernels/src/phases.rs::a_participant_alone_builds_flushes_sizes_links_and_probes`;
  `test/tessera_table_test.c::build_alone`

#### Scenario: A participant that attaches late
- **WHEN** a participant attaches at each phase from FLUSH to FREE
- **THEN** it does that phase's work, or waits at SIZE, or leaves at
  FREE, and then the phases after it
- **Verified by:**
  `crates/tessera-kernels/src/phases.rs::a_late_participant_joins_the_phase_the_others_are_in`

#### Scenario: A state or a phase unknown
- **WHEN** a participant holds a state no step makes, or an attach
  answers a phase past FREE
- **THEN** the step fails, SQLSTATE `XX000`
- **Verified by:**
  `crates/tessera-kernels/src/phases.rs::an_unknown_state_is_an_error`;
  `crates/tessera-kernels/src/phases.rs::an_attach_past_the_last_phase_is_an_error`;
  `crates/tessera-capi/tests/table.rs::a_step_refuses_a_phase_or_a_state_it_does_not_know`

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

### Requirement: The rounds over a partition on disk
After a shared build that spilled, each partition on disk SHALL be a
round of its own, with a barrier of its own and these phases, stepped
by `tess_round_step` as a build is:

```
 0 ELECT      all wait
 1 ALLOCATE   the elected one makes the partition's index
 2 LOAD       each loads the files it takes, and links them
 3 PROBE      each probes, then leaves
 4 FREE       the last to leave frees the partition
```

A participant that attaches late SHALL load the files still left at
LOAD, probe at PROBE, and leave at once at FREE. Each file of a
partition SHALL be taken by one participant. A round's state or phase
outside these SHALL be refused.

`tess_table_spill_records` SHALL add to a partition's records and read
them. `tess_table_spill_start` SHALL give each participant the
partition to start its rounds at, spread over the partitions.
`tess_table_spill_take_file` SHALL give each number of a partition's
inner or outer files to one call, and `tess_table_spill_take_alone`
SHALL take a partition whole for one participant only.
`tess_table_spill_flags` SHALL read whether a partition is on disk and
whether one took it whole. A partition past the partitions in force
SHALL be refused, and so SHALL records past 2^64 - 1 and a file number
past 2^32 - 1.

#### Scenario: Rounds of several participants
- **WHEN** two or three participants join a round at any phase, load
  its files and probe it
- **THEN** every file is loaded once, every key is found, and the round
  is freed once
- **Verified by:**
  `crates/tessera-kernels/src/loom.rs::two_participants_load_a_partition_and_probe_it`;
  `crates/tessera-kernels/src/loom.rs::three_participants_join_a_round_at_any_phase`

#### Scenario: A round alone and late
- **WHEN** one participant attaches at ELECT, at ALLOCATE, at LOAD, at
  PROBE and at FREE
- **THEN** its actions follow the phases from the one it attached at
- **Verified by:**
  `crates/tessera-kernels/src/phases.rs::a_round_elects_allocates_loads_probes_and_frees`

#### Scenario: Taken once
- **WHEN** two participants take the files of a partition, and take it
  whole, at once, and participants ask where to start their rounds
- **THEN** each file goes to one of them, the partition to one, and the
  starts go round the partitions; a partition past those in force, or a
  file number past 2^32 - 1, is refused
- **Verified by:**
  `crates/tessera-kernels/src/loom.rs::files_and_a_whole_partition_go_to_one_participant_each`;
  `crates/tessera-kernels/src/shared_spill.rs::files_and_partitions_are_taken_once`;
  `crates/tessera-kernels/src/shared_spill.rs::counts_past_their_words_are_refused`

### Requirement: The words of a spill
A table that spills SHALL keep its decisions in words: a process's own
spill in its own memory, a shared table's in memory every participant
maps, aligned to 8. `tess_table_spill_words` SHALL give their count for
1 to 65536 partitions and refuse any other, and
`tess_table_spill_init` SHALL clear them and set the budget in bytes.
A call SHALL refuse words of another count or alignment. The words of
a process and the shared words SHALL take the same steps, the second
by atomic operations.

```
 word  0            1        2        3        4
     ┌────────────┬────────┬────────┬────────┬───────────┐
     │ partitions │ bytes  │ budget │ start  │ evictions │   the head
     │ in force   │ in all │        │        │           │
     └────────────┴────────┴────────┴────────┴───────────┘
 then, for each slot p of 0 .. capacity, at word 5 + 5 p:
     ┌────────────┬────────┬────────┬────────┬───────────┐
     │ bytes      │ records│ flags  │ next   │ next      │
     │            │        │ 1 disk │ inner  │ outer     │
     │            │        │ 2 alone│ file   │ file      │
     └────────────┴────────┴────────┴────────┴───────────┘
 the slot whose number is the partitions in force counts the outer
 files of the partitions kept in memory
```

`tess_table_spill_split` SHALL split the table into a power of two of
partitions, up to the words' capacity, unless a participant split it
before: the first split holds, and every call SHALL return the
partitions in force. `tess_table_spill_add_bytes` SHALL add the bytes
of a partition's chunks, or of none for a negative partition before the
split, to that partition and to the total, and answer whether the
total passes the budget; a total equal to it does not. A count of
bytes SHALL never go below zero, nor past 2^64 - 1: such a call fails.

#### Scenario: The count of words
- **WHEN** the words are sized for 1, 4 and 65536 partitions, and for 0
  and 65537, and words of another count or misaligned are given
- **THEN** the first three are 15, 30 and 327690 words, and the others
  are refused
- **Verified by:**
  `crates/tessera-kernels/src/shared_spill.rs::the_words_are_sized_for_1_to_65536_partitions`;
  `crates/tessera-kernels/src/shared_spill.rs::words_attach_only_as_words_for_sizes_them`;
  `crates/tessera-kernels/src/shared_spill.rs::counts_past_their_words_are_refused`

#### Scenario: Own and shared words
- **WHEN** the same steps run over a process's own words and over shared
  ones
- **THEN** every answer is the same
- **Verified by:**
  `crates/tessera-kernels/src/shared_spill.rs::local_words_choose_as_shared_ones_do`;
  `crates/tessera-capi/tests/spill.rs::the_spill_entry_points_choose_by_the_weights`

#### Scenario: The first split holds
- **WHEN** two participants past the budget split the table at once,
  into 4 and into 8 partitions
- **THEN** the first split holds for both, and a count not a power of
  two is refused
- **Verified by:**
  `crates/tessera-kernels/src/loom.rs::participants_past_the_budget_agree_on_one_split`;
  `crates/tessera-kernels/src/shared_spill.rs::the_first_split_holds_and_the_largest_partition_goes_to_disk`

#### Scenario: The budget
- **WHEN** bytes up to the budget, past it, below zero and past 2^64 - 1
  are added
- **THEN** the first is not over, the second is, and the others fail
- **Verified by:**
  `crates/tessera-kernels/src/shared_spill.rs::a_total_equal_to_the_budget_is_not_over`;
  `crates/tessera-kernels/src/shared_spill.rs::a_counter_never_goes_below_zero`;
  `crates/tessera-kernels/src/shared_spill.rs::counts_past_their_words_are_refused`

### Requirement: Sending partitions to disk
`tess_table_spill_evict` SHALL choose the next partition to send to
disk by weights the caller gives, and mark it on disk:

- while the bytes held, with `reserve` bytes for each partition on
  disk, pass `start` of the limit for a check's first partition and
  `target` for each after it, the partition with the most bytes,
  those already on disk weighed by `spilled` (0 leaves them out);
- then, with a partition on disk and those in memory holding fewer
  than `resident` of the records, the lowest partition in memory;
- at most `per_check` partitions a check, 0 for any.

A shared table SHALL weigh the bytes its words count against their
budget, a process's own spill the bytes it measured against its limit.
A partition SHALL be counted once in `tess_table_spill_evictions`, by
the participant whose mark set its flag; one already on disk SHALL be
chosen again when it holds the most, and its holder SHALL then free
what it holds of it. Weights that are negative or not finite SHALL be
refused.

`tess_table_spill_splits` SHALL answer whether a partition read back
splits into a level below by the next bits of the hash: when its size
passes `room` of what the limit leaves beside the bytes in use, two
bits are left past those its level's partitions take, and, with a
`key` share above 0, it holds fewer than that share of its level's
rows; more is one key, which no split parts. `room` and `key` that are
negative or not finite, and bits past 32, SHALL be refused.

#### Scenario: The weights
- **WHEN** a grouping's weights and a join's choose partitions while
  their memory goes down, with a reserve, a share of records, and a
  limit of partitions a check
- **THEN** the grouping sends from its start down to its target, the
  join keeps room for its tails and a quarter of the records in memory,
  no check passes its limit, and weights below zero or not finite are
  refused
- **Verified by:**
  `crates/tessera-kernels/src/shared_spill.rs::a_grouping_goes_from_its_start_down_to_its_target`;
  `crates/tessera-kernels/src/shared_spill.rs::a_join_keeps_reserve_for_its_tails_and_a_quarter_in_memory`;
  `crates/tessera-kernels/src/shared_spill.rs::the_first_split_holds_and_the_largest_partition_goes_to_disk`;
  `crates/tessera-capi/tests/spill.rs::weights_that_are_not_finite_are_refused`

#### Scenario: Marked once
- **WHEN** two participants send the largest partition at once
- **THEN** it is marked and counted once
- **Verified by:**
  `crates/tessera-kernels/src/loom.rs::a_partition_goes_to_disk_once`

#### Scenario: A partition chosen again
- **WHEN** a join, alone and with a shared table, spills with a weight
  of 10 for the partitions on disk and no limit a check
- **THEN** it frees what it holds of a partition chosen again, ends,
  and returns the core's rows
- **Verified by:**
  `crates/tessera-kernels/src/shared_spill.rs::a_grouping_goes_from_its_start_down_to_its_target`;
  `test/sql/join.sql::A partition on disk chosen again`;
  `test/sql/join.sql::A shared table's partition on disk chosen again`

#### Scenario: Room, bits and keys
- **WHEN** partitions of sizes on both sides of the room, with two
  bits left and one, and with a share of rows on both sides of `key`,
  are asked about
- **THEN** only the one past the room, with bits left and below the
  share, splits, and bits past 32 are refused
- **Verified by:**
  `crates/tessera-kernels/src/shared_spill.rs::a_partition_splits_past_its_room_with_bits_left_and_many_keys`;
  `crates/tessera-kernels/src/shared_spill.rs::a_level_past_the_bits_of_the_hash_is_refused`;
  `crates/tessera-capi/tests/spill.rs::the_split_entry_point_weighs_room_bits_and_keys`

### Requirement: The partitions of a level
`tess_spill_partitions` SHALL give the number of partitions of a level
of a table that spills. It SHALL start from `min_partitions` and double
the number while all of these hold:

- the number is below `max_partitions`;
- the number is below `at_least`, or the partitions, each holding half
  of `limit`, hold fewer bytes than `expected`;
- twice the number times `reserve`, what each partition keeps in memory
  at the least, fits in half of `limit`;
- the bits of the doubled number, after the `shift` bits the levels
  above take, stay below 32.

`min_partitions` and `max_partitions` SHALL be powers of two, the first
no larger. Expected bytes below zero or not a number SHALL be refused,
and so SHALL a `shift` past which `min_partitions` do not fit in the 32
bits of the hash.

`tess_spill_chunk_len` SHALL give the length of a level's chunks:
`limit` divided by `share` times the partitions, no more than
`max_chunk`, then no less than `min_chunk`, which wins over
`max_chunk`, rounded down to a multiple of 8. A `share` or a number of
partitions of 0, and a `min_chunk` below 8 or not a multiple of 8,
SHALL be refused.

#### Scenario: By the bytes, the participants and the reserve
- **WHEN** a level of 4 to 1024 partitions with a reserve of 48 KiB
  expects 10 MiB in a limit of 4 MiB, then wants 16 partitions at
  least, then has a limit of 256 KiB
- **THEN** it gets 8 partitions, then 16, then 4
- **Verified by:**
  `crates/tessera-spill/src/plan.rs::a_level_doubles_by_its_bytes_participants_and_reserve`

#### Scenario: By the bits of the hash
- **WHEN** a level of 4 to 1024 partitions that expects far more than
  its limit comes after 28, 29, 30 and 31 bits
- **THEN** it gets 8, 4 and 4 partitions, and the last is refused
- **Verified by:**
  `crates/tessera-spill/src/plan.rs::a_level_keeps_to_the_bits_of_the_hash`;
  `crates/tessera-capi/tests/spill.rs::the_plan_of_a_level_refuses_what_no_node_gives`

#### Scenario: Wrong bounds and bytes
- **WHEN** the least or the most partitions are not powers of two or
  are out of order, or the expected bytes are below zero or not a
  number
- **THEN** the call fails, SQLSTATE `XX000`
- **Verified by:**
  `crates/tessera-spill/src/plan.rs::a_plan_refuses_bounds_out_of_order`;
  `crates/tessera-capi/tests/spill.rs::the_plan_of_a_level_refuses_what_no_node_gives`

#### Scenario: A share of the limit
- **WHEN** at a share of 16, with chunks of 8 KiB to 1 MiB, 4 MiB is
  shared among 8 partitions and among 1024, and 1 GiB among 4; and
  1000000 bytes among 8, with chunks of 8 bytes to 1 MiB
- **THEN** the chunks are 32 KiB, 8 KiB and 1 MiB, and 7808 bytes
- **Verified by:**
  `crates/tessera-spill/src/plan.rs::a_chunk_is_a_share_of_the_limit_within_its_bounds`

#### Scenario: Wrong shares and bounds
- **WHEN** the share or the partitions are 0, or the least chunk is 4
  or 4100 bytes
- **THEN** the call fails, SQLSTATE `XX000`
- **Verified by:**
  `crates/tessera-spill/src/plan.rs::a_plan_refuses_bounds_out_of_order`;
  `crates/tessera-capi/tests/spill.rs::the_plan_of_a_level_refuses_what_no_node_gives`

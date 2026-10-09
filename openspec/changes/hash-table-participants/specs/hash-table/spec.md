## ADDED Requirements

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
  `crates/tessera-kernels/src/table/phases.rs::participants_count_a_build_at_once`

#### Scenario: Wrong counters and outputs
- **WHEN** a call gets null or misaligned counters, a null output, or a
  count that passes 2^64 - 1
- **THEN** it fails, SQLSTATE `XX000`, and no output and no counter has
  changed
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_build_counters_refuse_null_and_misaligned_words`;
  `crates/tessera-capi/tests/table.rs::outputs_are_checked_before_anything_changes`;
  `crates/tessera-kernels/src/table/phases.rs::counts_past_their_words_are_refused`

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

#### Scenario: A participant alone
- **WHEN** one participant attaches at BUILD and is elected at SIZE and
  last at its leave
- **THEN** its actions are attach, build, wait, flush, wait, size,
  wait, link, wait, outer, wait, probe, leave and free
- **Verified by:**
  `crates/tessera-kernels/src/table/phases.rs::a_participant_alone_builds_flushes_sizes_links_and_probes`;
  `test/tessera_table_test.c::build_alone`

#### Scenario: A participant that attaches late
- **WHEN** a participant attaches at each phase from FLUSH to FREE
- **THEN** it does that phase's work, or waits at SIZE, or leaves at
  FREE, and then the phases after it
- **Verified by:**
  `crates/tessera-kernels/src/table/phases.rs::a_late_participant_joins_the_phase_the_others_are_in`

#### Scenario: A state or a phase unknown
- **WHEN** a participant holds a state no step makes, or an attach
  answers a phase past FREE
- **THEN** the step fails, SQLSTATE `XX000`
- **Verified by:**
  `crates/tessera-kernels/src/table/phases.rs::an_unknown_state_is_an_error`;
  `crates/tessera-kernels/src/table/phases.rs::an_attach_past_the_last_phase_is_an_error`;
  `crates/tessera-capi/tests/table.rs::a_step_refuses_a_phase_or_a_state_it_does_not_know`

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

#### Scenario: Rounds of several participants
- **WHEN** two or three participants join a round at any phase, load
  its files and probe it
- **THEN** every file is loaded once, every key is found, and the round
  is freed once
- **Verified by:**
  `crates/tessera-kernels/src/table/loom.rs::two_participants_load_a_partition_and_probe_it`;
  `crates/tessera-kernels/src/table/loom.rs::three_participants_join_a_round_at_any_phase`

#### Scenario: A round alone and late
- **WHEN** one participant attaches at ELECT, at ALLOCATE, at LOAD, at
  PROBE and at FREE
- **THEN** its actions follow the phases from the one it attached at
- **Verified by:**
  `crates/tessera-kernels/src/table/phases.rs::a_round_elects_allocates_loads_probes_and_frees`

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

#### Scenario: The count of words
- **WHEN** the words are sized for 1, 4 and 65536 partitions, and for 0
  and 65537, and words of another count or misaligned are given
- **THEN** the first three are 15, 30 and 327690 words, and the others
  are refused
- **Verified by:**
  `crates/tessera-kernels/src/table/shared_spill.rs::the_words_are_sized_for_1_to_65536_partitions`;
  `crates/tessera-kernels/src/table/shared_spill.rs::words_attach_only_as_words_for_sizes_them`;
  `crates/tessera-kernels/src/table/shared_spill.rs::counts_past_their_words_are_refused`

#### Scenario: Own and shared words
- **WHEN** the same steps run over a process's own words and over shared
  ones
- **THEN** every answer is the same
- **Verified by:**
  `crates/tessera-kernels/src/table/shared_spill.rs::local_words_choose_as_shared_ones_do`;
  `crates/tessera-capi/tests/spill.rs::the_spill_entry_points_choose_by_the_weights`

### Requirement: Splitting a table once
`tess_table_spill_split` SHALL split the table into a power of two of
partitions, up to the words' capacity, unless a participant split it
before: the first split holds, and every call SHALL return the
partitions in force. `tess_table_spill_add_bytes` SHALL add the bytes
of a partition's chunks, or of none for a negative partition before the
split, to that partition and to the total, and answer whether the
total passes the budget; a total equal to it does not. A count of
bytes SHALL never go below zero, nor past 2^64 - 1: such a call fails.

#### Scenario: The first split holds
- **WHEN** two participants past the budget split the table at once,
  into 4 and into 8 partitions
- **THEN** the first split holds for both, and a count not a power of
  two is refused
- **Verified by:**
  `crates/tessera-kernels/src/table/loom.rs::participants_past_the_budget_agree_on_one_split`;
  `crates/tessera-kernels/src/table/shared_spill.rs::the_first_split_holds_and_the_largest_partition_goes_to_disk`

#### Scenario: The budget
- **WHEN** bytes up to the budget, past it, below zero and past 2^64 - 1
  are added
- **THEN** the first is not over, the second is, and the others fail
- **Verified by:**
  `crates/tessera-kernels/src/table/shared_spill.rs::a_total_equal_to_the_budget_is_not_over`;
  `crates/tessera-kernels/src/table/shared_spill.rs::a_counter_never_goes_below_zero`;
  `crates/tessera-kernels/src/table/shared_spill.rs::counts_past_their_words_are_refused`

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

#### Scenario: The weights
- **WHEN** a grouping's weights and a join's choose partitions while
  their memory goes down, with a reserve, a share of records, and a
  limit of partitions a check
- **THEN** the grouping sends from its start down to its target, the
  join keeps room for its tails and a quarter of the records in memory,
  no check passes its limit, and weights below zero or not finite are
  refused
- **Verified by:**
  `crates/tessera-kernels/src/table/shared_spill.rs::a_grouping_goes_from_its_start_down_to_its_target`;
  `crates/tessera-kernels/src/table/shared_spill.rs::a_join_keeps_reserve_for_its_tails_and_a_quarter_in_memory`;
  `crates/tessera-kernels/src/table/shared_spill.rs::the_first_split_holds_and_the_largest_partition_goes_to_disk`;
  `crates/tessera-capi/tests/spill.rs::weights_that_are_not_finite_are_refused`

#### Scenario: Marked once
- **WHEN** two participants send the largest partition at once
- **THEN** it is marked and counted once
- **Verified by:**
  `crates/tessera-kernels/src/table/loom.rs::a_partition_goes_to_disk_once`

#### Scenario: A partition chosen again
- **WHEN** a join, alone and with a shared table, spills with a weight
  of 10 for the partitions on disk and no limit a check
- **THEN** it frees what it holds of a partition chosen again, ends,
  and returns the core's rows
- **Verified by:**
  `crates/tessera-kernels/src/table/shared_spill.rs::a_grouping_goes_from_its_start_down_to_its_target`;
  `test/sql/join.sql::A partition on disk chosen again`;
  `test/sql/join.sql::A shared table's partition on disk chosen again`

### Requirement: Records, starts and files of the rounds
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

#### Scenario: Taken once
- **WHEN** two participants take the files of a partition, and take it
  whole, at once, and participants ask where to start their rounds
- **THEN** each file goes to one of them, the partition to one, and the
  starts go round the partitions; a partition past those in force, or a
  file number past 2^32 - 1, is refused
- **Verified by:**
  `crates/tessera-kernels/src/table/loom.rs::files_and_a_whole_partition_go_to_one_participant_each`;
  `crates/tessera-kernels/src/table/shared_spill.rs::files_and_partitions_are_taken_once`;
  `crates/tessera-kernels/src/table/shared_spill.rs::counts_past_their_words_are_refused`

### Requirement: A partition read back that splits
`tess_table_spill_splits` SHALL answer whether a partition read back
splits into a level below by the next bits of the hash: when its size
passes `room` of what the limit leaves beside the bytes in use, two
bits are left past those its level's partitions take, and, with a
`key` share above 0, it holds fewer than that share of its level's
rows; more is one key, which no split parts. `room` and `key` that are
negative or not finite, and bits past 32, SHALL be refused.

#### Scenario: Room, bits and keys
- **WHEN** partitions of sizes on both sides of the room, with two
  bits left and one, and with a share of rows on both sides of `key`,
  are asked about
- **THEN** only the one past the room, with bits left and below the
  share, splits, and bits past 32 are refused
- **Verified by:**
  `crates/tessera-kernels/src/table/shared_spill.rs::a_partition_splits_past_its_room_with_bits_left_and_many_keys`;
  `crates/tessera-kernels/src/table/shared_spill.rs::a_level_past_the_bits_of_the_hash_is_refused`;
  `crates/tessera-capi/tests/spill.rs::the_split_entry_point_weighs_room_bits_and_keys`

## MODIFIED Requirements

### Requirement: Calls at the same time
Over shared memory several processes SHALL be able to append at once,
each to chunks of its own, to link at once, each its own chunks, and to
probe and read at once. A chunk SHALL have one writer, and the calls of
one writer run alone. Of the index and the chunks, only the count of
records and the buckets change while the table is shared, by atomic
operations: a link adds to the count before it publishes a record, and
publishes it by a compare-and-swap of its bucket with release; a probe
reads a bucket with acquire. A process that finds a record SHALL see
it whole. The counters of a build, the words of a spill and the marks
change by atomic operations too, and a barrier orders what one
participant did before the next phase of the others.

#### Scenario: A probe beside a link
- **WHEN** one process probes for a key while another links its record
- **THEN** in every order of their steps the probe finds the record
  whole or not at all, and its chain is never taken for a loop
- **Verified by:**
  `crates/tessera-kernels/src/table/loom.rs::a_probe_sees_a_published_record_whole`

#### Scenario: Weaker orders are caught
- **WHEN** the model reads the buckets with relaxed order, or raises the
  count of records only after it publishes a record
- **THEN** a probe can read a record that is not written yet, or take a
  sound chain for a loop, and the model reports it
- **Verified by:**
  `crates/tessera-kernels/src/table/loom.rs::relaxed_heads_let_a_probe_read_an_unwritten_record`;
  `crates/tessera-kernels/src/table/loom.rs::a_count_raised_after_publishing_makes_a_sound_chain_a_loop`

#### Scenario: A shared build
- **WHEN** two or three processes append, size the index, link and
  probe one table, joining at any phase
- **THEN** every key is found
- **Verified by:**
  `crates/tessera-kernels/src/table/loom.rs::two_participants_append_size_link_and_probe`;
  `crates/tessera-kernels/src/table/loom.rs::three_participants_attach_at_any_phase`

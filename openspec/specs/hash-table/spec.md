# hash-table Specification

## Purpose
The hash table that joins, groupings and sorts keep their rows in: its
format in memory, local or shared between processes, and what each call
over it promises. When a node builds a table, how large it makes its
chunks and when it spills belong to the node's own capability.

A table is an index and chunks. The index is a header and an array of
buckets; the chunks hold the records, one after another. A record is
one row: its hash, a link to the next record of its bucket's chain, its
keys and a payload the table does not look into. A reference names a
record by its chunk and its place there, so the bytes mean the same in
every process. The requirements go from the bytes to the calls: the
index, chunks and references, a record and the SQL types of its keys,
the errors of a call, then appending, linking, looking rows up, the
records of a key together, the calls of one writer, and the calls that
may run at once.
[design.md](design.md) explains the whole and the reasons.

## Requirements

### Requirement: The index of a table
A table SHALL have one index: a header of 96 bytes followed by the
buckets, 4 bytes each. Every field is an integer in the machine's byte
order.

```
 byte    0           4           8           12          16
         ┌───────────────────────┬───────────┬───────────┐
       0 │ magic                 │ version   │ header    │
         │                       │           │ size      │
         ├───────────────────────┼───────────┴───────────┤
      16 │ index length          │ buckets offset        │
         ├───────────────────────┼───────────────────────┤
      32 │ reserved              │ records               │
         ├───────────┬───────────┼───────────┬───────────┤
      48 │ buckets   │ bucket    │ record    │ payload   │
         │           │ shift     │ size      │ size      │
         ├───────────┼───────────┼───────────┴───────────┤
      64 │ keys      │ flags     │ kinds, one byte a key │
         ├───────────┴───────────┼───────────────────────┤
      80 │ kinds (cont.)         │ reserved              │
         ├───────────────────────┴───────────────────────┤
      96 │ bucket 0, bucket 1, …  4 bytes each           │
         └───────────────────────────────────────────────┘
```

- magic: a number of 64 bits whose bytes are `TESSTABL` on a
  little-endian machine;
- version: the version of the format, 1, which
  `tess_table_format_version()` returns;
- header size: 96; buckets offset: 96, where the buckets start;
- index length: the bytes the index was made in, a multiple of 8;
- records: the records linked into the buckets;
- buckets: a power of two, at least 1024; bucket shift: 32 less the
  logarithm of the buckets, so that the bucket of a hash is its high
  bits, `hash >> shift`;
- record size, payload size, keys and kinds: see "A record";
- flags and both reserved words: 0.

For a capacity of `n` records, `tess_table_size` SHALL return
`96 + 4 * buckets`, where buckets is the least power of two that is at
least 1024 and at least `2 * n`, and at most 2^31.
`tess_table_create` SHALL make the index of an empty table in a block
aligned to 8 whose length is a multiple of 8 and at least that size:
the header as above, no record, every bucket 0. `tess_table_stats`
SHALL report the records, the buckets, the bytes of the header and the
buckets, and the index length.

#### Scenario: The size of an index
- **WHEN** an index is sized for capacities from 0 to 100000
- **THEN** it takes the header and a power of two of buckets, at least
  1024 and at least twice the capacity
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::index_size_covers_header_and_buckets`;
  `test/sql/table.sql::tessera_test_table_regrow`

#### Scenario: A new index is an empty table
- **WHEN** an index is created and attached again
- **THEN** its stats show no record, the buckets of its capacity and
  its length, and its keys and payload are those it was created with
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::a_created_table_is_empty_and_attaches_again`;
  `crates/tessera-capi/tests/table.rs::the_entry_points_round_trip`

#### Scenario: An index in the wrong block is refused
- **WHEN** an index is created at an address not aligned to 8, in a
  length that is not a multiple of 8, or in fewer bytes than its size
- **THEN** the call fails and writes nothing
- **Verified by:**
  `crates/tessera-kernels/src/table/mod.rs::misaligned_or_odd_blocks_are_refused`

### Requirement: Chunks and references
The records of a table SHALL lie in chunks, at most 32768 of them,
numbered from 0. A chunk is a block aligned to 8 of 8 bytes to 1 MiB, a
multiple of 8. Its first 8 bytes are its used mark, the bytes its
records take with the mark's own 8; the records follow one after
another from byte 8. `tess_table_chunk_init` SHALL make a block an
empty chunk, its used mark 8.

A record SHALL be named by a reference of 32 bits: the number of its
chunk and its first byte there, in units of 8 bytes. Reference 0, the
used mark of chunk 0, names no record. `tess_table_ref`,
`tess_table_ref_chunk` and `tess_table_ref_byte` make and split a
reference, and a reference names the same record in every process.

```
  31             17 16                     0
 ┌─────────────────┬────────────────────────┐
 │ chunk (15 bits) │ byte / 8 (17 bits)     │
 └─────────────────┴────────────────────────┘
   the record is at chunks[chunk] + 8 * (byte / 8)
```

#### Scenario: Chunks past the limits are refused
- **WHEN** a call is given a chunk not aligned to 8, of a length that
  is not a multiple of 8, shorter than 8 bytes or longer than 1 MiB, or
  more than 32768 chunks
- **THEN** the call fails
- **Verified by:**
  `crates/tessera-kernels/src/table/mod.rs::chunks_past_the_limits_are_refused`;
  `crates/tessera-kernels/src/table/mod.rs::misaligned_or_odd_blocks_are_refused`

#### Scenario: A reference names its chunk and its place
- **WHEN** rows fill one chunk and go on into the next
- **THEN** each row's reference names the chunk and the byte of its
  record, and the helpers of the C API read them back
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::inserted_rows_are_found_and_absent_keys_are_not`;
  `test/sql/table.sql::tessera_test_table_cycle`

#### Scenario: Reference 0 names no record
- **WHEN** a call is asked for the record at reference 0
- **THEN** the call fails, and an empty bucket or the end of a chain
  holds 0
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::reference_zero_names_no_record`

### Requirement: A record
Every record of a table SHALL have the size in the header: a header of
16 bytes, a slot of 8 bytes for each key, then the payload, padded with
zeros to a multiple of 8. A table SHALL have 1 to 16 keys, each of kind
1, an `int4` widened with its sign into its slot, or kind 2, an `int8`.
A NULL key SHALL hold 0 in its slot and set its bit of the NULL bits.
`tess_table_record_size` SHALL return the size of a record of a number
of keys and a payload, the size `tess_table_record_bytes` computes, and
SHALL refuse a record that does not fit in a chunk.

```
 byte 0       4       8            12      16       24
      ┌───────┬───────┬────────────┬───────┬────────┬───┬─────────┐
      │ hash  │ next  │ NULL bits  │ size  │ key 0  │ … │ payload │
      │       │       │ bit k: key │ / 8   │ 8 bytes│   │ padded  │
      │       │       │ k is NULL  │       │        │   │ to 8    │
      └───────┴───────┴────────────┴───────┴────────┴───┴─────────┘
```

- hash: the hash the row came with;
- next: the reference of the next record of the chain, 0 at its end;
- size / 8: the record's size in units of 8 bytes.

#### Scenario: The size of a record
- **WHEN** the size of a record is asked for 1 to 16 keys and payloads
  of 0 to 1024 bytes
- **THEN** it is 16 bytes, 8 a key and the payload, rounded up to 8,
  the same in the kernels and in the C header; 0 and 17 keys are
  refused
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::a_record_takes_its_header_its_key_slots_and_its_payload`;
  `test/sql/table.sql::tessera_test_table_record_size`

#### Scenario: A record as the C API shows it
- **WHEN** a record is read through `tess_table_record`
- **THEN** it has the row's hash, an `int4` key widened with its sign, a
  NULL key as 0 with its bit set, and the row's payload
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_entry_points_round_trip`;
  `crates/tessera-kernels/tests/table.rs::a_null_key_groups_apart_from_the_value_it_hashes_like`

#### Scenario: A record larger than a chunk
- **WHEN** a table is sized or created for a record that does not fit
  in a chunk of 1 MiB
- **THEN** the call fails, and the largest record that fits is accepted
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::a_record_larger_than_a_chunk_is_refused`

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

#### Scenario: Keys left to the core
- **WHEN** a grouping or a join is by a key of `money`, or a join by
  `float4` equal to `float8`
- **THEN** the core's node runs it
- **Verified by:**
  `test/sql/types.sql::Keys of other types of 8 bytes`

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
its arguments, the header and the list of chunks before it changes
anything.

#### Scenario: A damaged header
- **WHEN** one field of a valid header is changed: the magic, the
  version, a length or offset, the buckets or their shift, the record
  size, the keys or a kind, the flags or a reserved word
- **THEN** every call over the table fails, and the SQLSTATE is `XX000`
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::a_corrupt_header_is_refused`;
  `test/sql/table.sql::tessera_test_table_errors`

#### Scenario: A damaged reference or chain
- **WHEN** a bucket points into a missing chunk, past its chunk or into
  a record, a record's next points to itself, or a used mark ends no
  record, also with a count of records far past what the chunks hold
- **THEN** the probe, the walk or the link fails
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::a_corrupt_reference_chain_or_used_mark_is_an_error`;
  `crates/tessera-kernels/tests/table.rs::a_word_probed_at_once_detects_a_cycle`;
  `crates/tessera-kernels/src/table/mod.rs::a_damaged_count_is_bounded_by_the_places_of_the_chunks`

#### Scenario: Arguments are checked first
- **WHEN** a call is given arrays of the wrong length, a missing output
  or a wrong key count
- **THEN** it fails, and the table, the chunks, the masks and the cursor
  are as they were
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::dimension_errors_come_before_any_change`;
  `crates/tessera-capi/tests/table.rs::outputs_are_checked_before_anything_changes`

### Requirement: Appending rows
`tess_table_append` SHALL write the rows of the pending mask, in row
order, as records into the chunk it names, while a whole record fits.
Each row written SHALL leave the mask and get the reference of its
record; the others stay. A record SHALL take the row's hash, keys and
payload, or a payload of zeros when none is given. Append SHALL not
need the index, which may be absent; when the table has one, append
SHALL check it as every call does, and SHALL refuse keys or a payload
size other than the table's before it writes anything. Append SHALL
not link the records: they are not counted or found until linked.
`tess_table_append_columns`
SHALL take the payload from up to 2048 columns, without allocating
memory: words of NULL bits, bit `c % 64` of word `c / 64` for column `c`
and at least one word, then a word for each column, 0 for a NULL.

#### Scenario: A full chunk leaves rows pending
- **WHEN** a batch of 100 rows is appended to a chunk with room for 16
- **THEN** the first 16 rows get records and leave the mask, the others
  stay, nothing is found before a link, and the rest go into the next
  chunk from byte 8
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::a_full_chunk_leaves_the_rest_pending_until_linked_elsewhere`

#### Scenario: The payload of a record
- **WHEN** rows are appended with no payload, or with their payload in
  columns, of 3, 64, 65, 130 and 2048 columns, some of them NULL
- **THEN** the payload is zeros, or the words of NULL bits and a word a
  column, the same as an array of those words gives, and the entry point
  allocates no memory
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::a_missing_payload_is_zeros`;
  `crates/tessera-kernels/tests/table.rs::payload_columns_append_the_records_a_payload_array_does`;
  `crates/tessera-capi/tests/table.rs::the_columns_entry_point_writes_each_payload_word`;
  `crates/tessera-capi/tests/table.rs::payload_columns_append_without_allocating`

#### Scenario: An append checks the table's index
- **WHEN** rows are appended to a table with an index, with another
  payload size or another kind of key, from an array or from columns,
  and then with the table's own; and rows are appended without an index
- **THEN** with the index the wrong ones fail, write nothing and leave
  every row pending, and the table's own go in; without an index the
  records that the arguments describe go in
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::an_append_refuses_records_that_are_not_its_table_s`

### Requirement: Linking records
`tess_table_link` SHALL link the records of a chunk from a byte at a
record's start up to the used mark, each first in its bucket's chain,
and SHALL move the byte past them. Records of equal keys SHALL stay
separate records. The count of records in the header SHALL grow before
any of them can be found. With duplicates asked, the call SHALL count
the records whose hash, NULL bits and keys a record linked earlier
already has; the sum of these counts over the processes that link at
once SHALL be exact.

#### Scenario: Linking counts what was there
- **WHEN** a chunk is linked in two parts, and then again from its end
- **THEN** each part links its records and counts its duplicates, and
  the last call links none
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::linking_counts_the_records_whose_keys_were_there_already`

#### Scenario: Several processes link at once
- **WHEN** two or three processes link chunks of their own into one
  bucket at once, with keys that repeat within and across them
- **THEN** in every order of their steps every record is linked once,
  the count is the total, and the duplicates add up exactly
- **Verified by:**
  `crates/tessera-kernels/src/table/loom.rs::three_participants_link_into_one_bucket`;
  `crates/tessera-kernels/src/table/loom.rs::a_key_shared_by_two_participants_counts_once`;
  `crates/tessera-kernels/src/table/loom.rs::three_records_of_one_key_count_twice`

### Requirement: Looking rows up
`tess_table_probe` SHALL find for each row of a mask the first record
of its chain whose hash, NULL bits and every key slot equal the row's,
SHALL write its reference for the row, and SHALL fill the mask of found
rows whole. An `int4` key and an `int8` key of the same value SHALL
match. `tess_table_next_match` SHALL move each row to the next record
of its chain with the same hash, NULL bits and keys, and keep in its
mask only the rows that have one. The calls that read found records
SHALL change only the rows of their mask: `tess_table_gather`,
`tess_table_gather_scattered` and `tess_table_gather_words` read words
of the payload, each at a multiple of 8 bytes within it,
`tess_table_gather_key` a key as a Datum and its NULL flag, and
`tess_table_record` and `tess_table_payloads` give pointers into a
record, valid as long as its chunk.

#### Scenario: Rows find their records
- **WHEN** rows are probed with their keys, with absent keys, with a
  NULL key under the grouping's rule, and with keys of the other
  integer kind
- **THEN** each row finds its own record, absent keys find none, a NULL
  finds the NULL record and not the value it hashes like, and an
  `int4` finds the record of the same `int8`
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::inserted_rows_are_found_and_absent_keys_are_not`;
  `crates/tessera-kernels/tests/table.rs::a_null_key_groups_apart_from_the_value_it_hashes_like`;
  `crates/tessera-capi/tests/table.rs::int4_keys_find_the_records_of_int8_keys`;
  `crates/tessera-capi/tests/table.rs::int8_keys_find_the_records_of_int4_keys`

#### Scenario: Every record of a key
- **WHEN** a key has ten records and its rows step with next_match
- **THEN** each step finds another record of the key, and the step after
  the last finds none and leaves the reference as it was
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::equal_keys_chain_through_next_match`;
  `crates/tessera-kernels/tests/table.rs::a_null_key_groups_apart_from_the_value_it_hashes_like`

#### Scenario: Reading the found records
- **WHEN** a payload word, several words or a key of the found rows is
  read, with rows outside the mask
- **THEN** each found row gets its record's value, the other rows keep
  theirs, and a word past the payload or not at a multiple of 8 fails
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::gather_reads_one_payload_word_of_each_selected_match`;
  `crates/tessera-kernels/tests/table.rs::a_scattered_gather_reads_what_a_gather_reads`;
  `crates/tessera-capi/tests/table.rs::the_words_entry_point_gathers_every_column`

### Requirement: The records of a key together
`tess_table_link_grouped` SHALL link a chunk as `tess_table_link` does,
except that a record whose hash, NULL bits and keys the table already
holds SHALL go right after the first such record of its chain, so that
the records of a key stand together. `tess_table_next_in_group` SHALL
move each row to the record right after its own when that record has
the same hash, NULL bits and keys.

#### Scenario: A key's records in a row
- **WHEN** records of ten keys, ten each, are linked grouped
- **THEN** next_in_group steps through exactly the records of each key,
  and the count of duplicates is the records less the keys
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::grouped_records_of_a_key_lie_together`;
  `test/sql/table.sql::tessera_test_table_cycle`

#### Scenario: Grouped chains across chunks and a larger index
- **WHEN** grouped records span several chunks and the table moves to a
  larger index
- **THEN** next_in_group still takes as many steps as next_match
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::grouped_chains_span_chunks_and_survive_a_larger_index`

### Requirement: Calls of one writer
These calls SHALL run only when no other call uses the table:
`tess_table_link_grouped`, `tess_table_find_or_insert`,
`tess_table_regrow`, `tess_table_scan`, `tess_table_clear_key`, and a
change of a payload in place.

- `tess_table_find_or_insert` SHALL give each row of the pending mask,
  in row order, the record of its keys, and SHALL make one with a
  payload of zeros, linked at once, when the table has none. It SHALL
  stop at the first row that needs a new record when the chunk is full
  or the records are half the buckets; the rows not done stay pending.
  The mask of inserted rows SHALL hold exactly the rows whose record the
  call made.
- `tess_table_regrow` SHALL move the table to a new index for a
  capacity, with at least the buckets of the old one, and link every
  record of every chunk it is given again, grouped. No record moves,
  and every reference stays valid.
- `tess_table_scan` SHALL visit every record below the used marks,
  chunk by chunk in the order appended, at least one a call, from a
  cursor that starts at 0; a count of 0 SHALL mean the end.
- `tess_table_clear_key` SHALL write 0 into one key of every record and
  keep its NULL bit.

#### Scenario: Rows find or make their record
- **WHEN** a batch with repeated keys is resolved, a chunk fills, and
  the records reach half the buckets
- **THEN** each key gets one record with a zero payload, inserted marks
  its first row, a full chunk resolves the known keys only, and half the
  buckets stops the call until a larger index
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::rows_find_or_create_the_record_of_their_keys`;
  `crates/tessera-kernels/tests/table.rs::a_full_chunk_resolves_known_keys_only`;
  `crates/tessera-kernels/tests/table.rs::an_index_takes_records_up_to_half_its_buckets_and_regrows`

#### Scenario: A larger index keeps the records
- **WHEN** a table moves to a larger index, or to one with fewer buckets
- **THEN** the walk, the references, the payloads and the chains are as
  before; fewer buckets are refused
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::regrowing_keeps_records_and_their_references`;
  `test/sql/table.sql::tessera_test_table_regrow`

#### Scenario: A walk and a cleared key
- **WHEN** the records are walked a few at a time, a walk of no records
  is asked for, and a key is cleared
- **THEN** the walk visits each record once in the order appended and
  ends with 0, the walk of no records is refused, and the key is 0 in
  every record with its NULL bit kept
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::a_walk_visits_every_record_once_in_appended_order`;
  `crates/tessera-capi/tests/table.rs::the_writer_entry_points_round_trip`;
  `crates/tessera-kernels/tests/table.rs::a_cleared_key_is_zero_in_every_record`

### Requirement: Calls at the same time
Over shared memory several processes SHALL be able to append at once,
each to chunks of its own, to link at once, each its own chunks, and to
probe and read at once. A chunk SHALL have one writer, and the calls of
one writer run alone. Only the count of records and the buckets change
while the table is shared, by atomic operations: a link adds to the
count before it publishes a record, and publishes it by a
compare-and-swap of its bucket with release; a probe reads a bucket
with acquire. A process that finds a record SHALL see it whole.

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

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
may run at once. Then what a join and a grouping keep beside the table:
partitions when it spills, and the calls that append, split, find and
merge by them; the Bloom filter of its keys, alone, shared and filled
together; the marks of RIGHT and FULL joins.
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
empty chunk, its used mark 8. A call SHALL refuse more than 32768
chunks, and SHALL refuse a chunk it writes or walks that is not such a
block before it reads the chunk; every other chunk the caller SHALL
give as such a block, valid for its length. A debug build of the entry
points SHALL check every chunk it is given.

A record SHALL be named by a reference of 32 bits: the number of its
chunk and its first byte there, in units of 8 bytes. Reference 0, the
used mark of chunk 0, names no record. `tess_table_ref`,
`tess_table_ref_chunk` and `tess_table_ref_byte` make and split a
reference, and a reference names the same record in every process.
`tess_table_memory_limit` SHALL return a limit of memory as it is up to
`TESS_TABLE_MEMORY_MAX`, 16 GiB, half of what the most chunks of the
longest length hold, and that maximum for a larger limit.

```
  31             17 16                     0
 ┌─────────────────┬────────────────────────┐
 │ chunk (15 bits) │ byte / 8 (17 bits)     │
 └─────────────────┴────────────────────────┘
   the record is at chunks[chunk] + 8 * (byte / 8)
```

#### Scenario: Chunks past the limits are refused
- **WHEN** a call appends to or walks a chunk that is null, not aligned
  to 8, of a length that is not a multiple of 8, shorter than 8 bytes or
  longer than 1 MiB, whose used mark reads as an empty chunk's; or a
  call is given more than 32768 chunks
- **THEN** the call fails
- **Verified by:**
  `crates/tessera-kernels/src/table/mod.rs::chunks_past_the_limits_are_refused`;
  `crates/tessera-kernels/src/table/mod.rs::misaligned_or_odd_blocks_are_refused`;
  `crates/tessera-kernels/src/table/marks.rs::a_walk_refuses_a_chunk_that_cannot_be_one`

#### Scenario: A debug build checks every chunk
- **WHEN** a probe that reads no chunk is given a chunk longer than
  1 MiB
- **THEN** in a debug build of the entry points it fails, and in a
  release build it goes on
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::a_debug_build_checks_every_chunk`

#### Scenario: A reference names its chunk and its place
- **WHEN** rows fill one chunk and go on into the next
- **THEN** each row's reference names the chunk and the byte of its
  record, and the helpers of the C API read them back
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::inserted_rows_are_found_and_absent_keys_are_not`;
  `test/sql/table.sql::tessera_test_table_cycle`

#### Scenario: The most memory of a table
- **WHEN** limits of 0, 8 MiB and one byte under 16 GiB are given, and
  of 16 GiB, one byte over it, 64 GiB and the largest size
- **THEN** the first three come back as they are and the others as
  16 GiB, which fills half of the 32768 chunks of 1 MiB
- **Verified by:** `test/sql/table.sql::tessera_test_table_memory_limit`

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
its arguments, the header and the count of chunks before it changes
anything, and a chunk before it reads or writes it.

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

### Requirement: Partitions of a table
A table that a node spills to disk SHALL keep its records in
partitions, chosen by the bits of the hash. With `n` partitions at a
shift `s`, the partition of a hash `h` is `(h >> s) & (n - 1)`. `n`
SHALL be a power of two from 1 to 65536, and the bits SHALL lie within
the hash: `s < 32` and `s + log2(n) <= 32`. The buckets take the high
bits of a hash, so a node's first partitions take the low bits, and a
partition split again takes the bits above those.
`tess_table_partition(hash, shift, n)` SHALL give the same partition in
C.

```
 bit 31                                     4   3   2   1   0
    ┌─────────────────────────────────────┬───────┬───────┐
    │ the buckets take the high bits  …   │ split │ first │
    └─────────────────────────────────────┴───────┴───────┘
 first partitions: 4 at shift 0, (h >> 0) & 3
 one of them split again: 4 at shift 2, (h >> 2) & 3
```

A call that takes partitions SHALL take a chunk number for each, below
the table's count of chunks, and SHALL refuse another count, shift or
chunk number before it changes anything. The caller SHALL be the one
writer of those chunks, and two partitions SHALL share a chunk only when
it has no room for a record.

#### Scenario: The partition of a hash
- **WHEN** a chunk of 200 records is split into 4 partitions at shift 9,
  in Rust and through the C API
- **THEN** every record goes to the chunk of partition
  `(hash >> 9) & 3`, the one `tess_table_partition` gives
- **Verified by:**
  `crates/tessera-kernels/src/table/mod.rs::a_chunk_splits_into_the_chunks_of_its_partitions`;
  `test/sql/table.sql::tessera_test_table_partitions`

#### Scenario: Partitions past the limits are refused
- **WHEN** 0, 3 or 131072 partitions are given, 4 partitions at shift
  31, 1 partition at shift 32, or a chunk number past the table's
  chunks, to a split, an append by partition or a lookup by partition
- **THEN** the call fails, and the chunks and the masks are as they were
- **Verified by:**
  `crates/tessera-kernels/src/table/mod.rs::partitions_past_the_hash_or_the_chunks_are_refused`;
  `crates/tessera-capi/tests/table.rs::an_append_by_partition_refuses_partitions_past_the_limits`

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
  table with an index of another kind of key, and then to that table
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
linked. The records SHALL have the size of the key kinds and the
payload the caller gives, and the source SHALL not be the chunk of a
partition.

#### Scenario: A chunk splits into its partitions
- **WHEN** a chunk of 200 records is split 16 at a time into partitions
  whose chunks hold 8, and each partition named full gets a new chunk
- **THEN** every record is copied once, whole, with a next of 0, to its
  partition, and `*from` ends at the source's used mark
- **Verified by:**
  `crates/tessera-kernels/src/table/mod.rs::a_chunk_splits_into_the_chunks_of_its_partitions`;
  `test/sql/table.sql::tessera_test_table_partitions`

#### Scenario: A split that cannot be done is refused
- **WHEN** a split is asked for no records, into one of its partitions'
  chunks, or with a record size other than the source's records
- **THEN** it fails, and `*from` and the chunks are as they were
- **Verified by:**
  `crates/tessera-kernels/src/table/mod.rs::a_split_that_cannot_be_done_is_refused`

### Requirement: Groups by partition
`tess_table_find_or_insert_partitioned` SHALL give each row of the
pending mask the record of its keys as `tess_table_find_or_insert`
does, but SHALL write a new record to the chunk of its partition. A row
whose partition's chunk has no room SHALL stay pending while the rows
after it go on. Once the linked records reach half the buckets, the call
SHALL resolve no more rows: the rows not done stay pending, those of a
known key too, until the caller moves the table to a larger index. The
mask of inserted rows SHALL hold exactly the rows whose record the call
made. The caller is the one writer of the table.

#### Scenario: Groups go to the chunks of their partitions
- **WHEN** three batches of keys go to 4 partitions whose chunks hold 8
  records, each partition with pending rows getting a new chunk
- **THEN** each row's record has its key and lies in its partition, a
  key keeps one record across the batches, and the inserted rows are
  the first row of each new key
- **Verified by:**
  `crates/tessera-kernels/src/table/mod.rs::groups_go_to_the_chunks_of_their_partitions`;
  `test/sql/table.sql::tessera_test_table_combine`

#### Scenario: Half the buckets stops every row
- **WHEN** a batch brings more new keys than half the buckets can take
- **THEN** the call stops at the first row that needs a new record past
  that count, and that row and every row after it stay pending,
  including rows of keys the table holds
- **Verified by:**
  `crates/tessera-kernels/src/table/mod.rs::half_the_buckets_stops_every_row_of_a_lookup_by_partition`

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
  `crates/tessera-kernels/src/table/mod.rs::states_of_a_group_merge`;
  `test/sql/table.sql::tessera_test_table_combine`

#### Scenario: A merge stops for a chunk or a larger index
- **WHEN** the new groups of a source need more room than `chunk` has,
  and then more records than half the buckets
- **THEN** the call stops with `CHUNK_FULL`, then with `INDEX_FULL`, its
  `*from` on the first group not merged, and goes on from there with a
  new chunk and a larger index to `DONE`
- **Verified by:**
  `crates/tessera-kernels/src/table/mod.rs::a_merge_stops_for_a_chunk_or_a_larger_index`

#### Scenario: A merge that cannot be done fails
- **WHEN** a count or a sum would pass the int8 range, or a merge has 65
  aggregates, a payload too small, an unknown kind or a source that is
  `chunk`
- **THEN** the first fails with status
  `TESS_ERROR_INTEGER_OUT_OF_RANGE` and SQLSTATE `22003`, the others
  with `TESS_ERROR_INVALID_ARGUMENT` before any change
- **Verified by:**
  `crates/tessera-kernels/src/table/mod.rs::a_merged_count_past_the_int8_range_fails`;
  `crates/tessera-capi/tests/table.rs::a_merge_that_cannot_be_done_fails`

### Requirement: A Bloom filter of the keys
A Bloom filter SHALL be a power of two of 64-bit words, which the caller
owns and which hold no address. A hash SHALL set four bits of one word,
taken from the hash multiplied by an odd constant: the word from the
product's high bits, the four bits from its low 24 bits, six bits each.
The four bits may coincide.

```
 s = hash × 0x9E3779B97F4A7C15, modulo 2^64; k = log2(words)

 bit 63                        24 23    18 17    12 11     6 5      0
    ┌───────────────────────────┬────────┬────────┬────────┬────────┐
    │ the word: the top k bits  │ bit 4  │ bit 3  │ bit 2  │ bit 1  │
    │ (word 0 when k is 0)      │        │        │        │        │
    └───────────────────────────┴────────┴────────┴────────┴────────┘
```

A row SHALL pass when every bit of its hash is set. The hash is the
table's hash of the row, every key folded in under the table's rule for
NULL, so every key of the table passes. `tess_table_bloom` SHALL clear
the words and set the bits of every record below the used marks of the
table's chunks, linked or not; no append may run meanwhile.
`tess_bloom_add` SHALL set the bits of the hashes of the selected rows.
`tess_bloom_probe` SHALL write the whole of `found`: the selected rows
that pass, and no other row. Words that are not a power of two, and
hashes, masks or results of different row counts, SHALL be refused
before any change.

#### Scenario: The bits of a hash
- **WHEN** one hash is added to an empty filter of 1, 2 and 1024 words
- **THEN** exactly the word and the bits the rule gives are set; the
  hash passes, and a hash with any of those bits clear does not
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::a_hash_sets_the_bits_the_rule_gives`

#### Scenario: Every key of the table passes
- **WHEN** a filter is filled from a table of 5000 keys over several
  chunks, of int8 keys, or of a NULL key under the rule of a grouping,
  or by adding rows, and its rows are probed with some rows not selected
- **THEN** every key passes, and the rows not selected are clear in
  `found`
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::a_filter_lets_every_key_through_and_few_others`;
  `crates/tessera-kernels/tests/table.rs::a_null_group_key_and_int8_keys_pass_their_filter`;
  `crates/tessera-kernels/tests/table.rs::a_probe_writes_the_whole_result`;
  `crates/tessera-capi/tests/table.rs::the_filter_entry_points_size_fill_and_probe`

#### Scenario: A filter of the wrong size is refused
- **WHEN** a filter of 0 or 3 words, or hashes of another row count than
  the mask, are given to fill, add or probe
- **THEN** the call fails, and the words and `found` are as they were
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::a_filter_of_the_wrong_size_is_refused`;
  `crates/tessera-capi/tests/table.rs::the_filter_entry_points_size_fill_and_probe`

### Requirement: The size of a Bloom filter
`tess_table_bloom_words` SHALL give, for `r` records, the least power of
two of words that is at least `max(1, ceil(16 r / 64))`, so 16 bits a
record at least. `tess_table_bloom_words_within`
SHALL give the same, halved while its bytes are more than an eighth of
`limit`, one word at least. A count of records whose filter cannot be
sized SHALL be refused. At 16 bits a record, a filter SHALL let fewer
than one in 100 absent keys through.

#### Scenario: The words for a count of records
- **WHEN** filters are sized for 0, 4, 5 and 5000 records, within limits
  from 0 to a gigabyte, and for 2^64 - 1 records
- **THEN** they have 1, 1, 2 and 2048 words, halved within a limit while
  more than an eighth of it, and the last is refused
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::the_words_of_a_filter_for_its_records`;
  `crates/tessera-kernels/tests/table.rs::a_spill_filter_stays_within_an_eighth_of_its_memory`

#### Scenario: Few absent keys pass
- **WHEN** a filter of 1024 words holds 4096 keys, 16 bits a key, and
  100000 other keys are probed
- **THEN** fewer than 1000 of them pass
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::few_absent_keys_pass_a_filter_of_16_bits_a_key`

### Requirement: A shared Bloom filter
A shared filter SHALL be a state word followed by the words of a filter,
`1 + 2^k` words aligned to 8, which the participants of a shared table
map. `tess_bloom_shared_words` SHALL give the state word and
`tess_table_bloom_words` of the records.

```
 word   0         1         2                    2^k
       ┌─────────┬─────────┬─────────┬─────────┬─────────┐
       │ state   │ word 0  │ word 1  │    …    │ word    │
       │         │ of the filter     │         │ 2^k - 1 │
       └─────────┴─────────┴─────────┴─────────┴─────────┘
 state: 0 none, 1 building, 2 ready
```

`tess_bloom_shared_init` SHALL clear the words and set the state to
none; one participant calls it before any other uses the filter.
`tess_table_try_build_bloom` SHALL let one participant build the
filter: the one whose compare-and-swap moves the state from none to
building fills the filter from the table's records and then stores
ready with release, and `*built` is true for it alone. The others SHALL
return at once with `*built` false, and nobody waits.
`tess_bloom_shared_ready` SHALL read the state with acquire; a
participant that reads ready SHALL see every bit of the filter.
`tess_bloom_shared_probe` SHALL refuse a filter that is not ready, and
otherwise probe as `tess_bloom_probe`. A shared filter that is not
aligned to 8, or whose words less the state word are not a power of
two, SHALL be refused.

#### Scenario: One participant builds the filter
- **WHEN** two participants in the model, four threads, or two calls of
  one process try to build a shared filter
- **THEN** exactly one builds it, a probe before it is ready fails, and
  once it is ready every key passes
- **Verified by:**
  `crates/tessera-kernels/src/table/loom.rs::two_participants_race_to_build_the_filter_and_one_does`;
  `crates/tessera-kernels/src/table/mod.rs::one_of_four_threads_builds_a_shared_filter_every_key_passes`;
  `test/sql/table.sql::tessera_test_table_groups`

#### Scenario: A filter that reads ready is whole
- **WHEN** one participant builds the filter while another reads its
  state and probes, in every order of their steps, and when the model
  stores the state relaxed
- **THEN** the reader either finds the filter not ready or sees every
  key pass, and with a relaxed state the model reports a reader that
  sees an unfilled filter
- **Verified by:**
  `crates/tessera-kernels/src/table/loom.rs::a_reader_sees_the_filter_whole_once_it_is_ready`;
  `crates/tessera-kernels/src/table/loom.rs::a_relaxed_state_lets_a_reader_see_an_unfilled_filter`

#### Scenario: A shared filter of the wrong shape is refused
- **WHEN** a shared filter is not aligned to 8, or has 1 or 4 words
- **THEN** every call over it fails, and its words are as they were
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_filter_entry_points_size_fill_and_probe`

### Requirement: A filter filled together
`tess_bloom_add_atomic` SHALL set the bits of the hashes of the selected
rows in a filter without a state word, each word changed by an atomic
OR, so that several participants may add their rows at once. The caller
SHALL order every addition before any probe, by a barrier, and SHALL
then probe with `tess_bloom_probe`. Its words SHALL be aligned to 8 and
a power of two.

#### Scenario: Participants add at once
- **WHEN** two participants add keys of one word at the same time, in
  every order of their steps, and then probe after both are done; and
  when the model adds by a plain read and write
- **THEN** every key passes; with the plain read and write the model
  reports a key whose bits were lost
- **Verified by:**
  `crates/tessera-kernels/src/table/loom.rs::participants_add_to_one_word_and_lose_no_bit`;
  `crates/tessera-kernels/src/table/loom.rs::a_plain_read_and_write_loses_the_bits_of_a_filter`

#### Scenario: A filter filled by one participant
- **WHEN** one participant adds the valid rows of a batch and probes
  them back
- **THEN** exactly the valid rows pass
- **Verified by:** `test/sql/table.sql::tessera_test_table_shared_spill`

### Requirement: The marks of RIGHT and FULL joins
A RIGHT or FULL join SHALL mark each record of its table that a pair
matched, a bit a record, in words the caller keeps: a run of words for
each chunk, where bit `i` of word `w` of chunk `c`'s run stands for the
chunk's record `64 w + i`, records counted from byte 8 in steps of the
record size `s`.

```
 chunk c    ┌──────┬────────────┬────────────┬─────┬────────────┬─────┐
            │ used │ record 0   │ record 1   │  …  │ record 64  │  …  │
            └──────┴────────────┴────────────┴─────┴────────────┴─────┘
 byte        0      8            8 + s              8 + 64 s

 marks[c]   ┌──────────────────────────────┬───────────────────────┐
            │ word 0: bit i for record i   │ word 1: bit i for     │
            │                              │ record 64 + i  …      │
            └──────────────────────────────┴───────────────────────┘
```

`tess_table_mark_words` SHALL give the words of a chunk of `len` bytes,
`ceil(floor((len - 8) / s) / 64)`. The record size SHALL be a multiple
of 8, at least 24, a header and one key, and room for a record in the
largest chunk.
`tess_table_mark` SHALL set the mark of the record each selected row's
reference names: a record of the record size, on its boundary, within
its chunk's length, in a chunk of the table. It SHALL not need the
index; when the table has one, a record size other than its SHALL be
refused. With `shared`, every participant may mark at once, and each
word SHALL change by an atomic OR.

`tess_table_next_unmarked` SHALL walk the records below the used marks
as `tess_table_scan` does, from a cursor that starts at 0, and give the
references of those without a mark, at most `capacity` and at least one
a call; the cursor SHALL then name the first record not given, and a
count of 0 SHALL mean the end. Without marks it SHALL give every record.
A cursor off a record's boundary or past its chunk's used mark SHALL be
refused. The caller SHALL order every participant's marks before the
walk, and nothing marks or appends during it.

Over a shared table, the last participant to leave walks the records
without a mark. A participant that leaves while it probes, its share of
the outer side not done, SHALL first mark a word that the participants
of the table, or of the round over a partition, share, with
`tess_build_stop`, which SHALL mark it only while the participant
probes. The last participant to leave SHALL read the word with
`tess_build_stopped` and, when it is marked, return no record without a
mark: the pairs of the rows left unprobed were never marked, and a
participant stops so only when its plan wants no more rows.

#### Scenario: The marks of a chunk
- **WHEN** the words of chunks of 8, 40, 2056 and 2088 bytes are asked
  for records of 32 bytes, and the record 70 of chunk 1 is marked
- **THEN** they are 0, 1, 1 and 2, and bit 6 of word 1 of chunk 1's
  run is the only bit set
- **Verified by:**
  `crates/tessera-kernels/src/table/marks.rs::a_chunk_has_a_word_of_marks_per_64_records`;
  `crates/tessera-kernels/tests/table.rs::a_record_s_mark_is_its_bit_of_its_chunk_s_run`

#### Scenario: Marked records are left out of the walk
- **WHEN** some records of several chunks are marked, alone or shared,
  and the records without a mark are walked a few at a time, with and
  without marks
- **THEN** the walk gives exactly the records without a mark, each once
  in the order appended, or every record without marks, and ends with 0
- **Verified by:**
  `crates/tessera-kernels/tests/table.rs::marked_records_are_left_out_of_the_walk`;
  `crates/tessera-capi/tests/table.rs::the_mark_entry_points_leave_marked_records_out`

#### Scenario: Marks and walks that cannot be done are refused
- **WHEN** a mark names a byte before a chunk's records, off a record's
  boundary, past its chunk's length or in no chunk; a record size is 0,
  12, larger than a chunk or not the index's; a cursor is off a record
  or past its chunk's used mark; or a walk asks for no records
- **THEN** the call fails and the cursor and the count are as they were
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::marks_and_walks_that_cannot_be_done_are_refused`;
  `crates/tessera-kernels/src/table/marks.rs::a_walk_refuses_a_used_mark_or_a_cursor_off_the_records`

#### Scenario: Participants mark at once
- **WHEN** two participants mark records of one word at the same time,
  in every order of their steps, and the records are walked after both
  are done; and when the model marks by a plain read and write
- **THEN** the walk gives no marked record; with the plain read and
  write the model reports a mark that was lost
- **Verified by:**
  `crates/tessera-kernels/src/table/loom.rs::participants_mark_one_word_and_lose_no_mark`;
  `crates/tessera-kernels/src/table/loom.rs::a_plain_read_and_write_loses_marks`

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
  `crates/tessera-kernels/src/table/loom.rs::the_last_participant_sees_one_that_stopped_while_it_probed`;
  `crates/tessera-kernels/src/table/loom.rs::a_stop_marked_after_leaving_goes_unseen`;
  `crates/tessera-kernels/src/table/phases.rs::a_participant_stops_only_while_it_probes`;
  `crates/tessera-capi/tests/table.rs::a_participant_marks_its_stop_only_while_it_probes`;
  `test/sql/join.sql::worker that has its share stops while it probes`

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

## ADDED Requirements

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
      64 │ keys      │ flags     │ reserved              │
         ├───────────┴───────────┼───────────────────────┤
      80 │ reserved              │ reserved              │
         ├───────────────────────┴───────────────────────┤
      96 │ bucket 0, bucket 1, …  4 bytes each           │
         └───────────────────────────────────────────────┘
```

- magic: a number of 64 bits whose bytes are `RELHASH` and a zero on
  a little-endian machine;
- version: the version of the format, 2, `FORMAT_VERSION`;
- header size: 96, `HEADER_SIZE`; buckets offset: 96, where the
  buckets start;
- index length: the bytes the index was made in, a multiple of 8;
- records: the records linked into the buckets;
- buckets: a power of two, at least 1024; bucket shift: 32 less the
  logarithm of the buckets, so that the bucket of a hash is its high
  bits, `hash >> shift`;
- record size, payload size and keys: see "A record";
- flags and every reserved word: 0.

For a capacity of `n` records, `index_size` SHALL return
`96 + 4 * buckets`, where buckets is the least power of two that is at
least 1024 and at least `2 * n`, and at most 2^31.
`TableMut::create` SHALL make the index of an empty table in a block
aligned to 8 whose length is a multiple of 8 and at least that size:
the header as above, no record, every bucket 0. `Table::stats` SHALL
report the records, the buckets, the bytes of the header and the
buckets, and the index length.

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
The records of a table SHALL lie in chunks, at most 32768 of them,
`MAX_CHUNKS`, numbered from 0. A chunk is a block aligned to 8 of 8
bytes to 1 MiB, `MAX_CHUNK_LEN`, a multiple of 8. Its first 8 bytes,
`CHUNK_HEADER`, are its used mark, the bytes its records take with the
mark's own 8; the records follow one after another from byte 8.
`Chunks::init` SHALL make a block an empty chunk, its used mark 8. A
call SHALL refuse more than 32768 chunks, and SHALL refuse a chunk it
writes or walks that is not such a block before it reads the chunk;
every other chunk the caller SHALL give as such a block, valid for its
length.

A record SHALL be named by a reference of 32 bits: the number of its
chunk and its first byte there, in units of 8 bytes, in the low 17
bits, `UNIT_BITS`. Reference 0, the used mark of chunk 0, names no
record. A reference names the same record in every process.

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
  `crates/relhash/src/table/tests.rs::chunks_past_the_limits_are_refused`;
  `crates/relhash/src/table/tests.rs::misaligned_or_odd_blocks_are_refused`;
  `crates/relhash/src/table/marks.rs::a_walk_refuses_a_chunk_that_cannot_be_one`

#### Scenario: A reference names its chunk and its place
- **WHEN** rows fill one chunk and go on into the next
- **THEN** each row's reference names the chunk and the byte of its
  record, and splits back into them
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::inserted_rows_are_found_and_absent_keys_are_not`;
  `test/sql/table.sql::tessera_test_table_cycle`

#### Scenario: Reference 0 names no record
- **WHEN** a call is asked for the record at reference 0
- **THEN** the call fails, and an empty bucket or the end of a chain
  holds 0
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::reference_zero_names_no_record`

### Requirement: A record
Every record of a table SHALL have the size in the header: a header of
16 bytes, a slot of 8 bytes for each key, then the payload, padded with
zeros to a multiple of 8. A table SHALL have 1 to 16 keys, `MAX_KEYS`,
each a word of 8 bytes that the table compares bit for bit and gives
no type: a source of keys, `KeySource`, gives each row's words. A NULL
key SHALL hold 0 in its slot and set its bit of the NULL bits.
`record_bytes` SHALL return the size of a record of a configuration's
keys and payload, and SHALL refuse a record that does not fit in a
chunk.

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
  the same through the C API; 0 and 17 keys are refused
- **Verified by:**
  `crates/relhash/src/table/tests/format.rs::a_record_takes_its_header_its_key_slots_and_its_payload`;
  `test/sql/table.sql::tessera_test_table_record_size`

#### Scenario: A record as a call reads it
- **WHEN** a record is read through `Table::record`
- **THEN** it has the row's hash, its keys' words, a NULL key as 0 with
  its bit set, and the row's payload
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_null_key_groups_apart_from_the_value_it_hashes_like`;
  `crates/tessera-capi/tests/table.rs::the_entry_points_round_trip`

#### Scenario: A record larger than a chunk
- **WHEN** a table is sized or created for a record that does not fit
  in a chunk of 1 MiB
- **THEN** the call fails, and the largest record that fits is accepted
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_record_larger_than_a_chunk_is_refused`

### Requirement: Appending rows
An append SHALL write the rows of a batch's pending mask, in row order,
as records into the chunk it names, while a whole record fits. Each row
written SHALL leave the mask and get the reference of its record; the
others stay. A record SHALL take the row's hash, keys and payload, or a
payload of zeros when none is given. A chunk writer, `ChunkWriter`,
SHALL append without an index, for the keys and the payload of the
configuration it is made with. `Table::append` and `TableMut::append`
SHALL check the table's index as every call does, and SHALL refuse keys
or a payload size other than the table's before they write anything.
An append SHALL not link the records: they are not counted or found
until linked. `append_columns` SHALL take the payload from up to 2048
columns, `MAX_PAYLOAD_COLUMNS`, without allocating memory: words of
NULL bits, bit `c % 64` of word `c / 64` for column `c` and at least
one word, then a word for each column, 0 for a NULL.

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
  `crates/tessera-capi/tests/table.rs::an_append_refuses_records_that_are_not_its_table_s`

### Requirement: Linking records
`Table::link` and `TableMut::link` SHALL link the records of a chunk
from a byte at a record's start up to the used mark, each first in its
bucket's chain, and SHALL move the byte past them. Records of equal
keys SHALL stay separate records. The count of records in the header
SHALL grow before any of them can be found. `link_counting` SHALL link
as `link` does and SHALL also count the records whose hash, NULL bits
and keys a record linked earlier already has; the sum of these counts
over the processes that link at once SHALL be exact.

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

### Requirement: Looking rows up
`Table::probe` SHALL find for each row of a lookup's mask, `Matches`,
the first record of its chain whose hash, NULL bits and every key word
equal the row's, SHALL write its reference for the row, and SHALL keep
in the mask the rows that found one. `Table::next_match` SHALL move
each row to the next record of its chain with the same hash, NULL bits
and keys, and keep in its mask only the rows that have one. The calls
that read found records SHALL change only the rows of their mask:
`gather`, `gather_scattered` and `gather_words` read words of the
payload, each at a multiple of 8 bytes within it, and `gather_key` a
key's word and its NULL flag; `Table::record` gives a record's fields
and `TableMut::payload_ptrs` pointers to payloads, valid as long as
their chunk.

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
  `crates/tessera-capi/tests/table.rs::the_words_entry_point_gathers_every_column`

### Requirement: The records of a key together
`TableMut::link_grouped` SHALL link a chunk as `link` does, except that
a record whose hash, NULL bits and keys the table already holds SHALL
go right after the first such record of its chain, so that the records
of a key stand together. `Table::next_in_group` SHALL move each row to
the record right after its own when that record has the same hash,
NULL bits and keys.

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

### Requirement: Calls of one writer
These calls of a table's one writer, `TableMut`, SHALL run only when no
other call uses the table: `link_grouped`, `find_or_insert`, `regrow`,
`scan`, `clear_key`, and a change of a payload in place.

- `find_or_insert` SHALL give each row of the batch's pending mask, in
  row order, the record of its keys, and SHALL make one with a payload
  of zeros, linked at once, when the table has none. It SHALL stop at
  the first row that needs a new record when the chunk is full or the
  records are half the buckets; the rows not done stay pending. The
  call SHALL add to the mask of inserted rows exactly the rows whose
  record it made, and SHALL leave the mask's other bits as they were,
  so that the masks of the calls of one batch add up.
- `regrow` SHALL move the table to a new index for a capacity, with at
  least the buckets of the old one, and link every record of every
  chunk it is given again, grouped. No record moves, and every
  reference stays valid.
- `scan` SHALL visit every record below the used marks, chunk by chunk
  in the order appended, at least one a call, from a cursor that starts
  at `Cursor::start`; a count of 0 SHALL mean the end.
- `clear_key` SHALL write 0 into one key of every record and keep its
  NULL bit.

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

### Requirement: Calls at the same time
Over shared memory several processes SHALL be able to append at once,
each to chunks of its own, to link at once, each its own chunks, and to
probe and read at once, each through a `Table` of its own over the same
blocks. A chunk SHALL have one writer, and the calls of one writer run
alone. Of the index and the chunks, only the count of records and the
buckets change while the table is shared, by atomic operations: a link
adds to the count before it publishes a record, and publishes it by a
compare-and-swap of its bucket with release; a probe reads a bucket
with acquire. A process that finds a record SHALL see it whole.

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
  `crates/tessera-kernels/src/loom.rs::two_participants_append_size_link_and_probe`;
  `crates/tessera-kernels/src/loom.rs::three_participants_attach_at_any_phase`

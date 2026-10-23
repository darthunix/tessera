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
the table: its index, chunks and records, and its calls. Then
partitions and the calls by partition, the Bloom filter, alone, shared
and filled together, and the marks. Last, what makes a table damaged.
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

### Requirement: Partitions of a table
A table that a caller spills to disk SHALL keep its records in
partitions, chosen by the bits of the hash. A `Partitions` names them:
with `n` partitions, the length of its `chunks`, at its `shift` `s`,
the partition of a hash `h` is `(h >> s) & (n - 1)`. `n` SHALL be a
power of two from 1 to 65536, `MAX_PARTITIONS`, and the bits SHALL lie
within the hash: `s < 32` and `s + log2(n) <= 32`. The buckets take the
high bits of a hash, so a caller's first partitions take the low bits,
and a partition split again takes the bits above those.

```
 bit 31                                     4   3   2   1   0
    ┌─────────────────────────────────────┬───────┬───────┐
    │ the buckets take the high bits  …   │ split │ first │
    └─────────────────────────────────────┴───────┴───────┘
 first partitions: 4 at shift 0, (h >> 0) & 3
 one of them split again: 4 at shift 2, (h >> 2) & 3
```

A call that takes partitions SHALL take in `chunks` a chunk number for
each, below the count of chunks it is given, and SHALL refuse another
count, shift or chunk number before it changes anything. The caller
SHALL be the one writer of those chunks, and two partitions SHALL share
a chunk only when it has no room for a record.

#### Scenario: The partition of a hash
- **WHEN** a chunk of 200 records is split into 4 partitions at shift 9,
  by the crate and through Tessera's C API
- **THEN** every record goes to the chunk of partition
  `(hash >> 9) & 3`, the one `tess_table_partition` gives
- **Verified by:**
  `crates/relhash/src/table/tests.rs::a_chunk_splits_into_the_chunks_of_its_partitions`;
  `test/sql/table.sql::tessera_test_table_partitions`

#### Scenario: Partitions past the limits are refused
- **WHEN** 0, 3 or 131072 partitions are given, 4 partitions at shift
  31, 1 partition at shift 32, or a chunk number past the table's
  chunks, to a split, an append by partition or a lookup by partition
- **THEN** the call fails, and the chunks and the masks are as they were
- **Verified by:**
  `crates/relhash/src/table/tests.rs::partitions_past_the_hash_or_the_chunks_are_refused`;
  `crates/tessera-capi/tests/table.rs::an_append_by_partition_refuses_partitions_past_the_limits`

### Requirement: Appending rows by partition
`ChunkWriter::append_partitioned_columns` SHALL append the rows of the
batch's pending mask, in row order, each as a record to the chunk of
its partition, as `append_columns` appends to one chunk. A row whose
partition's chunk has no room SHALL stay pending while the rows after
it go on; the caller gives that partition a new chunk and calls again.
The payload SHALL be a word of NULL bits, bit `c` for column `c`, then
a word for each column, 0 for a NULL, so the call SHALL take at most 64
columns and a table whose payload is `8 * (1 + columns)` bytes. Each
row appended SHALL add one to its partition's count in
`Appended::rows`, which the call adds to and never clears, and SHALL
OR its NULL bits into `Appended::nulls`. As a chunk writer does, the
call SHALL not need the index and SHALL not link the records.

#### Scenario: Rows go to the chunks of their partitions
- **WHEN** 70 rows go to 4 partitions at shift 3, the chunk of
  partition 3 having room for 5 records
- **THEN** each row's record lies in its partition's chunk with its
  payload words, the counts are those of the rows appended, the NULL
  bits have the bit of the column with a NULL, and exactly the rows of
  partition 3 past the first 5 stay pending
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_partitioned_columns_entry_point_counts_each_partition`

#### Scenario: An append by partition checks its columns
- **WHEN** rows are appended by partition with 65 columns, or with a
  table whose payload is not a word of NULL bits and a word a column
- **THEN** the call fails, writes nothing and leaves every row pending
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::an_append_by_partition_checks_what_it_is_given`

### Requirement: Splitting a chunk by partition
`ChunkWriter::split` SHALL copy the records of chunk `source` from byte
`*from` on, whole and in the order appended, each to the chunk of its
partition, and SHALL move `*from` past each record copied. It SHALL
copy at most as many records as `refs` has slots, at least one a call,
and give the reference and the hash of each copy in `refs` and
`hashes`. It SHALL stop before a record whose partition's chunk has no
room and name that partition in `Split::full`; a `Split::count` of 0
with no full partition SHALL mean that the source is done. A copy's
next SHALL be 0: the copies are not linked. The records SHALL have the
size of the keys and the payload of the writer's configuration, and the
source SHALL not be the chunk of a partition.

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
`TableMut::find_or_insert_partitioned` SHALL give each row of the
batch's pending mask the record of its keys as `find_or_insert` does,
but SHALL write a new record to the chunk of its partition. A row whose
partition's chunk has no room SHALL stay pending while the rows after
it go on. Once the linked records reach half the buckets, the call
SHALL resolve no more rows: the rows not done stay pending, those of a
known key too, until the caller moves the table to a larger index. The
call SHALL add to the mask of inserted rows the rows whose record it
made, as `find_or_insert` does. It is a call of the one writer.

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

A row SHALL pass when every bit of its hash is set. A filter is filled
with the hashes of the table's records, so a row with the hash of a
record passes. `Table::bloom` SHALL clear the words and set the bits of
every record below the used marks of the table's chunks, linked or
not; no append may run meanwhile. `bloom::add` SHALL set the bits of
the hashes of the selected rows. `bloom::probe` SHALL keep in its mask
the selected rows that pass, and no other row. Words that are not a
power of two, and hashes of another row count than the mask, SHALL be
refused before any change.

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

### Requirement: The size of a Bloom filter
`bloom::words_for` SHALL give, for `r` records, the least power of two
of words that is at least `max(1, ceil(16 r / 64))`, so 16 bits a
record at least. `bloom::words_within` SHALL give the same, halved
while its bytes are more than an eighth of `limit`, one word at least.
A count of records whose filter cannot be sized SHALL be refused. At 16
bits a record, a filter SHALL let fewer than one in 100 absent keys
through.

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
A shared filter, `SharedFilter`, SHALL be a state word followed by the
words of a filter, `1 + 2^k` words aligned to 8, which the participants
of a shared table map. `bloom::shared_words_for` SHALL give the state
word and `bloom::words_for` of the records.

```
 word   0         1         2                    2^k
       ┌─────────┬─────────┬─────────┬─────────┬─────────┐
       │ state   │ word 0  │ word 1  │    …    │ word    │
       │         │ of the filter     │         │ 2^k - 1 │
       └─────────┴─────────┴─────────┴─────────┴─────────┘
 state: 0 none, 1 building, 2 ready
```

`SharedFilter::init` SHALL clear the words and set the state to none;
one participant calls it before any other uses the filter.
`Table::try_build_bloom` SHALL let one participant build the filter:
the one whose compare-and-swap moves the state from none to building
fills the filter from the table's records and then stores ready with
release, and the call returns true for it alone. The others SHALL
return false at once, and nobody waits. `SharedFilter::ready` SHALL
read the state with acquire; a participant that reads ready SHALL see
every bit of the filter. `bloom::probe_shared` SHALL refuse a filter
that is not ready, and otherwise probe as `bloom::probe`. Words that
are not aligned to 8, or whose count less the state word is not a power
of two, SHALL be refused as a shared filter.

#### Scenario: One participant builds the filter
- **WHEN** two participants in the model, four threads, or two calls of
  one process try to build a shared filter
- **THEN** exactly one builds it, a probe before it is ready fails, and
  once it is ready every key passes
- **Verified by:**
  `crates/relhash/src/table/loom/filter.rs::two_participants_race_to_build_the_filter_and_one_does`;
  `crates/relhash/src/table/tests.rs::one_of_four_threads_builds_a_shared_filter_every_key_passes`;
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
  `crates/tessera-capi/tests/table.rs::the_filter_entry_points_size_fill_and_probe`

### Requirement: A filter filled together
`bloom::add_atomic` SHALL set the bits of the hashes of the selected
rows in a filter without a state word, each word changed by an atomic
OR, so that several participants may add their rows at once. The caller
SHALL order every addition before any probe, by a barrier, and SHALL
then probe with `bloom::probe`. Its words SHALL be aligned to 8 and a
power of two.

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
- **Verified by:** `test/sql/table.sql::tessera_test_table_shared_spill`

### Requirement: The marks of RIGHT and FULL joins
A RIGHT or FULL join SHALL be able to mark each record of its table
that a pair matched, a bit a record, in words the caller keeps and the
crate reaches through `Marks`: a run of words for each chunk, where bit
`i` of word `w` of chunk `c`'s run stands for the chunk's record
`64 w + i`, records counted from byte 8 in steps of the record size
`s`.

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

`mark_words` SHALL give the words of a chunk of `len` bytes,
`ceil(floor((len - 8) / s) / 64)`. The record size SHALL be a multiple
of 8, at least 24, a header and one key, and room for a record in the
largest chunk. `mark` SHALL set the mark of the record each selected
row's reference names: a record of the record size, on its boundary,
within its chunk's length, in a chunk it is given. It SHALL need only
the lengths of the chunks and the record size, not the index;
`Table::mark` marks with the table's. `Marks::set` takes `&self`, so
that participants may mark at once, through marks that change each word
by an atomic OR.

`scan_unmarked` SHALL walk the records below the used marks as
`TableMut::scan` does, from a `Cursor` that starts at `Cursor::start`,
and give the references of those without a mark, at most as many as
its output has slots and at least one a call; the cursor SHALL then
name the first record not given, and a count of 0 SHALL mean the end.
Without marks it SHALL give every record. A cursor off a record's
boundary or past its chunk's used mark SHALL be refused.
`Table::scan_unmarked` walks the table's chunks with its record size.
The caller SHALL order every participant's marks before the walk, and
nothing marks or appends during it.

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
  `crates/tessera-capi/tests/table.rs::the_mark_entry_points_leave_marked_records_out`

#### Scenario: Marks and walks that cannot be done are refused
- **WHEN** a mark names a byte before a chunk's records, off a record's
  boundary, past its chunk's length or in no chunk; a record size is 0,
  12 or larger than a chunk; a cursor is off a record or past its
  chunk's used mark; or a walk asks for no records
- **THEN** the call fails and the cursor is as it was
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::marks_and_walks_that_cannot_be_done_are_refused`;
  `crates/relhash/src/table/marks.rs::a_walk_refuses_a_used_mark_or_a_cursor_off_the_records`

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
A call SHALL return an error, and never panic or loop without end, when
the bytes it is given are not a table: a header that breaks a rule of
"The index of a table", an index length below the header's, a
reference outside its chunk or to a byte where no record starts, a
record whose size is not the table's, a used mark that ends no record,
or a chain with a loop. A walk down a chain SHALL take no more steps
than the header's count of records, and no more than the places a
reference can name in the chunks it is given, 2^17 a chunk. A call
SHALL check its arguments, the header and the count of chunks before
it changes anything, and a chunk before it reads or writes it.

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

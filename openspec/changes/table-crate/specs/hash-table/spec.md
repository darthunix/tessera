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
share no word.

#### Scenario: The lookups in place
- **WHEN** a batch of 64 rows, of which 16 have keys of the table, is
  probed, stepped through its keys' records by group and by chain, and
  checked against a filter and a shared filter, each call given one
  mask as its rows and results, or two masks of the same words
- **THEN** each call keeps the rows, and gives the references, that a
  call with two masks keeps and gives
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_lookups_narrow_a_mask_in_place`

#### Scenario: A lookup in place refused
- **WHEN** a probe in place is given two keys for a table of one, or a
  mask of 63 rows over the words of its mask of 64
- **THEN** the call fails, and the words are as they were
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_lookups_narrow_a_mask_in_place`

## MODIFIED Requirements

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
- version: the version of the format, 2, which
  `tess_table_format_version()` returns;
- header size: 96; buckets offset: 96, where the buckets start;
- index length: the bytes the index was made in, a multiple of 8;
- records: the records linked into the buckets;
- buckets: a power of two, at least 1024; bucket shift: 32 less the
  logarithm of the buckets, so that the bucket of a hash is its high
  bits, `hash >> shift`;
- record size, payload size and keys: see "A record";
- flags and every reserved word: 0.

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
  `crates/relhash/src/table/tests.rs::misaligned_or_odd_blocks_are_refused`

### Requirement: A record
Every record of a table SHALL have the size in the header: a header of
16 bytes, a slot of 8 bytes for each key, then the payload, padded with
zeros to a multiple of 8. A table SHALL have 1 to 16 keys, each a word
of 8 bytes that the table compares bit for bit and gives no type: the
key columns of a call put each value into its word, an `int4` widened
with its sign and an `int8` as it is. A NULL key SHALL hold 0 in its
slot and set its bit of the NULL bits.
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
  size, the keys, the flags or a byte of a reserved word
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
  `crates/relhash/src/table/tests.rs::a_damaged_count_is_bounded_by_the_places_of_the_chunks`

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
  payload size or another count of keys, from an array or from columns,
  and then with the table's own; and rows are appended without an index
- **THEN** with the index the wrong ones fail, write nothing and leave
  every row pending, and the table's own go in; without an index the
  records that the arguments describe go in
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::an_append_refuses_records_that_are_not_its_table_s`

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
  table with an index of another count of keys, and then to that table
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
linked. The records SHALL have the size of the keys and the payload
the caller gives, and the source SHALL not be the chunk of a
partition.

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

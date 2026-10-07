## ADDED Requirements

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
- **Verified by:** pending

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
- **Verified by:** pending

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
- **Verified by:** pending

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
- **Verified by:** pending

#### Scenario: Half the buckets stops every row
- **WHEN** a batch brings more new keys than half the buckets can take
- **THEN** the call stops at the first row that needs a new record past
  that count, and that row and every row after it stay pending,
  including rows of keys the table holds
- **Verified by:** pending

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
uses the table no more.

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
- **Verified by:** pending

#### Scenario: A merge that cannot be done fails
- **WHEN** a count or a sum would pass the int8 range, or a merge has 65
  aggregates, a payload too small, an unknown kind or a source that is
  `chunk`
- **THEN** the first fails with status
  `TESS_ERROR_INTEGER_OUT_OF_RANGE` and SQLSTATE `22003`, the others
  with `TESS_ERROR_INVALID_ARGUMENT` before any change
- **Verified by:** pending

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
- **Verified by:** pending

#### Scenario: Every key of the table passes
- **WHEN** a filter is filled from a table of 5000 keys over several
  chunks, of int8 keys, or of a NULL key under the rule of a grouping,
  or by adding rows, and its rows are probed with some rows not selected
- **THEN** every key passes, and the rows not selected are clear in
  `found`
- **Verified by:** pending

#### Scenario: A filter of the wrong size is refused
- **WHEN** a filter of 0 or 3 words, or hashes of another row count than
  the mask, are given to fill, add or probe
- **THEN** the call fails, and the words and `found` are as they were
- **Verified by:** pending

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
- **Verified by:** pending

#### Scenario: Few absent keys pass
- **WHEN** a filter of 1024 words holds 4096 keys, 16 bits a key, and
  100000 other keys are probed
- **THEN** fewer than 1000 of them pass
- **Verified by:** pending

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
- **Verified by:** pending

### Requirement: A filter filled together
`tess_bloom_shared_add` SHALL set the bits of the hashes of the selected
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
- **Verified by:** pending

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
of 8, at least 16, and room for a record in the largest chunk.
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

#### Scenario: The marks of a chunk
- **WHEN** the words of chunks of 8, 40, 2056 and 2088 bytes are asked
  for records of 32 bytes, and the record 70 of chunk 1 is marked
- **THEN** they are 0, 1, 1 and 2, and bit 6 of word 1 of chunk 1's
  run is the only bit set
- **Verified by:** pending

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
- **Verified by:** pending

#### Scenario: Participants mark at once
- **WHEN** two participants mark records of one word at the same time,
  in every order of their steps, and the records are walked after both
  are done; and when the model marks by a plain read and write
- **THEN** the walk gives no marked record; with the plain read and
  write the model reports a mark that was lost
- **Verified by:** pending

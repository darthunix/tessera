## ADDED Requirements

### Requirement: A crate of its own
The table SHALL be the Rust crate `relhash`, which depends on no crate
of Tessera. The crate SHALL build, pass its tests and pack alone, on
targets whose pointers are 64 bits wide; on any
other target it SHALL fail to build, with an error that says why.

Every public item of the crate SHALL have a text that says what it is
for, what it takes and gives, who owns what and for how long, and the
kind of error it returns. The documentation SHALL build without
warnings, and every example in it SHALL run as a test.

#### Scenario: The crate packs alone
- **WHEN** the crate is packed, as for publishing
- **THEN** it builds from the packed files and the crates it names,
  none of them Tessera's
- **Verified by:**
  `.github/workflows/ci.yml::cargo publish --dry-run --locked -p relhash`

#### Scenario: The crate's tests run alone
- **WHEN** the tests of the crate run as the only package
- **THEN** they pass
- **Verified by:**
  `.github/workflows/ci.yml::cargo test --locked -p relhash`

#### Scenario: A target of 32 bits
- **WHEN** the crate is built for a target whose pointers are 32 bits
  wide
- **THEN** the build fails with an error that names 64-bit targets
- **Verified by:**
  `.github/workflows/ci.yml::relhash needs a target whose pointers
  are 64 bits wide`

#### Scenario: Documentation built with warnings as errors
- **WHEN** the documentation of the crate is built with every warning
  an error, and with a lint that refuses a public item without a text
- **THEN** it builds
- **Verified by:** pending

#### Scenario: The examples run
- **WHEN** the tests of the documentation run
- **THEN** every example passes
- **Verified by:** pending

### Requirement: The masks of rows
A mask of rows SHALL say which rows of a batch a call works on: one bit
a row, 1 for a row that takes part. A mask made of words SHALL have
exactly the words its rows need, row `r` in bit `r % 64` of word
`r / 64`, and SHALL be refused, its words left as they were, when it
has more or fewer words or a bit set past its last row. A mask that a
call writes SHALL keep every bit past its last row 0.

```
 a mask of 70 rows made of words: two words
 word 0   bits 0..=63    rows 0..=63
 word 1   bits 0..=5     rows 64..=69
          bits 6..=63    0, always
```

#### Scenario: Words of the wrong size or with a bit past the rows
- **WHEN** a mask of 70 rows is made of one word or of three, or of two
  words with bit 6 of the second set
- **THEN** it is refused, and its words are not changed
- **Verified by:** pending

#### Scenario: The bits past the rows of a mask a call writes
- **WHEN** a call writes a mask of 70 rows whose second word was all
  ones before
- **THEN** bits 6 to 63 of that word are 0
- **Verified by:** pending

### Requirement: A batch to place
A batch to place SHALL be one value: the hashes and the keys of its
rows, a mask of the rows still to place and, for each row, the
reference of its record. Hashes, keys, a mask and references of
different row counts SHALL be refused when the batch is made. A call
that places rows SHALL clear the bit of each row it places and write
the reference of its record at the row, SHALL write no place of a row
outside the mask, and SHALL return how many rows it placed. The calls
of the owning table SHALL take the rows to place as a mask they do not
change, and SHALL place every one of them.

#### Scenario: A batch over two chunks
- **WHEN** a batch of four rows, one of them left out of its mask, is
  appended to chunks with room for two records each
- **THEN** the first call places two rows and leaves one pending, the
  second places it, each placed row has the reference of its record,
  and the row left out keeps what its place held
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::a_batch_goes_through_two_chunks`

#### Scenario: The owning table places every row
- **WHEN** the owning table inserts, and finds or inserts, the rows of
  a mask over more records than its first chunk holds
- **THEN** every row of the mask gets the reference of its record, and
  the mask is as it was
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::the_owning_table_places_every_row_of_its_mask`

#### Scenario: A batch of other row counts
- **WHEN** a batch is made of a mask of 70 rows and 69 references
- **THEN** it is refused
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::dimension_errors_come_before_any_change`

### Requirement: A batch that fails part way
When a call that places rows fails at a row, because the source of keys
cannot give a word's keys or a partition's chunk is refused, the rows
it placed before SHALL stay placed: they SHALL have left the pending
mask with the references of their records, those that made a record
SHALL be in the mask of inserted rows, and the used mark of each chunk
SHALL cover the records written in it. A link that fails SHALL count
only the records it linked and SHALL move `*from` past them. The same
batch, given again, SHALL place the rest once. A call that fails because
it met damaged bytes keeps no such promise: what the table holds after
it is not promised, the caller drops or resets the table, and the calls
over it stay bounded, as "Any bytes, a bounded call" says.

#### Scenario: A source fails at the second word
- **WHEN** a batch of 128 rows is appended, or found or inserted, and
  the source of keys cannot give its second word
- **THEN** the call fails with a wrong call and the rule of the source,
  the first word's rows have left the pending mask with their
  references, a walk of the chunk sees their records, and the batch
  given again with a source that gives every word places the second
  word after them
- **Verified by:**
  `crates/relhash/src/table/tests/partial.rs::a_source_that_fails_at_the_second_word_leaves_the_first_placed`

#### Scenario: A partition's chunk is refused
- **WHEN** a call by partition meets, in the middle of a word, a
  partition whose chunk's used mark is damaged
- **THEN** it fails with a damaged table; the rows placed before it
  have left the pending mask and, for a find, are in the mask of
  inserted rows; and once the mark is repaired, the batch given again
  places the rest, each row once
- **Verified by:**
  `crates/relhash/src/table/tests/partial.rs::a_partition_with_a_damaged_mark_leaves_the_rows_before_placed`

#### Scenario: A link fails part way
- **WHEN** a link meets a damaged record after it has linked others
- **THEN** the count of records grows by the records linked, and
  `*from` lies past them
- **Verified by:**
  `crates/relhash/src/table/tests/partial.rs::a_link_that_meets_a_damaged_record_counts_what_it_linked`

### Requirement: A lookup narrows one mask
A probe, a step to the next match or to the next in a group, and a
Bloom filter's probe SHALL keep in the mask the rows that find what they
look for, SHALL leave the reference of a row that leaves as it was,
and SHALL return how many rows are left. A lookup SHALL count its steps
to a next match or a next in a group, and SHALL fail as a damaged table
at a step past the count of records, so that a cycle of matching
records ends a walk over the matches.

#### Scenario: A walk down the matches of a key
- **WHEN** ten rows, one for each of ten keys of ten records, step with
  the next match until no row is left
- **THEN** each step returns the rows left, and each key's walk reads
  its ten records
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::equal_keys_chain_through_next_match`

#### Scenario: A row without a next record
- **WHEN** two rows whose keys have one record each step with the next
  match
- **THEN** the call returns 0, both rows leave, and their references
  stay
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_null_key_groups_apart_from_the_value_it_hashes_like`

#### Scenario: A filter narrows the rows of a lookup
- **WHEN** a filter of the hashes of 100 rows probes every other row
- **THEN** the same 50 rows stay, and the call returns 50
- **Verified by:**
  `crates/relhash/src/table/tests/filter.rs::a_probe_narrows_its_rows_in_place`

#### Scenario: A cycle of matching records
- **WHEN** a record's next names itself, or two records of equal keys
  name each other, and a lookup steps to its next match, or its next in
  a group, again and again
- **THEN** within as many steps as the table has records, the step
  fails with a damaged table and the rule of a chain
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_cycle_of_matching_records_ends_a_walk_over_the_matches`

#### Scenario: A chain longer than a short walk
- **WHEN** 300 records of one hash are linked, and their keys and an
  absent key are probed row by row and by whole words, their matches
  are followed, and a find or insert looks for a known and a new key
- **THEN** every known key is found at the end of its walk, the absent
  key finds nothing, the matches visit all 300 records, and no call
  takes the chain for a cycle
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_healthy_chain_longer_than_a_short_walk_is_walked_to_its_end`

### Requirement: Records by reference
A call SHALL give a cursor that reads the record named by a reference,
and the one writer of a table a cursor that changes the payload of the
record named by a reference. Each reference SHALL be checked as a
lookup checks it, and only one payload SHALL be open for change at a
time. A writer SHALL be able to check a reference first and open its
payload later through the same cursor.

#### Scenario: Records read by reference
- **WHEN** a cursor reads the record of each row's reference, and of a
  reference inside a record
- **THEN** each record holds its row's keys and hash, and the reference
  inside a record fails
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::a_record_cursor_reads_records_by_reference`

#### Scenario: Payloads changed by reference
- **WHEN** a writer adds a value to the payload of the record of each
  row a probe found
- **THEN** each record's payload holds the sum of the values of its rows
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::a_payload_cursor_changes_payloads_by_reference`

#### Scenario: Checked now, opened later
- **WHEN** a writer checks the references of a batch, then opens their
  payloads one by one
- **THEN** each payload is the one of the record its reference names
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::a_payload_cursor_holds_checked_references_to_open_later`

#### Scenario: Two payloads open at once
- **WHEN** a program keeps one payload of a cursor open and opens
  another
- **THEN** it does not compile
- **Verified by:** pending

### Requirement: The payloads of a batch
The one writer of a table SHALL be able to change the payload of the
record of each selected row of a batch in one call, and SHALL get, for
each selected row, a raw pointer to the payload of its record, two rows
of one record the same pointer, each usable to read and write until the
chunk is reset, after the call and beside the others, within Rust's
rules of aliasing. When a reference fails its check, the call fails;
the payloads it changed before stay changed.

#### Scenario: Pointers to the payload of one record
- **WHEN** two rows of a batch name one record, the call gives their
  pointers, and the payload is written through each of them in turn
  after the call, under Miri, also through the C entry point
- **THEN** Miri reports no undefined behavior, and the payload holds
  the last write
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::pointers_to_the_payload_of_one_record_write_it_in_turn`;
  `crates/tessera-capi/tests/table.rs::the_writer_entry_points_round_trip`

#### Scenario: A damaged record in the middle
- **WHEN** the record that the third of five references names has a
  size that is not the table's
- **THEN** the call fails with a damaged table, and the payloads of the
  first two rows hold their values
- **Verified by:** pending

### Requirement: A merge by the caller's rule
The one writer of a table SHALL be able to merge the records of a
chunk, from a byte on, into the table by a rule it gives. A record
whose keys the table has SHALL pass its payload to the rule with the
payload of the table's record, which the rule changes. A record of new
keys SHALL be copied whole into a chunk the writer names, and linked.
The merge SHALL stop, with the records merged so far and the byte to go
on from, where a record of new keys finds no room in the chunk, or the
table's records reach half its buckets. When the rule fails, the merge
SHALL stop and return the rule's own error; the records merged before
stay merged, and the payload the rule was changing holds what the rule
left in it.

#### Scenario: Known keys and new keys
- **WHEN** a chunk of records, some of keys the table has and some of
  new keys, is merged by a rule that adds the payloads
- **THEN** the records of known keys hold the sums, and the new keys
  have records equal to the ones merged
- **Verified by:**
  `crates/relhash/src/table/tests.rs::a_merge_by_a_rule_changes_known_keys_and_copies_new_ones`

#### Scenario: A full chunk
- **WHEN** the chunk for new records has room for one, and two records
  of new keys come
- **THEN** the merge stops before the second, and gives the byte where
  it begins
- **Verified by:**
  `crates/relhash/src/table/tests.rs::a_merge_stops_for_a_chunk_or_a_larger_index`

#### Scenario: Half the buckets
- **WHEN** the table's records are half its buckets, and a record of
  new keys comes
- **THEN** the merge stops before it, and gives the byte where it
  begins
- **Verified by:**
  `crates/relhash/src/table/tests.rs::a_merge_stops_for_a_chunk_or_a_larger_index`

#### Scenario: The rule fails
- **WHEN** the rule fails on the second of three records of known keys
- **THEN** the merge returns the rule's error, and the first record is
  merged
- **Verified by:**
  `crates/relhash/src/table/tests.rs::a_failing_rule_stops_the_merge_with_its_own_error`

### Requirement: The owning table
Every order of the owning table's calls SHALL keep its table whole:
each record linked once, and each key found where it was placed. An
insertion that fails part way SHALL leave the rows it placed linked.
The owning table SHALL refuse, when it is made, chunks too small for
one record, and SHALL fail, not grow its index again and again, when it
holds the most records a find or insert may make.

#### Scenario: Calls in any order
- **WHEN** random sequences of insertions, finds or insertions, larger
  indexes and resets run over an owning table
- **THEN** after each call its records, their count and the keys it
  finds are those of a model of the sequence
- **Verified by:**
  `crates/relhash/src/table/tests/owned.rs::the_owning_table_stays_whole_under_any_order_of_its_calls`

#### Scenario: An insertion that fails part way
- **WHEN** the source of an insertion into the owning table cannot give
  its second word
- **THEN** the call fails with the rule of the source, the first word's
  keys are found, the count of records is theirs, and the rows given
  again with a source that gives every word are placed once more
- **Verified by:**
  `crates/relhash/src/table/tests/owned.rs::an_insertion_that_fails_at_the_second_word_links_the_first`

#### Scenario: An owning table at its most records
- **WHEN** a find or insert of the owning table needs a record past the
  most records a table may hold
- **THEN** it fails with a wrong call and the rule of that limit
- **Verified by:**
  `crates/relhash/src/table/tests/owned.rs::the_owning_table_grows_up_to_the_most_records_and_then_fails`

#### Scenario: A chunk too small for a record
- **WHEN** an owning table is made with chunks shorter than a record
- **THEN** it is refused with a wrong call and the rule of a record's
  room
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_record_larger_than_a_chunk_is_refused`

### Requirement: Safe calls are sound
No sequence of the crate's safe calls SHALL let a record that a caller
has borrowed change while it is borrowed. The calls that write a chunk
or the index SHALL need the table to themselves; over a table that
several writers share they SHALL be unsafe calls, whose contract names
the chunks each writer writes. The writer of the chunks of a table that
has no index yet SHALL be made by an unsafe call, whose contract makes
it the one writer of the chunks it writes.

#### Scenario: A record borrowed across an append
- **WHEN** a caller borrows a record of the owning table and then
  appends to its chunk
- **THEN** the code does not compile
- **Verified by:**
  `crates/relhash/src/table/owned.rs::A record borrowed across an append`

#### Scenario: A chunk writer made in safe code
- **WHEN** a program makes a writer of chunks outside an unsafe block
- **THEN** the code does not compile
- **Verified by:**
  `crates/relhash/src/table/chunks.rs::A chunk writer made in safe code`

#### Scenario: An old reference after a reset
- **WHEN** an owning table is reset, an old reference is read, and new
  rows are appended, under Miri
- **THEN** Miri reports no undefined behavior
- **Verified by:**
  `crates/relhash/src/table/tests/owned.rs::an_old_reference_after_a_reset_reads_what_its_place_holds`

### Requirement: Threads
A handle attached to a table's blocks SHALL NOT pass from one thread to
another, nor be shared by them: each thread attaches its own handle to
the table's blocks, and the threads then work as processes do under
"Calls at the same time". The owning table, which owns its blocks and
whose handles borrow it, SHALL pass to another thread whole. A shared
filter, which holds only atomic words, may be shared.

#### Scenario: A handle sent to another thread
- **WHEN** a program sends a handle attached to a table to another
  thread, or shares one between threads
- **THEN** it does not compile
- **Verified by:** pending

#### Scenario: An owning table sent to another thread
- **WHEN** an owning table is filled on one thread and moved to another
- **THEN** the other thread finds every key
- **Verified by:**
  `crates/relhash/src/table/tests/owned.rs::an_owning_table_filled_on_one_thread_is_probed_on_another`

#### Scenario: Threads with handles of their own
- **WHEN** two threads, each with its own handle, append, link and
  probe one table in every order of their steps
- **THEN** every key is found
- **Verified by:** pending

### Requirement: Any bytes, a bounded call
The caller promises that every block it gives is memory of the length
it states, aligned to 8 bytes and initialized, that no two blocks
overlap, that no other process or thread writes the table while the
call runs, and that nothing else reads or writes the chunks of a table
that has itself to itself, whatever index names them. Under that
promise a call SHALL end after a number of steps bounded by the
lengths of its blocks and its arguments, whatever bytes the blocks
hold; SHALL return a result or an error; SHALL NOT panic; and SHALL
read and write no memory but the blocks and arguments it is given. This
SHALL hold after a call that failed as well. Over damaged bytes a
result may be wrong: the promise is only this. Calls that run at once
over a shared table are promised what "Calls at the same time" says,
over a table that is not damaged.

Calls in one process over the same bytes, with the same arguments and
in the same order, SHALL leave the same bytes in every block and give
the same answers.

#### Scenario: Every damage against every call
- **WHEN** a valid table is damaged in one place, from a list that
  covers every field of the header, a bucket, the next, hash, NULL bits
  and length of a record, a used mark and the count of records, each
  with the values at and around its limits; then every call of the
  crate runs over it, and more calls after the first error
- **THEN** each call ends with a result or the error of the rule the
  damage breaks, and the bytes around every block are as they were
- **Verified by:** pending

#### Scenario: Bytes a fuzzer chose
- **WHEN** a fuzzer builds tables from its input, damages them in one
  or more places and runs calls over them
- **THEN** no call panics, runs without end, or reads or writes outside
  its blocks
- **Verified by:** pending

#### Scenario: The checks hold for all values
- **WHEN** a header passes its checks, a reference passes its check, or
  a walk starts over a chain or a chunk, for all values of the fields,
  references, used marks and counts the table may hold
- **THEN** every bucket lies inside the index, every record inside its
  chunk, and every walk ends within the bound its blocks give
- **Verified by:** pending

#### Scenario: One sequence, twice
- **WHEN** the same sequence of calls runs twice, over two copies of the
  same bytes
- **THEN** the blocks and the answers of the two runs are equal
- **Verified by:** pending

### Requirement: Errors of two kinds
An error of a call SHALL be of one of two kinds, and SHALL name the
rule that was broken. What the call read decides the kind: a value the
caller passed makes a wrong call, a byte of the table's blocks a
damaged table, as "A damaged table" lists. A mask is refused as "The
masks of rows" says. A wrong call is:

- arrays of a batch of different row counts;
- keys or a payload that are not the table's, a key past the table's
  keys, or a table of fewer than 1 or more than 16 keys;
- payload columns past their limit or that do not fill the payload, or
  a payload word past the payload;
- a record that does not fit a chunk;
- a chunk that does not exist, is not aligned to 8, or is not a
  multiple of 8 from 8 bytes to 1 MiB long, or more than 32768 chunks;
- an index shorter than its header or not aligned to 8;
- a new index that is not a multiple of 8, cannot hold its buckets, is
  smaller than its capacity needs, or has fewer buckets than the old;
- a chunk split or merged into itself, or a split of no records;
- partitions that are not a power of two up to the limit, or whose
  bits pass the 32 bits of a hash;
- a Bloom filter of the wrong size or alignment, for more records than
  it may hold, or a shared one not built yet;
- a source of keys that cannot give a row's key;
- a slot of a payload cursor past its slots or holding no reference;
- a reference or a cursor the caller passed that names no chunk or lies
  outside its chunk, a cursor of a walk past its chunk's used mark, or
  an empty output of a walk;
- a count of rows of a partition that one more row would carry past
  2^64;
- an owning table that holds the most records a find or insert may
  make.

#### Scenario: Each damage names its rule
- **WHEN** one rule of the format is broken in a valid table, and every
  call that reads the damaged bytes runs over it
- **THEN** each of them fails with a damaged table and that rule
- **Verified by:** pending

#### Scenario: Each wrong argument names its rule
- **WHEN** a call is given one argument that breaks one rule of a wrong
  call
- **THEN** it fails with a wrong call and that rule; when the argument
  is a size, a count or a block, which a call checks before its first
  row, it fails before it changes anything
- **Verified by:** pending

#### Scenario: A wrong chunk given to a regrow or a cleared key
- **WHEN** a larger index or a cleared key is given chunks of which the
  last cannot be a chunk
- **THEN** the call fails with a wrong call and the rule of a chunk,
  and the index, the chunks and the keys are as they were
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::a_wrong_last_chunk_fails_the_calls_over_every_chunk_before_a_change`

### Requirement: No memory taken
A call over blocks the caller gives SHALL allocate no memory, also when
it fails: the memory of the table, of the arguments and of the results
is the caller's. The crate's table that owns its memory, for tests and
simple uses, allocates its blocks and is outside this promise; a build
without the crate's feature `alloc` leaves it out.

#### Scenario: Calls under a counting allocator
- **WHEN** every call of the crate runs over the caller's blocks, with
  success and with each kind of error, under an allocator that counts
- **THEN** the count does not change
- **Verified by:** pending

#### Scenario: A build without the owning table
- **WHEN** the crate is built and tested without its feature `alloc`
- **THEN** it builds, its tests that need no owning table run, and it
  has no table that owns its memory
- **Verified by:**
  `Makefile::test --locked -p relhash --no-default-features`

## MODIFIED Requirements

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

A source of keys SHALL give 0 in the slot of every key it marks NULL:
the table stores and compares keys slot by slot, so another value would
split the group of NULL keys. A build with debug assertions SHALL check
it.

#### Scenario: The size of a record
- **WHEN** the size of a record is asked for 1 to 16 keys and payloads
  of 0 to 1024 bytes
- **THEN** it is 16 bytes, 8 a key and the payload, rounded up to 8;
  0 and 17 keys are refused
- **Verified by:**
  `crates/relhash/src/table/tests/format.rs::a_record_takes_its_header_its_key_slots_and_its_payload`

#### Scenario: A record as a call reads it
- **WHEN** a record is read through `Table::record`
- **THEN** it has the row's hash, its keys' words, a NULL key as 0 with
  its bit set, and the row's payload
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_null_key_groups_apart_from_the_value_it_hashes_like`;
  `crates/relhash/src/table/tests/lookup.rs::inserted_rows_are_found_and_absent_keys_are_not`;
  `crates/tessera-capi/tests/table.rs::the_entry_points_round_trip`

#### Scenario: A record larger than a chunk
- **WHEN** a table is sized or created for a record that does not fit
  in a chunk of 1 MiB
- **THEN** the call fails, and the largest record that fits is accepted
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::a_record_larger_than_a_chunk_is_refused`

#### Scenario: A value left in a NULL slot
- **WHEN** a source marks keys NULL but leaves values in their slots,
  in a build with debug assertions
- **THEN** the call stops on the assertion
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::a_value_a_source_leaves_in_a_null_slot_is_caught`

### Requirement: Linking records
`Table::link` and `TableMut::link` SHALL link the records of a chunk
from a byte at a record's start up to the used mark, each first in its
bucket's chain, and SHALL move the byte past them. Records of equal
keys SHALL stay separate records. The count of records in the header
SHALL grow before any of them can be found. `link_counting` SHALL link
as `link` does and SHALL also count the records whose hash, NULL bits
and keys a record linked earlier already has; the sum of these counts
over the processes that link at once SHALL be exact.

`TableMut::link_grouped` SHALL link a chunk as `link` does, except that
a record whose hash, NULL bits and keys the table already holds SHALL
go right after the first such record of its chain, so that the records
of a key stand together. `Table::next_in_group` SHALL move each row to
the record right after its own when that record has the same hash,
NULL bits and keys.

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

A lookup, `Matches`, SHALL be one value: a mask of the rows still in it
and, for each row, the reference of its record. A mask and references
of different row counts SHALL be refused when the lookup is made.

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
  `crates/relhash/src/table/tests/lookup.rs::gather_key_reads_the_key_of_each_found_row`;
  `crates/tessera-capi/tests/table.rs::the_words_entry_point_gathers_every_column`

#### Scenario: A mask and references of other row counts
- **WHEN** a lookup is made of a mask of 70 rows and 69 references
- **THEN** it is refused
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::dimension_errors_come_before_any_change`

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

Building a Bloom filter of a table's records, alone or shared, and a
walk of the unmarked records of a whole table read the used mark of
every chunk. Over a table that several writers share they SHALL run
only once every writer's appends are done and ordered before them, as
by a barrier; the contracts of attaching a table and of making its
chunks SHALL say so.

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
  `crates/relhash/src/table/loom/table.rs::two_participants_build_a_table_and_find_every_key`;
  `crates/tessera-kernels/src/loom.rs::two_participants_append_size_link_and_probe`;
  `crates/tessera-kernels/src/loom.rs::three_participants_attach_at_any_phase`

#### Scenario: A filter built after the appends
- **WHEN** a filter is built over a shared table
- **THEN** the contracts of attaching the table and of making its
  chunks name the call and the barrier before it
- **Verified by:** review only — the contract forbids the race it
  names, so no run can show it

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

#### Scenario: The partition of a hash
- **WHEN** a chunk of 200 records is split into 4 partitions at shift 9
- **THEN** every record goes to the chunk of partition
  `(hash >> 9) & 3`
- **Verified by:**
  `crates/relhash/src/table/tests.rs::a_chunk_splits_into_the_chunks_of_its_partitions`

#### Scenario: Partitions past the limits are refused
- **WHEN** 0, 3 or 131072 partitions are given, 4 partitions at shift
  31, 1 partition at shift 32, or a chunk number past the table's
  chunks, to a split, an append by partition or a lookup by partition
- **THEN** the call fails, and the chunks and the masks are as they were
- **Verified by:**
  `crates/relhash/src/table/tests.rs::partitions_past_the_hash_or_the_chunks_are_refused`;
  `crates/tessera-capi/tests/table.rs::an_append_by_partition_refuses_partitions_past_the_limits`

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

`bloom::words_for` SHALL give, for `r` records, the least power of two
of words that is at least `max(1, ceil(16 r / 64))`, so 16 bits a
record at least. `bloom::words_within` SHALL give the same, halved
while its bytes are more than an eighth of `limit`, one word at least.
A count of records whose filter cannot be sized SHALL be refused. At 16
bits a record, a filter SHALL let fewer than one in 100 absent keys
through.

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

`bloom::add_atomic` SHALL set the bits of the hashes of the selected
rows in a filter without a state word, each word changed by an atomic
OR, so that several participants may add their rows at once. The caller
SHALL order every addition before any probe, by a barrier, and SHALL
then probe with `bloom::probe`. Its words SHALL be aligned to 8 and a
power of two.

#### Scenario: One participant builds the filter
- **WHEN** two participants in the model, four threads, or two calls of
  one process try to build a shared filter
- **THEN** exactly one builds it, a probe before it is ready fails, and
  once it is ready every key passes
- **Verified by:**
  `crates/relhash/src/table/loom/filter.rs::two_participants_race_to_build_the_filter_and_one_does`;
  `crates/relhash/src/table/tests.rs::one_of_four_threads_builds_a_shared_filter_every_key_passes`;
  `crates/relhash/src/table/tests/filter.rs::two_calls_of_one_process_build_a_shared_filter_once`;
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
  `crates/relhash/src/table/tests/filter.rs::a_shared_filter_of_the_wrong_shape_is_refused`;
  `crates/tessera-capi/tests/table.rs::the_filter_entry_points_size_fill_and_probe`

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
- **Verified by:**
  `crates/relhash/src/table/tests/filter.rs::a_filter_filled_by_one_participant_passes_its_rows`;
  `test/sql/table.sql::tessera_test_table_shared_spill`

### Requirement: A damaged table
A call SHALL return an error of a damaged table, and never panic or
loop without end, when the bytes it reads are not a table:

- a header with a field that breaks a rule of "The index of a table",
  the field named, or an index length below the header's;
- a reference read from the table that names no chunk, lies outside
  its chunk or names a byte where no record starts;
- a record, whoever named it, whose size is not the table's;
- a used mark that ends no record;
- a chain longer than the count of records;
- a header of a grown index that no longer matches the table it grew
  from.

A walk down a chain SHALL take no more steps than the header's count of
records, and no more than the places a reference can name in the chunks
it is given, 2^17 a chunk. A call SHALL check its arguments, the header
and the count of chunks before it changes anything, and a chunk before
it reads or writes it.

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

## REMOVED Requirements

### Requirement: The records of a key together
**Reason**: One topic with "Linking records", which takes its text and
its scenarios.
**Migration**: "Linking records".

### Requirement: Splitting a chunk by partition
**Reason**: One topic with "Partitions of a table", which takes its text
and its scenarios.
**Migration**: "Partitions of a table".

### Requirement: The size of a Bloom filter
**Reason**: One topic with "A Bloom filter of the keys", which takes its
text and its scenarios.
**Migration**: "A Bloom filter of the keys".

### Requirement: A filter filled together
**Reason**: One topic with "A shared Bloom filter", which takes its text
and its scenarios.
**Migration**: "A shared Bloom filter".

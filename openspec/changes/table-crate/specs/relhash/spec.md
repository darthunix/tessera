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

### Requirement: A crate of its own
The table SHALL be the Rust crate `relhash`, which depends on no crate
of Tessera. The crate SHALL build, pass its tests and pack alone, on
targets whose pointers are 64 bits wide; on any
other target it SHALL fail to build, with an error that says why.

#### Scenario: The crate packs alone
- **WHEN** the crate is packed, as for publishing
- **THEN** it builds from the packed files and the crates it names,
  none of them Tessera's
- **Verified by:** pending

#### Scenario: The crate's tests run alone
- **WHEN** the tests of the crate run as the only package
- **THEN** they pass
- **Verified by:** pending

#### Scenario: A target of 32 bits
- **WHEN** the crate is built for a target whose pointers are 32 bits
  wide
- **THEN** the build fails with an error that names 64-bit targets
- **Verified by:** pending

### Requirement: The documentation of the crate
Every public item of the crate SHALL have a text that says what it is
for, what it takes and gives, who owns what and for how long, and the
kind of error it returns. The documentation SHALL build without
warnings, and every example in it SHALL run as a test.

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

### Requirement: A lookup narrows one mask
A lookup SHALL be one value: a mask of the rows still in it and, for
each row, the reference of its record. A mask and references of
different row counts SHALL be refused when the lookup is made. A
probe, a step to the next match or to the next in a group, and a Bloom
filter's probe SHALL keep in the mask the rows that find what they
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
- **Verified by:** pending

#### Scenario: A chain longer than a short walk
- **WHEN** 300 records of one hash are linked, and their keys and an
  absent key are probed row by row and by whole words, their matches
  are followed, and a find or insert looks for a known and a new key
- **THEN** every known key is found at the end of its walk, the absent
  key finds nothing, the matches visit all 300 records, and no call
  takes the chain for a cycle
- **Verified by:** pending

#### Scenario: A mask and references of other row counts
- **WHEN** a lookup is made of a mask of 70 rows and 69 references
- **THEN** it is refused
- **Verified by:**
  `crates/relhash/src/table/tests/lookup.rs::dimension_errors_come_before_any_change`

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
- **Verified by:** pending

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

### Requirement: The calls over every chunk
Building a Bloom filter of a table's records, alone or shared, and a
walk of the unmarked records of a whole table read the used mark of
every chunk. Over a table that several writers share they SHALL run
only once every writer's appends are done and ordered before them, as
by a barrier; the contracts of attaching a table and of making its
chunks SHALL say so.

#### Scenario: A filter built after the appends
- **WHEN** a filter is built over a shared table
- **THEN** the contracts of attaching the table and of making its
  chunks name the call and the barrier before it
- **Verified by:** review only — the contract forbids the race it
  names, so no run can show it

### Requirement: Keys from a source
A source of keys SHALL give 0 in the slot of every key it marks NULL:
the table stores and compares keys slot by slot, so another value would
split the group of NULL keys. A build with debug assertions SHALL check
it.

#### Scenario: A value left in a NULL slot
- **WHEN** a source marks keys NULL but leaves values in their slots,
  in a build with debug assertions
- **THEN** the call stops on the assertion
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::a_value_a_source_leaves_in_a_null_slot_is_caught`

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

### Requirement: Errors of two kinds
An error of a call SHALL be of one of two kinds, and SHALL name the
rule that was broken. What the call read decides the kind: a value the
caller passed makes a wrong call, a byte of the table's blocks a
damaged table. A mask is refused as "The masks of rows" says.

- **A wrong call**: the arrays of a batch have different row counts;
  keys or a payload that are not the table's; a key past the table's
  keys; a table of fewer than 1 or more than 16 keys; payload columns
  past their limit or that do not fill the payload, or a payload word
  past the payload; a record that does not fit a chunk; a chunk that
  does not exist, is not aligned to 8, or is not a multiple of 8 from
  8 bytes to 1 MiB long; more than 32768 chunks; an index shorter than
  its header or not aligned to 8; a new index that is not a multiple of
  8, cannot hold its buckets, is smaller than its capacity needs, or
  has fewer buckets than the old; a chunk split or merged into itself;
  a split of no records; partitions that are not a power of two up to
  the limit, or whose bits pass the 32 bits of a hash; a Bloom filter
  of the wrong size or alignment, for more records than it may hold,
  or a shared one not built yet; a source of keys that cannot give a
  row's key; a slot of a payload cursor past its slots or holding no
  reference; a reference or a cursor the caller passed that names no
  chunk or lies outside its chunk, or a cursor of a walk past its
  chunk's used mark, or an empty output of a walk; a count of rows of
  a partition that one more row would carry past 2^64; an owning table
  that holds the most records a find or insert may make.
- **A damaged table**: a header with a field that breaks its rule, the
  field named; a reference read from the table that names no chunk or
  lies outside its chunk; a record, whoever named it, whose size is not
  the table's; a used mark that ends no record; a chain longer than the
  count of records; a header of a grown index that no longer matches
  the table it grew from.

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
- **Verified by:** pending

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
- **Verified by:** pending

### Requirement: The same calls, the same bytes
Calls in one process over the same bytes, with the same arguments and
in the same order, SHALL leave the same bytes in every block and give
the same answers.

#### Scenario: One sequence, twice
- **WHEN** the same sequence of calls runs twice, over two copies of the
  same bytes
- **THEN** the blocks and the answers of the two runs are equal
- **Verified by:** pending

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
- **Verified by:** pending

#### Scenario: Threads with handles of their own
- **WHEN** two threads, each with its own handle, append, link and
  probe one table in every order of their steps
- **THEN** every key is found
- **Verified by:** pending

### Requirement: Records by reference
A call SHALL give a cursor that reads the record named by a reference,
and the one writer of a table a cursor that changes the payload of the
record named by a reference. Each reference SHALL be checked as a
lookup checks it, and only one payload SHALL be open for change at a
time. A writer SHALL be able to check a reference first and open its
payload later through the same cursor. When a reference fails its
check, the call fails; the payloads it changed before stay changed.
A call SHALL also give, for each selected row, a raw pointer to the
payload of its record, two rows of one record the same pointer, each
usable to read and write until the chunk is reset, after the call and
beside the others, within Rust's rules of aliasing.

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

#### Scenario: Pointers to the payload of one record
- **WHEN** two rows of a batch name one record, the call gives their
  pointers, and the payload is written through each of them in turn
  after the call, under Miri, also through the C entry point
- **THEN** Miri reports no undefined behavior, and the payload holds
  the last write
- **Verified by:**
  `crates/relhash/src/table/tests/records.rs::pointers_to_the_payload_of_one_record_write_it_in_turn`;
  `crates/tessera-capi/tests/table.rs::the_writer_entry_points_round_trip`

#### Scenario: Two payloads open at once
- **WHEN** a program keeps one payload of a cursor open and opens
  another
- **THEN** it does not compile
- **Verified by:** pending

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

# The hash table in a borrowed region

Joins and grouping keep their state in a hash table that Tessera keeps in
a region of memory the C node owns. The table is implemented in Rust
(`crates/tessera-kernels`, module `table`) and reaches C through
`include/tessera/table.h`, in the static library of the kernels. This
guide is the C-side contract: where the region lives, what it holds, how
a batch goes in and comes out, how the table grows, and what several
processes may do at once.

## Why a region and offsets

A serial plan keeps the table in the memory of its query context and
grows it with `repalloc`; a parallel plan keeps the build side of a join
in dynamic shared memory, which every process maps at an address of its
own. One table serves both because it never holds an address: the caller
hands the region to every call as a pointer and a length, Rust keeps
nothing between calls and allocates nothing, and everything inside the
region refers to other parts by byte offsets from its start. The bytes
therefore stay valid after a move and mean the same in every process.

## The region

The region is aligned to 8 (`palloc` and DSA allocations are). It holds:

- a header of 96 bytes with a magic value and the format version
  (`TESS_TABLE_FORMAT_VERSION`), the region length, the key kinds, the
  payload size, the record size, the bucket count, and two counters that
  change as records come in: the end of the record area and the record
  count;
- the record area, filled upward from the header;
- the bucket array at the end of the region: a power of two of 32-bit
  slots, at least 1024 and at least twice the capacity the table was
  created for, each holding the offset of the first record of its chain.

Buckets sit at the end so that growth is `repalloc` followed by
`tess_table_grow`: the records stay where they are and the buckets are
rebuilt at the new end. A bucket is the high bits of the hash.

A record offset is a `uint32` in units of 8 bytes, so a region can reach
32 GiB; 0 means none, since the header lies there. A record holds 16
bytes of header (its hash, the offset of the next record of its bucket,
a bit per key that is NULL, its length in 8-byte units), then one 8-byte
slot per key, then the payload the table was created with, rounded up to
8. Keys are `int4` (sign-extended into the slot) or `int8`
(`TessTableKeyKind`), up to `TESS_TABLE_MAX_KEYS` of them; a NULL key
holds 0 in its slot and sets its bit. The payload is opaque: a join keeps
the Datums of its build row there, grouping its aggregate states.

## Creating and attaching

`tess_table_size(nkeys, kinds, payload_size, capacity, &size, &status)`
says how many bytes a table of `capacity` records needs: the header, the
records and the buckets, a multiple of 8. `tess_table_create(region, len,
nkeys, kinds, payload_size, capacity, &status)` lays the table out over
all `len` bytes (a multiple of 8 of at least that size), so a larger
region holds more records. `tess_table_attach` only checks that the
bytes hold a table of this format, and `tess_table_stats` reports the
record count, the bucket count, the bytes in use and the region length
for planning and `EXPLAIN`.

Every call attaches anew and checks the whole header: the magic and the
version, the sizes, that the buckets are a power of two inside the
region, that the record area lies between the header and the buckets.
Every offset a call follows is checked against the record area and the
record length, and a chain is walked at most as many steps as there are
records. A corrupt region is therefore a status, never a crash or a hang.
`tess_table_format_version` and `tess_table_layout` let a C test compare
the format and the structures with what the library was built with.

## A batch in and out

A batch brings three things: its hashes, one `uint32` per physical row,
from `tess_int4_hash`, `tess_int8_hash` and their `_next` forms, which
also apply the NULL policy (`TESS_NULL_KEYS_REJECT` drops NULL keys from
the mask, for joins; `TESS_NULL_KEYS_GROUP` keeps them as a key of their
own, for grouping); its keys, one `TessTableKey` per key of the table, a Datum
column read by its kind with the readiness mask of the batch contract;
and a row mask. An int8 inside the int4 range hashes as the int4 and
both are stored as 8-byte slots, so an int4 key column may insert into or
probe a table whose records came from int8 keys, as a join of an int4
column with an int8 one does.

`tess_table_insert(region, len, hashes, nkeys, keys, payload, &pending,
offsets, &status)` inserts the rows of `pending` in row order, always as
new records, so that equal keys chain in their bucket. Each row inserted
leaves `pending` and gets the offset of its record in `offsets`;
`payload` is the payload of every physical row one after another, or
`NULL` for zeros. A full table is not an error: the rows still in
`pending` need a larger region, and the caller grows it and calls again
with the same mask.

`tess_table_probe(region, len, hashes, nkeys, keys, &rows, matches,
&found, &status)` finds, for each row of `rows`, the first record of its
chain with its hash, NULL bits and keys: `matches[row]` gets the offset and `found`,
a mask the call fills whole, the rows that have one. Keys are compared
whole: the hash alone cannot decide, since under the group policy a NULL
key hashes like the value `0x9e3779b9`, and int8 keys have no bijection.
Equal keys have separate records, so `tess_table_next_match(region, len,
offsets, &rows, &found, &status)` replaces each row's offset in place by
the next record of its chain with the same keys, until `found` is empty:
a join walks the chains of a whole batch of probe rows at a time. A table
filled by `tess_table_insert_grouped` (below) keeps a key's records next
to each other, and `tess_table_next_in_group(region, len, offsets, &rows,
&found, &status)` steps to the next one by looking at the record right
after a row's own only: one step, where `tess_table_next_match` walks the
rest of the chain to find that no other record of the key is there.

`tess_table_gather(region, len, offsets, &rows, at, values, &status)`
reads, for each row of `rows`, the 8 bytes at byte `at` of the payload of
the record at `offsets[row]` into `values[row]`; other rows keep their
values, and `at + 8` must lie within the payload. A join keeps the Datums
of its build row as payload words and fetches one column of a batch of
matches per call, with one check of the header, where a call per row
would check it per row.

`tess_table_gather_key(region, len, offsets, &rows, key, values, isnull,
&status)` reads key `key` of each row's record the same way, as its Datum
(an int4 key sign-extended, as `Int32GetDatum` makes it) and its NULL flag
from the record's NULL bits: a grouped aggregate returns its groups' keys
with it.

`tess_table_record(region, len, offset, &record, &status)` exposes a
record's hash, NULL bits, key slots and payload as pointers into the
region, valid until the region moves or the table grows.

## One writer

Grouping, output and growth need the region to themselves, with no other
call over it at the same time:

- `tess_table_find_or_insert(region, len, hashes, nkeys, keys, &pending,
  offsets, &inserted, &status)` gives each pending row the record of its
  keys, creating one with a zero payload where none exists, in row order,
  until no new record fits; `inserted` receives the rows whose record the
  call created, so the caller initializes their aggregate states;
- `tess_table_insert_grouped(region, len, hashes, nkeys, keys, payload,
  &pending, offsets, &duplicates, &status)` inserts the rows as
  `tess_table_insert` does, but each right after a record with the same
  keys when the table holds one, so that a key's records lie together in
  their chain; `duplicates`, which the call fills whole, receives the rows
  whose keys were there already. It looks every row up, which is why it
  belongs to one writer: a join builds its table with it, and a table
  without duplicates needs no second round at all;
- `tess_table_payload(region, len, offset, &payload, &status)` hands out a
  payload to change in place;
- `tess_table_accumulate(region, len, offsets, &rows, op, column, prepared,
  value_at, flags_at, flag_bit, &status)` folds each selected row into the
  aggregate state of its record, the offsets `tess_table_find_or_insert`
  gave: `count(*)` and `count(x)` add one to the int8 at byte `value_at`;
  `sum(int4)`, `min` and `max` of int4 or int8 take the row's non-NULL
  value, the first one also setting bit `flag_bit` of the word at byte
  `flags_at`, so a state without the bit has seen no value and stands for
  NULL. The rows of a batch go in row order, several of one group in turn,
  so a sum past the int8 range fails with 22003 "bigint out of range"
  where the row-wise transition would; one check of the header per batch,
  as for `tess_table_gather`;
- `tess_table_scan(region, len, &cursor, offsets, capacity, &count,
  &status)` visits the records in insertion order, up to `capacity` per
  call, from a cursor the caller starts at 0 and keeps between calls; a
  count of 0 ends the walk;
- `tess_table_grow(region, len, &status)` follows `repalloc` (or a copy
  into a new region): the table takes the whole new length, keeps its
  records and their offsets, and rebuilds the buckets at the new end for
  the records that could now fit, with the bucket count `tess_table_size`
  would have chosen for them; a record goes right after an earlier one
  with the same keys, so the records of a key stay together.

A walk reads every record below the used mark, which an insertion in
flight would have reserved but not written; that is why it belongs to the
one writer.

## A Bloom filter of the keys

A probe that finds no record still reads a bucket, and a record too when
the bucket holds another key; on a table past the cache these are cache
misses. A join whose rows mostly find no pair can check them first against
a Bloom filter of the table's keys, which rejects most of those rows
without touching the table. The filter is a blocked one: one 64-bit word
per key, four bits in it, both taken from the row hash (the same hash the
table uses, with every key and the NULL policy) multiplied by
`0x9E3779B97F4A7C15`, the word from the high bits of the product and the
bits from its low 24, so the word does not repeat the bucket index. A
check reads one word and compares it with a four-bit mask. The size is a
power of two words, 16 bits per record, which lets about 1 % of absent
keys through; a key of the table always passes.

Like the table, the filter lives in a borrowed buffer of words with no
process addresses in it, so it may later sit in shared memory next to a
shared table:

- `tess_table_bloom_words(records, &nwords, &status)` gives the size for
  a number of records;
- `tess_table_bloom(region, len, words, nwords, &status)` clears the
  words and sets the bits of every record of the table; it walks the
  records, so it belongs to the one writer, like a scan;
- `tess_bloom_probe(words, nwords, hashes, &rows, &found, &status)` fills
  `found` whole with the rows of `rows` whose bits are all set; the two
  masks must not share words.

## Several participants

Over shared memory, insertions may run in several processes at once, and
so may probes, but not both at a time: a join builds, passes a barrier,
then probes. An insertion reserves the room of a word's rows with one
compare-and-swap on the end of the record area, writes each record in
its reserved bytes and publishes it with a compare-and-swap of its
bucket's head (release), which a probe reads with acquire; the record
count grows before the records are published, so that a probe that finds
one also sees a count that covers its chain, whose length it checks
against the count. A published record never changes, except its payload
under the one writer. The same code runs over local memory, where the
compare-and-swaps never fail.

`make rust-loom` runs the table's own code over a model region of loom
atomics (`crates/tessera-kernels/src/table/loom.rs`) with the orderings
the real region uses: two and three threads inserting into one bucket,
two records reserved at once per thread, a full table taking one record
of two, and a probe that finds a record another thread is publishing and
reads it whole. Every access to a record's bytes is announced to loom
first, so a read not ordered after the writing is reported; a test with
relaxed bucket heads checks that the model does report it. The model
found that counting the records after publishing them let such a probe
call a chain corrupt. The TLA+ specification of the
build-barrier-probe protocol comes with the parallel join (plan item
5.5).

## Ownership and errors

Entry points borrow the region and the batch's buffers and own nothing.
The status rules of `tessera/kernels.h` apply: a dimension, pointer or
header error comes before any change; after a failure the mutable outputs
of the call (masks, offsets) hold unspecified values, and the caller
reports the status with `ereport` after the call returns. Buffers must
not alias: a mask a call fills must not be the mask it reads.

## Tests and measurements

`crates/tessera-kernels/tests/table.rs` covers the format and every
operation, including corrupt headers, chains and offsets, and runs under
Miri; `crates/tessera-capi/tests/table.rs` compares Datum and dense key
columns and calls the entry points as C would; `test/tessera_table_test.c`
is the C test module, run by `make installcheck` as the `table` suite,
which checks the layout probes against `sizeof` and `offsetof` and runs a
batch through creation, insertion, probing, grouping, growth by
`repalloc` and the error statuses. The benchmark `table_int32`
(`crates/tessera-capi/benches/README.md`) measures insertion, probes and
find-or-insert on PMU counters against a chained table with plain stores. The
benchmark `table_large` adds a table past the cache, where the groups
`hit`, `miss` and `occupied` also time the Bloom filter check alone
(`bloom_probe`) and with the probe of the rows it passes
(`bloom_then_probe`).

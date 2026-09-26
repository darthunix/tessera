# Temporary storage for the hash nodes

TessHashJoin and TessAgg keep their state in the hash table of
[table.md](table.md). When the table would outgrow `hash_mem`, a node
writes parts of it to PostgreSQL's temporary files and reads them back
later (plan item 5.6). This guide describes what goes to disk and how it
is checked; the nodes' policies (partitions, recursion, what stays in
memory) are in [nodes.md](nodes.md).

## What is written

A node spills chunks of its table whole, never rows one by one:

- a chunk of **records**, in the table's own format: its 8-byte used
  mark, then the records one after another, each with its hash, keys and
  payload (the `next` field is left as it was: a new index relinks the
  records when the chunk is read back);
- a chunk of **values**: the by-reference inner values the records refer
  to. A payload word holds a value's reference, the value chunk's number
  plus one and the byte in it, not an address, so the words mean the same
  after the chunk has been on disk; a reading node sets the chunk's base
  in its array of value bases under the chunk's number.

Reading a chunk back gives a chunk that is ready at once: no row is
decoded or allocated, and `tess_table_link` or `tess_table_link_grouped`
puts its records into a new index. PostgreSQL's own spilling writes a
tuple at a time (its hash, then the minimal tuple) and allocates each on
reading.

## The block header

Each chunk goes to disk as a header of `tess_spill_header_size()` bytes
(48) and then the chunk's bytes (`include/tessera/spill.h`,
`crates/tessera-spill`):

```
 0  magic "TESSSPIL"    8  version = 1    12  kind (1 records, 2 values)
16  chunk number       20  partition     24  level (below 32)   28  reserved = 0
32  table fingerprint  40  body length (a multiple of 8; at least 8 for records)
```

`tess_spill_header_read` checks the magic, the version, the kind, the
reserved word, the body length against the most the reader accepts, and
the fingerprint against the reading table's (`tess_table_fingerprint`: a
hash of the table format, the key kinds and the record and payload
sizes), so that a block of another table or a damaged file is an error
status, never a record read the wrong way; the records themselves are
then checked as any chunk is, when the table attaches them. The header is
in native byte order and carries no checksum: temporary files are read by
the query that wrote them, on the same machine, as PostgreSQL's are.

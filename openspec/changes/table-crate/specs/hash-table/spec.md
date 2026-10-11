## ADDED Requirements

### Requirement: The used mark of a chunk
`tess_table_chunk_used` SHALL give the used mark of a chunk the caller
holds, from its base, its length and the size of the table's records.
It SHALL check the mark first: the block can be a chunk, the record
size is a multiple of 8 of at least 16 bytes, and the mark is at least
8, at most the chunk's length, and 8 more than a whole number of
records. A mark that fails is refused, and `used` is left as it was.
The call reads the first 8 bytes of the chunk and nothing else.

#### Scenario: The mark of a chunk
- **WHEN** an empty chunk of 64 bytes is asked for its mark, and then
  a chunk of 128 bytes for records of 32 bytes that holds three of them
- **THEN** the first mark is 8 and the second 104
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_mark_of_a_chunk`

#### Scenario: A mark that ends no record
- **WHEN** a chunk of 64 bytes for records of 32 bytes holds a mark of
  0, 20 or 72; or a record size of 0 or 12 is given; or a block that
  is not aligned to 8
- **THEN** the call fails, and `used` keeps its value
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::a_mark_that_ends_no_record`

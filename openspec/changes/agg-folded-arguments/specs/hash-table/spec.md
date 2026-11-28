## MODIFIED Requirements

### Requirement: Merging groups read back
`tess_table_combine` SHALL merge the records of chunk `source`, from
byte `*from` on, into the table. Each record is a group: its keys, then
a payload of `n` aggregates, as many as the call is given, within the
table's payload:

```
 offset    size     field
 0         8 * w    the words of flags, w = max(1, (n + 63) / 64):
                    the bit i % 64 of word i / 64 set once aggregate
                    i has a value
 8 * w     8 * n    a word for each aggregate, aggregate i at
                    8 * (w + i)
```

A payload of 64 aggregates or fewer thus holds one word of flags. The
record of the same keys in the table SHALL take each state in as
`combines[i]` says:

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
`TESS_TABLE_COMBINE_DONE` (0). A payload too small for its words of
flags and its aggregates, an unknown kind of merge and a source that is
`chunk` SHALL be refused before any change. A merge that fails after
these checks leaves some states merged and some groups copied, and the
caller uses the table no more. The source is not linked: the caller
keeps it out of a regrow, which links every record of every chunk it is
given.

#### Scenario: The states of a group merge
- **WHEN** groups of a count, a sum, a minimum and a maximum, with every
  combination of flags, are merged into a table that holds some of them
- **THEN** each state is as the rules give, and the groups the table
  lacked are copied and found
- **Verified by:**
  `crates/tessera-kernels/src/accumulate.rs::states_of_a_group_merge`;
  `test/sql/table.sql::tessera_test_table_combine`

#### Scenario: Groups of more than 64 aggregates merge
- **WHEN** groups of 65 and of 130 aggregates of every kind, with flags
  set and clear in each word of flags, are merged into a table that
  holds some of them
- **THEN** each state is as the rules give, by its flag in its own word,
  and the groups the table lacked are copied and found
- **Verified by:**
  `crates/tessera-kernels/src/accumulate.rs::groups_of_more_than_64_aggregates_merge`;
  `crates/tessera-capi/tests/table.rs::groups_of_more_than_64_aggregates_merge`

#### Scenario: A merge stops for a chunk or a larger index
- **WHEN** the new groups of a source need more room than `chunk` has,
  and then more records than half the buckets
- **THEN** the call stops with `CHUNK_FULL`, then with `INDEX_FULL`, its
  `*from` on the first group not merged, and goes on from there with a
  new chunk and a larger index to `DONE`
- **Verified by:**
  `crates/relhash/src/table/tests.rs::a_merge_stops_for_a_chunk_or_a_larger_index`

#### Scenario: A merge that cannot be done fails
- **WHEN** a count or a sum would pass the int8 range, or a merge has a
  payload too small for its words of flags and its aggregates, an
  unknown kind or a source that is `chunk`
- **THEN** the first fails with status
  `TESS_ERROR_INTEGER_OUT_OF_RANGE` and SQLSTATE `22003`, the others
  with `TESS_ERROR_INVALID_ARGUMENT` before any change
- **Verified by:**
  `crates/tessera-kernels/src/accumulate.rs::a_merged_count_past_the_int8_range_fails`;
  `crates/tessera-kernels/src/accumulate.rs::a_payload_short_of_its_states_is_refused`;
  `crates/tessera-capi/tests/table.rs::a_merge_that_cannot_be_done_fails`

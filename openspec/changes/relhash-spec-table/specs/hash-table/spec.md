## ADDED Requirements

### Requirement: The C API over the crate
Each call of `tessera/table.h` over a table SHALL do what the calls of
the crate `relhash` do, as the requirement of the spec of `relhash`
named here says, and SHALL return the crate's error as a status, as
"Errors of a call" says:

- `tess_table_format_version`, `tess_table_size`, `tess_table_create`
  and `tess_table_stats`: "The index of a table";
- `tess_table_chunk_init`, `tess_table_ref`, `tess_table_ref_chunk`
  and `tess_table_ref_byte`, which make a reference and split it:
  "Chunks and references";
- `tess_table_record_size`, `tess_table_record_bytes` and
  `tess_table_record`: "A record";
- `tess_table_append` and `tess_table_append_columns`, which take the
  index or none and without one append as a chunk writer does:
  "Appending rows";
- `tess_table_link`, which counts duplicates when asked: "Linking
  records";
- `tess_table_probe`, `tess_table_next_match`, `tess_table_gather`,
  `tess_table_gather_scattered`, `tess_table_gather_words`,
  `tess_table_gather_key` and `tess_table_payloads`: "Looking rows up";
- `tess_table_link_grouped` and `tess_table_next_in_group`: "The
  records of a key together";
- `tess_table_find_or_insert`, `tess_table_regrow`, `tess_table_scan`
  and `tess_table_clear_key`: "Calls of one writer".

Beside them the C API SHALL do what the crate leaves to its caller:

- the key columns of a call SHALL put each value into its word, an
  `int4` widened with its sign and an `int8` as it is, so that an
  `int4` key and an `int8` key of the same value match;
  `tess_table_gather_key` SHALL give a key's word as a Datum with its
  NULL flag;
- a lookup SHALL take a mask of rows and a mask of results, and SHALL
  fill the mask of results whole with the rows the crate's call keeps;
- a debug build of the entry points SHALL check every chunk it is
  given;
- `tess_table_memory_limit` SHALL return a limit of memory as it is up
  to `TESS_TABLE_MEMORY_MAX`, 16 GiB, half of what the most chunks of
  the longest length hold, and that maximum for a larger limit;
- over a shared table the counters of a build, the words of a spill
  and the marks SHALL change by atomic operations too, and a barrier
  SHALL order what one participant did before the next phase of the
  others.

#### Scenario: Keys of int4 and int8 through C
- **WHEN** a record is read through `tess_table_record`, and rows with
  `int4` keys probe the records of `int8` keys and the other way round
- **THEN** the record has its `int4` key widened with its sign, and each
  row finds the record of its value
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::the_entry_points_round_trip`;
  `crates/tessera-capi/tests/table.rs::int4_keys_find_the_records_of_int8_keys`;
  `crates/tessera-capi/tests/table.rs::int8_keys_find_the_records_of_int4_keys`

#### Scenario: A debug build checks every chunk
- **WHEN** a probe that reads no chunk is given a chunk longer than
  1 MiB
- **THEN** in a debug build of the entry points it fails, and in a
  release build it goes on
- **Verified by:**
  `crates/tessera-capi/tests/table.rs::a_debug_build_checks_every_chunk`

#### Scenario: The most memory of a table
- **WHEN** limits of 0, 8 MiB and one byte under 16 GiB are given, and
  of 16 GiB, one byte over it, 64 GiB and the largest size
- **THEN** the first three come back as they are and the others as
  16 GiB, which fills half of the 32768 chunks of 1 MiB
- **Verified by:** `test/sql/table.sql::tessera_test_table_memory_limit`

## REMOVED Requirements

### Requirement: The index of a table
**Reason**: The table is the crate `relhash`; the requirement moves to
its spec, in the crate's terms.
**Migration**: The spec of `relhash`, "The index of a table"; the C
calls follow it as "The C API over the crate" says.

### Requirement: Chunks and references
**Reason**: The table is the crate `relhash`; the requirement moves to
its spec, in the crate's terms. The check of every chunk by a debug
build and the limit of memory stay here, in "The C API over the crate".
**Migration**: The spec of `relhash`, "Chunks and references".

### Requirement: A record
**Reason**: The table is the crate `relhash`; the requirement moves to
its spec, in the crate's terms. How the C key columns fill the words
stays here, in "The C API over the crate".
**Migration**: The spec of `relhash`, "A record".

### Requirement: Appending rows
**Reason**: The table is the crate `relhash`; the requirement moves to
its spec, in the crate's terms.
**Migration**: The spec of `relhash`, "Appending rows".

### Requirement: Linking records
**Reason**: The table is the crate `relhash`; the requirement moves to
its spec, in the crate's terms.
**Migration**: The spec of `relhash`, "Linking records".

### Requirement: Looking rows up
**Reason**: The table is the crate `relhash`; the requirement moves to
its spec, in the crate's terms. The masks of results, a key as a Datum
and the match of `int4` and `int8` keys stay here, in "The C API over
the crate".
**Migration**: The spec of `relhash`, "Looking rows up".

### Requirement: The records of a key together
**Reason**: The table is the crate `relhash`; the requirement moves to
its spec, in the crate's terms.
**Migration**: The spec of `relhash`, "The records of a key together".

### Requirement: Calls of one writer
**Reason**: The table is the crate `relhash`; the requirement moves to
its spec, in the crate's terms.
**Migration**: The spec of `relhash`, "Calls of one writer".

### Requirement: Calls at the same time
**Reason**: The table is the crate `relhash`; the requirement moves to
its spec, in the crate's terms. The atomic counters, spill words and
marks beside the table stay here, in "The C API over the crate".
**Migration**: The spec of `relhash`, "Calls at the same time".

## ADDED Requirements

### Requirement: The partitions of a level
`tess_spill_partitions` SHALL give the number of partitions of a level
of a table that spills. It SHALL start from `min_partitions` and double
the number while all of these hold:

- the number is below `max_partitions`;
- the number is below `at_least`, or the partitions, each holding half
  of `limit`, hold fewer bytes than `expected`;
- twice the number times `reserve`, what each partition keeps in memory
  at the least, fits in half of `limit`;
- the bits of the doubled number, after the `shift` bits the levels
  above take, stay below 32.

`min_partitions` and `max_partitions` SHALL be powers of two, the first
no larger. Expected bytes below zero or not a number SHALL be refused,
and so SHALL a `shift` past which `min_partitions` do not fit in the 32
bits of the hash.

#### Scenario: By the bytes, the participants and the reserve
- **WHEN** a level of 4 to 1024 partitions with a reserve of 48 KiB
  expects 10 MiB in a limit of 4 MiB, then wants 16 partitions at
  least, then has a limit of 256 KiB
- **THEN** it gets 8 partitions, then 16, then 4
- **Verified by:** pending

#### Scenario: By the bits of the hash
- **WHEN** a level of 4 to 1024 partitions that expects far more than
  its limit comes after 28, 29, 30 and 31 bits
- **THEN** it gets 8, 4 and 4 partitions, and the last is refused
- **Verified by:** pending

#### Scenario: Wrong bounds and bytes
- **WHEN** the least or the most partitions are not powers of two or
  are out of order, or the expected bytes are below zero or not a
  number
- **THEN** the call fails, SQLSTATE `XX000`
- **Verified by:** pending

### Requirement: The chunks of a level
`tess_spill_chunk_len` SHALL give the length of a level's chunks:
`limit` divided by `share` times the partitions, no more than
`max_chunk`, then no less than `min_chunk`, which wins over
`max_chunk`, rounded down to a multiple of 8. A `share` or a number of
partitions of 0, and a `min_chunk` below 8 or not a multiple of 8,
SHALL be refused.

#### Scenario: A share of the limit
- **WHEN** at a share of 16, with chunks of 8 KiB to 1 MiB, 4 MiB is
  shared among 8 partitions and among 1024, and 1 GiB among 4; and
  1000000 bytes among 8, with chunks of 8 bytes to 1 MiB
- **THEN** the chunks are 32 KiB, 8 KiB and 1 MiB, and 7808 bytes
- **Verified by:** pending

#### Scenario: Wrong shares and bounds
- **WHEN** the share or the partitions are 0, or the least chunk is 4
  or 4100 bytes
- **THEN** the call fails, SQLSTATE `XX000`
- **Verified by:** pending

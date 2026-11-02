## MODIFIED Requirements

### Requirement: When a hash join prunes its outer side
A hash join of Tessera SHALL prune the partitions of its outer side by
the keys of its table when all of the following hold, and SHALL not
otherwise:

- `enable_partition_pruning` is on;
- the outer side is a TessAppend over the partitions of one partitioned
  table;
- the join is INNER, SEMI, RIGHT or RIGHT ANTI, which return no outer
  row without a pair; LEFT, ANTI and FULL joins never prune;
- a key of the join has one of the types int2, int4, int8, date, bool,
  timestamp and timestamptz, and its outer side is a column that is the
  first column of the partition key of the table, or of one of its
  partitioned partitions, compared by an operator of that key's family;
  a hash partitioning's key has that column alone.

When several keys can prune, the first one in the join's order SHALL.

#### Scenario: Joins that keep an outer row without a pair
- **WHEN** SEMI, RIGHT, RIGHT ANTI, LEFT and ANTI joins have keys that
  reach one partition
- **THEN** the SEMI, the RIGHT and the RIGHT ANTI join prune the
  others, the LEFT and the ANTI join read them all, and the results are
  the core's
- **Verified by:**
  `test/sql/join.sql::SEMI and RIGHT keep no outer row without a pair and prune`;
  `test/sql/join.sql::RIGHT ANTI keeps the inner rows without a pair, not the outer ones`

#### Scenario: The key that prunes
- **WHEN** a join has two keys, the partition key second; a join's key
  is the second column of a partition key; and a join prunes with
  `enable_partition_pruning` off
- **THEN** the first prunes by its second key, and the others prune
  nothing
- **Verified by:**
  `test/sql/join.sql::The key that prunes: the first that can, here the second`

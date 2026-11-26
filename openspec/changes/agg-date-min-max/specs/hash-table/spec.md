## MODIFIED Requirements

### Requirement: The SQL types of a key
A join and a grouping SHALL put each column of a key into the table in
one of two forms, by the column's type, and SHALL leave a key of any
other type to the core:

- the value itself: `int2`, `int4`, `date` and `bool` as kind 1, and
  `int8`, `timestamp`, `timestamptz`, `time` and `money` as kind 2,
  since their values are equal exactly when their Datums are;
- a number of 64 bits that stands for the value, as kind 2, for any
  other type whose equality is the type's default one and has a hash
  function, such as `text`, `numeric`, `float8` or `interval`: in a
  grouping, the value's number in a dictionary of the node, which finds
  equal values by the type's own hash and equality; in a join, the
  value's hash from the type's own function, and the type's equality
  then decides every pair the table finds.

A key of a type without a hash function, such as `bit varying`, a join
by `money`, whose equality PostgreSQL does not let a hash join use, and
a join by the equality of two types other than the integers, such as
`float4` with `float8`, SHALL leave the grouping or the join to the
core's node.

#### Scenario: Keys whose values a word holds
- **WHEN** a join or a grouping has keys of `int2`, `date`, `bool`,
  `timestamp` or `timestamptz`, negative and NULL among them, or joins
  `int2` with `int4` and `int8`
- **THEN** Tessera's node runs it, and its rows are the core's
- **Verified by:**
  `test/sql/types.sql::Keys the table keeps in a word besides int4 and int8`

#### Scenario: Keys of time and money in a word
- **WHEN** a grouping or a join has keys of `time`, or a grouping has
  keys of `money`, of both signs, the edges of their ranges and NULL
  among them
- **THEN** Tessera's node runs it, a join's plan shows no join filter
  for the key, and the rows are the core's
- **Verified by:** pending

#### Scenario: Keys through a number for the value
- **WHEN** a grouping or a join has keys of `text` or `numeric`, with
  numeric 1.0 and 1.00, float8 -0 and 0, and text of either case under
  a case-insensitive collation
- **THEN** Tessera's node runs it, a join's plan shows the equality as
  its join filter, equal values meet though their bytes differ, and the
  rows are the core's
- **Verified by:**
  `test/sql/types.sql::Keys a word does not hold: text, numeric`;
  `test/sql/types.sql::Hash joins by keys a word does not hold`;
  `test/sql/types.sql::Equal values of other forms`

#### Scenario: A key through a number, on disk
- **WHEN** a join by a key of `text` that is an expression its child
  computes, such as `lower(t)`, spills: in one process, in each
  participant's table, and in a shared table
- **THEN** Tessera's node runs it, its plan shows the spill, and the
  rows are the core's
- **Verified by:**
  `test/sql/join.sql::A hashed key that is an expression its child computes`

#### Scenario: Keys left to the core
- **WHEN** a grouping or a join is by a key of `bit varying`, a join by
  `money`, or a join by `float4` equal to `float8`
- **THEN** the core's node runs it
- **Verified by:** pending

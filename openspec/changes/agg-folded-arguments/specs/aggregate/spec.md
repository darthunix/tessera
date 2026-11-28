## Purpose

The aggregates the batch grouping `TessAgg` folds into states shared
with other aggregates, the errors of their arguments, what `EXPLAIN`
shows of them, the arguments it does not fold, and the count of
aggregates one node takes. The node's other promises stay described in
`docs/nodes.md` until this capability holds them.

## ADDED Requirements

### Requirement: Aggregates folded from shared states
A folded form SHALL be an expression `e` of the input under these
steps: `+` and `-` with a constant on either side, `*` by a constant,
unary minus, and the casts of an integer to a wider integer or to
`numeric` without a precision. `e` is where the steps stop, any
expression of type `smallint`, `integer` or `bigint`, or a column of a
table of type `numeric` of a declared precision, with no volatile
function in it or in the aggregate's `FILTER`. A constant SHALL be a
value known when the query is planned, not `NULL`, and of `numeric`
finite: a literal, an immutable function of literals, or a parameter
of a custom plan; not a parameter of a generic plan nor a stable
function. A form whose type is `numeric` SHALL be folded only where no
row's argument and no sum of the rows can pass the range of `numeric`
or be rounded.

A plain aggregation or a grouping of `TessAgg` SHALL fold the
aggregates of one `e`, one `FILTER` and one `DISTINCT` where at least
two of them are of the kinds below, one of those has a form other than
`e` itself, and the states they would share are no more than they are:

- `sum`, `avg` and `count` of a folded form;
- `min` and `max` of a folded form of an integer `e`, and of a
  `numeric` `e` whose factor is positive.

The partial and the final aggregation of a parallel plan SHALL fold
nothing. A folded aggregate's value, type, scale and `NULL` SHALL be
those the core gives, with and without `GROUP BY` and with groups past
`hash_mem`, for a group whose aggregate reads fewer than 2^32 rows.

#### Scenario: Ninety sums of one column
- **WHEN** a query sums a `smallint` and the `smallint` plus each
  integer from 1 to 89, 90 sums
- **THEN** its plan is `TessAgg`'s, and its row is the core's
- **Verified by:**
  `test/sql/agg.sql::spec: aggregate/Ninety sums of one column`

#### Scenario: Every aggregate, type and step
- **WHEN** `sum`, `avg`, `count`, `min` and `max` of a `smallint`, an
  `integer`, a `bigint` and a `numeric(12,2)` are computed, each beside
  its column plus, minus and times a constant on either side, its
  negation, nested forms and widening casts, with `NULL` values and
  `NaN`, a group of `NaN` alone, no rows, a `FILTER` that leaves no row
  of a group, `GROUP BY` and groups past `hash_mem`
- **THEN** every row is the core's, with its types and scales
- **Verified by:**
  `test/sql/agg.sql::spec: aggregate/Every aggregate, type and step`

#### Scenario: Aggregates left as they are
- **WHEN** a query computes `sum(x + 1)` of an `integer` alone, or
  `sum(x)`, `avg(x)` and `count(x)` of it, or `sum(x + 1)` beside
  `max(x)`, which would share three states, or two forms of `x` under
  a volatile `FILTER`, `random() < 2`, which holds for every row, or
  `sum(nextval('s'))` beside `max(nextval('s'))`
- **THEN** its plan is `TessAgg`'s, it folds nothing, and its rows are
  the core's
- **Verified by:**
  `test/sql/agg.sql::spec: aggregate/Aggregates left as they are`

### Requirement: The errors of a folded argument
Where the core's computation of a folded aggregate's argument fails for
a row the aggregate reads, the query SHALL fail with an error the core
raises for one of those rows, of SQLSTATE `22003`, before `TessAgg`
returns its first row. A row a `WHERE` clause or the aggregate's
`FILTER` leaves out SHALL not fail it; `HAVING` and a `LIMIT` above the
node SHALL not keep it from failing. A rescan SHALL check the rows it
reads alone.

#### Scenario: Past the range of each integer
- **WHEN** an `integer` plus a constant passes the `integer` range in
  one row, a `smallint` plus a `smallint` the `smallint` range at its
  least value, a `bigint` times a negative constant the `bigint` range,
  and a nested form passes the range of an inner step
- **THEN** each query fails with SQLSTATE `22003` and the core's message
  of one of the steps that fail
- **Verified by:**
  `test/sql/agg.sql::spec: aggregate/Past the range of each integer`

#### Scenario: Failing rows left out and kept
- **WHEN** the only rows whose argument fails are left out by `WHERE`,
  or by the aggregate's `FILTER`, or are in a group that `HAVING`
  removes, or in a group past a `LIMIT` above the node, or in groups
  past `hash_mem`
- **THEN** the first two queries return the core's rows, and the others
  fail as the core's hashed aggregation does
- **Verified by:**
  `test/sql/agg.sql::spec: aggregate/Failing rows left out and kept`

#### Scenario: A rescan with other rows
- **WHEN** the folded grouping is the inner side of a nested loop whose
  outer value picks its rows, and only the rows of the first outer
  value fail
- **THEN** the query fails as the core does, and without those rows it
  returns the core's rows
- **Verified by:**
  `test/sql/agg.sql::spec: aggregate/A rescan with other rows`

### Requirement: Folded Aggregates in EXPLAIN
`EXPLAIN VERBOSE` of a `TessAgg` that folds SHALL show `Folded
Aggregates`, the count of the query's aggregates it computes from
states it keeps for several of them rather than from their own
arguments, aggregates of a constant among them, and SHALL show nothing
of the kind where it folds none.

#### Scenario: The count shown
- **WHEN** a query computes `sum(x)`, `sum(x + 1)`, `avg(x * 2)` and
  `count(x - 3)` of an `integer`, and `sum(y)` of another column
- **THEN** `EXPLAIN VERBOSE` shows `Folded Aggregates: 4`
- **Verified by:**
  `test/sql/agg.sql::spec: aggregate/The count shown`

### Requirement: Arguments the node does not fold
`TessAgg` SHALL compute from its own argument, without folding, an
aggregate whose `e` is of `real`, `double precision`, `numeric` without
a declared precision, `date`, `time`, `timestamp`, `timestamptz`,
`interval` or `money`, and an aggregate whose form has a step other
than those of the requirement "Aggregates folded from shared states",
a division say, or a constant that is not one. An expression of several
columns, `x + y` say, SHALL be an `e` of its own. Their results SHALL
be the core's.

#### Scenario: Forms the node does not fold
- **WHEN** a query computes, each beside the same aggregate of its
  column alone, `sum(f + 0.5)` of a `double precision`, `max(d + 1)` of
  a `date`, `max(t + interval '1 month')` of a `timestamp`, `sum(x /
  2 + 1)` beside `sum(x / 3)` of an `integer`, `sum(n + 1)` of a
  `numeric` without a precision, a `numeric(12,2)` column of a function
  in `FROM`, `sum(n * 'Infinity')`, and `sum(x + $1)` under a generic
  plan
- **THEN** its rows are the core's, and `EXPLAIN VERBOSE` shows no
  `Folded Aggregates`
- **Verified by:**
  `test/sql/agg.sql::spec: aggregate/Forms the node does not fold`

### Requirement: Aggregates of a constant
A plain aggregation or a grouping of `TessAgg` SHALL compute `sum`,
`avg`, `min`, `max` and `count` of a constant of an integer type or of
`numeric`, without `DISTINCT`, from the count of the rows the aggregate
reads, with the core's value, type, scale and `NULL`: `NULL` where it
reads no row, but 0 for `count`.

#### Scenario: A sum of 1 and an average of a numeric
- **WHEN** a query computes `sum(1)`, `avg(2.50)`, `max(7)` and
  `count(3)`, with `GROUP BY`, over no rows, and under a `FILTER` that
  leaves no row of a group
- **THEN** its rows are the core's
- **Verified by:**
  `test/sql/agg.sql::spec: aggregate/A sum of 1 and an average of a numeric`

### Requirement: DISTINCT folded
`TessAgg` SHALL fold `sum(DISTINCT ...)`, `avg(DISTINCT ...)` and
`count(DISTINCT ...)` of folded forms of an integer, and
`count(DISTINCT ...)` of folded forms of a `numeric` column of a
declared precision, whose factor is not 0, from the distinct values of
`e`.
Results and errors SHALL be the core's.

#### Scenario: Distinct values shifted and scaled
- **WHEN** a query computes `count(DISTINCT x + 1)`, `sum(DISTINCT x *
  2)` and `avg(DISTINCT 10 - x)` of a `smallint` with repeated values
  and `NULL`, `count(DISTINCT x * 0)` beside them, and `count(DISTINCT
  ...)` of a `numeric(12,2)` `n`, `n * 1.5`, `n + 1` and `-n` with
  `NaN`, with `GROUP BY` and groups past `hash_mem`
- **THEN** its rows are the core's
- **Verified by:**
  `test/sql/agg.sql::spec: aggregate/Distinct values shifted and scaled`

### Requirement: The aggregates a node takes
`TessAgg` SHALL take a grouping of any count of aggregates, with or
without `GROUP BY`, where its other conditions hold, its grouping keys,
the arguments of the states it keeps and their `FILTER` conditions are
at most 1 664 columns, and a group's keys and states take at most 64 kB;
past either bound, the grouping SHALL be the core's.

#### Scenario: More than 64 aggregates
- **WHEN** a query computes 65 and 200 aggregates that no form folds,
  without and with `GROUP BY`, in a parallel plan and with groups past
  `hash_mem`
- **THEN** its plans are `TessAgg`'s, and its rows are the core's
- **Verified by:**
  `test/sql/agg.sql::spec: aggregate/More than 64 aggregates`;
  `test/sql/parallel.sql::spec: aggregate/More than 64 aggregates`

#### Scenario: Past the bounds
- **WHEN** a grouping's aggregates of 900 arguments under 900 `FILTER`
  conditions pass 1 664 columns, and another's 1 650 averages of
  `bigint` expressions take more than 64 kB a group
- **THEN** both plans are the core's, and their rows are the core's
- **Verified by:**
  `test/sql/agg.sql::spec: aggregate/Past the bounds`

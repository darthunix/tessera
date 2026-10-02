# tessera-crosscheck

Random queries with Tessera on and off, compared (plan 9.8).

The SQL suites compare about a thousand queries in both modes, each
written by hand for what someone thought of. This tool draws queries from a
seed over three tables of edge-leaning data, runs each with
`tessera.enable` on and off in one connection, and compares the results.
A query whose modes disagree is shrunk to a small one that still does and
written out as a ready case for a suite.

```sh
make crosscheck                                  # 200 queries of a new seed
make crosscheck SEED=101 QUERIES=1000            # a given seed
cargo run --release -p tessera-crosscheck -- --help
cargo run --release -p tessera-crosscheck -- stop
```

The tool builds and installs the release build of Tessera into the
PostgreSQL build of `PG_CONFIG`, as `cargo tpch` does (`--no-install` keeps
what is installed), and runs its own cluster on port 5435 under
`target/bench-runs/pgdata-crosscheck`.

## What it generates

The data comes from PostgreSQL's `random()` after `setseed`, so a seed gives
the same rows on the same build. There are three tables: `fact` (2000 rows
by default), `dim` (200) and `tiny` (5).

- **Types.** int2, int4, int8, numeric at a fixed and at varying scales,
  float8, text, date, timestamp and bool.
- **NULLs.** Every column but the key has them.
- **Edge values.** The ends of the integer types, zero and ±1, the empty
  string, numerics of 19 and more digits, NaN and the infinities, and dates
  far from today.

A query is a typed tree, drawn by proptest:

- **Expressions.** Arithmetic, casts, CASE, COALESCE, comparisons, BETWEEN,
  IN lists, LIKE and NULL tests, nested two levels deep.
- **FROM.** One to three tables, joined inner, left, right or full on
  integer or text keys, with an extra join condition at times.
- **Conditions.** An expression, a semi or anti join (`[NOT] EXISTS`), or
  both.
- **Outputs.** Expressions, DISTINCT, or a grouping (none to two keys) with
  count, sum, avg, min, max, bool_and and bool_or, DISTINCT inside some of
  them, and a HAVING on a count.
- **Ordering and limits.** ORDER BY every output column with a LIMIT.
- **Set operations.** UNION, INTERSECT or EXCEPT, with or without ALL, of
  two queries of one row type.
- **Settings.** Serial or with two workers (parallel plans made free), and
  work_mem of 4MB, 256kB or 64kB, so that hash tables and sorts spill.

## How it decides

Both modes run as `SELECT array_agg(q::text ORDER BY q::text) FROM (query)
q`, as the suites' `agg_same` does, so their rows compare as multisets.

- **Agree.** The same rows. Or both modes fail, whatever their SQLSTATEs,
  unless one is an internal error (class XX). A batch evaluates a condition
  for its 64 rows before it computes their outputs, so of two rows that
  would each raise an error, the batch can meet the other first.
- **Disagree.** Different rows; an error in one mode only; an internal
  error; a timeout in one mode only (5 s); a lost connection, which means
  the backend crashed.

Outputs that stand for equal values take no stand on which value shows:
a group's key, a distinct or set-operation row, min and max. For these,
numerics go through `trim_scale` and float8 gets `+ 0`, because 0 and 0.0,
or 0 and -0, are equal and either may show, in the core too.

A disagreement is shrunk by proptest within a minute and written to
`target/bench-runs/crosscheck/finding-<seed>.sql`. The file holds the
data's SQL, the settings and the query in both modes. Its seed goes into
`seeds.txt`, whose seeds run first in every run, so that a fixed finding
stays fixed. A seed draws the same queries only from the same generator, so
a change to the generator rechecks those seeds by hand. The lasting record
of a fixed finding is its shrunk query, added as a case to a suite.

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
`target/bench-runs/pgdata-crosscheck`: `UTF8`, with the C collation where
a query names none. A cluster an older version made without `UTF8` fails
the setup; remove its directory. The server must be built with ICU.

## What it generates

The data comes from PostgreSQL's `random()` after `setseed`, so a seed gives
the same rows on the same build. There are four tables: `fact` (2000 rows
by default), `dim` (200), `tiny` (5), and `words` (3000), a table of
strings: `t` of a few words, which a Parquet writer keeps through a
dictionary, and `u` of longer strings, most of them apart.

- **Types.** int2, int4, int8, numeric at a fixed and at varying scales,
  float8, text, date, timestamp and bool.
- **NULLs.** Every column but the key has them.
- **Edge values.** The ends of the integer types, zero and ±1, the empty
  string, numerics of 19 and more digits, NaN and the infinities, and dates
  far from today.
- **Strings.** Characters of two, three and four bytes in UTF-8, a
  combining accent, and pairs that `nd` takes as equal though their bytes
  differ: `é` and `É`, and `é` whole and as `e` with a combining accent.
  The setup checks that it does.
- **Collations.** The setup makes `lc`, of libc for a language
  (`en_US.UTF-8`, or `C.UTF-8` where that is missing), and `nd`, ICU's
  nondeterministic one that ignores case; `C` and the builtin
  `pg_c_utf8` are there already.

A query is a typed tree, drawn by proptest:

- **Expressions.** Arithmetic, casts, CASE, COALESCE, comparisons, BETWEEN,
  IN lists, LIKE and NULL tests, nested two levels deep. A text side of a
  comparison, an IN list or a LIKE, the other side of a text key of a
  join or of an EXISTS, a text grouping key and the argument of a text
  aggregate are now and then put under one of the four collations; a
  text output, the argument of lower and upper, and the argument of min
  and max of text, under one of the deterministic three. A text column
  of a set operation is under one deterministic collation in both
  branches, C where none is drawn.
- **FROM.** One to three tables, joined inner, left, right or full on
  integer or text keys, with an extra join condition at times.
- **Conditions.** An expression, a semi or anti join (`[NOT] EXISTS`), or
  both.
- **Outputs.** Expressions, DISTINCT, or a grouping (none to two keys) with
  count, sum, avg, min, max, bool_and and bool_or, DISTINCT inside some of
  them, and a HAVING on a count.
- **Ordering and limits.** ORDER BY every output column, with a LIMIT
  or without one.
- **Set operations.** UNION, INTERSECT or EXCEPT, with or without ALL, of
  two queries of one row type.
- **Settings.** Serial or with two workers (parallel plans made free), and
  work_mem of 4MB, 256kB or 64kB, so that hash tables and sorts spill.

## How it decides

Both modes run as `SELECT array_agg(q::text ORDER BY q::text) FROM (query)
q`, as the suites' `agg_same` does, so their rows compare as multisets. A
query ordered by every output column runs as `SELECT array_agg(q::text)
FROM (query) q`, so that its rows compare in their order too: a subquery
with ORDER BY is not pulled up, and the aggregate over it is not
parallel.

- **Over the Parquet twin.** `words` has a twin, `words_pq`, a foreign
  table of `tessera_parquet`: its rows, read back from the heap table, are
  written by the reader's crate into
  `target/bench-runs/crosscheck-twin`, in a file of chunks through a
  dictionary, one of plain chunks, and one of chunks whose dictionary
  fills up and falls back to plain pages, which the setup checks in the
  footer; a group of the first and of the last file is longer than a
  window of the reader, 1024 rows. A query that reads `words`, and
  whose modes agree, runs once more over the twin with Tessera on, and
  is decided against the heap table with Tessera off, as two modes are:
  a fault of the reader, which both modes over the twin would share,
  shows there. A finding over the twin shrinks to one over the twin, and
  its file keeps the seed's files of the twin beside it, with the SQL
  that reads them.
- **Agree.** The same rows. Or both modes fail, whatever their SQLSTATEs,
  unless one is an internal error (class XX). A batch evaluates a condition
  for its 64 rows before it computes their outputs, so of two rows that
  would each raise an error, the batch can meet the other first.
- **Agree: an error of a row one plan skips.** A data exception (class 22,
  such as an overflow or a division by zero) in one mode while the other
  returns rows. Plans differ in which rows they evaluate. A batch of 64
  rows or a parallel worker runs past a LIMIT, the core's sorted grouping
  stops at the limit, and a join may hash on a condition the other plan
  checks only after a match, or skip a side whose other side is empty. To
  tell this from a wrong overflow, the mode that returned rows runs the
  relaxed query: no LIMIT, no DISTINCT, every aggregate filtered (the core
  plans min and max of a constant as a subquery with LIMIT 1), joins
  hashed so that they read every row, and with Tessera on the core's join
  over Tessera's scans. Then it runs a probe of each table: every row that
  passes the table's own WHERE conditions, through each largest part of
  the query's expressions that reads that table alone. If either raises a
  data exception too, the error is the data's.
- **Disagree.** Different rows. An error in one mode only that neither
  the relaxed query nor a probe confirms. An internal error. A timeout in
  one mode only (5 s), the limit of the query alone; a query whose limit
  fires while its rows go out has timed out. A lost connection, which
  means the backend crashed.

Outputs that stand for equal values take no stand on which value shows:
a group's key, a distinct or set-operation row, min and max, and every
output of a query with a LIMIT, where rows tied in the order may stay
either way, or of an order without one. For these, numerics go through
`trim_scale` and float8 gets `+ 0`, because 0 and 0.0, or 0 and -0, are
equal and either may show, in the core too. Under `nd` equal strings
differ in their bytes, so no output stands for them: a grouping key under
it is not an output, and the groups compare by their aggregates, and
min and max of text take no collation but a deterministic one.

A disagreement is shrunk by proptest within a minute, keeping its class: a
crash stays a crash, different rows stay different rows. It is written to
`target/bench-runs/crosscheck/finding-<seed>.sql`. The file holds the
query first found and what its modes did, the data's SQL, the settings
and the shrunk query in both modes. The shrunk query runs once more on its
own, and the file's first line says when it then agrees: a parallel plan
can raise an error, or not, by which participant reaches a row first. Its
seed goes into
`seeds.txt`, whose seeds run first in every run, so that a fixed finding
stays fixed. A seed draws the same queries only from the same generator, so
a change to the generator rechecks those seeds by hand. The lasting record
of a fixed finding is its shrunk query, added as a case to a suite.

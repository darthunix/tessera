# parquet-fdw: the architecture contract

The Parquet wrapper is a module apart from Tessera: a foreign data
wrapper, a source of the source registry and a batch node, over a
reader of the files in Rust. These rules keep it a module of another
extension, keep its rows the same whichever path reads them, and keep
the reader's memory and errors inside PostgreSQL's rules. Each rule
says what must not happen and why; how the code is laid out is left to
the work.

## Rules

### Only the public C API
The module builds against Tessera's installed headers and runtime
library and its own reader alone, and reads no private structure of
the bridge, the runtime or the nodes, so that it can be built, shipped
and loaded as a module of another extension would be.
- **Checked by:** pending

### The same rows by every path
A query that ends without an error returns the same rows by the
wrapper's rows, the source's batches and a parallel plan of either,
since one reader and one making of values serve them all, so that the
plan the planner chooses never changes a result.
- **Checked by:** pending

### No text unchecked by its column's rule
No value of a `text` or `varchar` column reaches a query without the
check its column's read plan asks, whatever path made it, so that a
string that is not UTF-8 never becomes a value of a column that
promised to check it.
- **Checked by:** pending

### No Rust frame under an error
Every call into the reader returns before PostgreSQL raises an error
about it, so that an error's unwinding never crosses a Rust frame.
- **Checked by:** agent review — list every call from C into the
  reader and every callback the reader makes into C; a call during
  which PostgreSQL may raise an error, or a callback that may, fails
  the rule

### The reader's memory is the query's
Every object of the reader that the module holds is freed when the
memory context that owns it is reset or deleted, after an error too,
so that a failed or cancelled query leaves no file open and no memory
behind; the one exception is the backend's cache of footers, bounded
by its count of files and owned by the module for the backend's life.
- **Checked by:** agent review — list every object of the reader the C
  side holds, with the memory context whose reset frees it; one freed
  only on a path an error skips fails the rule

## Forecast

- **Grouping by a string on the file's bytes.** A form of column other
  than `Datum` in the batch, which the node offers and a grouping reads:
  `tessera/batch.h`, the node's batches, TessAgg; not the reader, nor
  the wrapper.
- **Another format of files, such as Arrow's own.** A reader of its own
  and its module, which take the node's pattern; not this module.
- **Files in an object store.** The reader's input and the option of
  files; not the node, the clauses or the parallel handout.
- **Page indexes and Bloom filters of a file.** The reader and the
  skipping of groups and pages; not the wrapper's options.
- **More types, such as decimals into `numeric`.** `datum-arrow`'s
  plans, the mapping of columns and the making of values; not the
  clauses already checked.

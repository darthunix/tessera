# source-scan: the architecture contract

A source lets a module that keeps a relation in a format of its own hand
Tessera batches of the relation's rows, while Tessera plans its filter
and its batch parents over them. These rules keep a source a module
apart from Tessera, and keep Tessera's plans of everything else as they
are when a source is added. Each rule says what must not happen and why;
how the code is laid out is left to the work.

## Rules

### Only the public C API
A source builds against Tessera's installed headers and runtime library
alone and reads no private structure of the bridge, the runtime or the
nodes, so that a source can be built, shipped and loaded apart from
Tessera's tree.
- **Checked by:**
  `test/installed/Makefile::installcheck-source`

### One check for each clause
Each clause of a relation is checked once: by the source when it answers
that it checks the clause exactly, and otherwise by the filter node, in
the core's order, so that no row is judged by two readings of one clause
and every clause has one place that answers for its result.
- **Checked by:** agent review — for each source in the repository, list
  the clauses its node evaluates at execution, its plan's qualifiers and
  its plan's expressions, beside the answers its planning gives; a
  clause evaluated without an exact answer, beyond what the source
  evaluates to prove that a clause it uses to skip rows rejects them,
  such as a comparison with its least and greatest values or with the
  values of a dictionary, a qualifier of the plan, a clause answered for
  that is not among the plan's expressions, or an exact answer for a
  clause the node does not evaluate, fails the rule

### No clause ahead of its guards
A source uses only the clauses Tessera marks usable, so that no clause
runs, on rows or on data derived from them, such as their least and
greatest values, ahead of the clauses of a row security policy that
guard it.
- **Checked by:**
  `test/sql/source_scan.sql::Clauses under a row security policy`

### A source's own check reveals nothing
A source that checks a clause by code of its own, rather than by
PostgreSQL's functions, raises no error, notice or other message that
depends on the values of the rows it checks, so that a row a policy or a
view's clause hides is never shown by the source that skipped it.
- **Checked by:** agent review — for each source in the repository, list
  every path of its own checking of clauses that can raise an error or
  emit a message; one whose text or occurrence depends on a row's value
  fails the rule

### Sources leave heap plans alone
Registering a source changes no plan and no result of a query that reads
no relation of the source, so that a source can be added without a risk
to the queries that do not use it.
- **Checked by:**
  `test/sql/source_scan.sql::-- spec: source-scan/Heap tables beside a source`

### A source touches no part of Tessera
Adding a source for a kind of relation that Tessera already offers to
the sources changes no file of the bridge, the runtime or the nodes, so
that a new form of storage of such a relation is a module of its own and
not a change across Tessera.
- **Checked by:** probe — add a second test source, for a foreign data
  wrapper of its own, on a branch never merged; every file it changes
  outside its own module and its tests fails the rule

## Forecast

### The Parquet source
A module of its own, a foreign data wrapper with its batch node and a
reader of the files: its build, its tests and CI. No file of the bridge,
the runtime or the nodes.

### Join clauses checked by a source
A source that checks the clauses of a join for each outer row needs a
parameterized batch path: the runtime's rule that a batch path is never
parameterized, the scan planner, and the clauses offered with their
parameters. The sources that do not take them are untouched.

### An aggregate answered by a source
A source that answers a count or a least value from data of its own,
without reading rows, as a Parquet file's footer allows: the planner of
the grouping and a new callback of `TessSource`. Not the scan planner.

### Columns in forms other than Datum
Columns a source gives as arrays of native values, or strings without a
header of their own: the batch contract, the reading of columns by the
expression compiler and the kernels' inputs. Not the source registry,
and not the scan planner.

### Tables of another access method
A table whose access method is not heap, read through a source: the scan
planner's walk, which would offer the source a sequential scan of such a
table, and its choice of relations. Not the heap scan node, and not the
runtime's calls.

### Keys of a join handed to a source
A source's node that takes the key filter of a hash join above it. A
source alone under the join: the node's optional callback that every
node has, in the source's module alone. A source under `TessFilter`:
also the filter's handing down of the keys, which it takes itself today,
and the capability `key-filter`.

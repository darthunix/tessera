## Context

`docs/table.md` has thirteen sections. Nine describe the table itself:
why chunks and references, the index and the chunks, the layout in
pictures, creating and attaching, a batch in and out, the calls of one
writer, how the table grows, the atomics in order, ownership and errors.
Four describe what other parts build on it: partitions for spilling,
the Bloom filter, sorting records, and the phases of a shared build
with its rounds. The calls of one writer also include the aggregate
states of a grouping.

The code is the module `table` of `tessera-kernels`, its C entry points
in `tessera-capi`, and the C API in `include/tessera/table.h` and
`table_key.h`. The nodes reach the entry points through the kernels'
table of operations (`tessera/kernel_ops.h`).

## Goals / Non-Goals

**Goals:**
- One place that says what the bytes of a table are, in local and in
  shared memory, and what each call over them promises.
- Each statement tied to a test by name.
- The code made to keep the promises that it breaks today, each
  correction with the test that shows it.

**Non-Goals:**
- What other parts build on the table: partitions for spilling, the
  Bloom filter, the marks of RIGHT and FULL joins, the aggregate states
  and the calls that fold rows into them, the items of a sort, the
  phases of a shared build and of the rounds, the shared words of a
  spill. They are the second half of the capability, or belong to the
  capabilities of the join, the grouping and the sort.
- The choice of the nodes: when they make an index, how large their
  chunks are, when they spill.
- Speed. No hot loop changes; the corrections add work only where a
  call starts or where a walk is about to be called a loop.

## Decisions

### Two pull requests for the table

**Was:** the table's whole C API, about sixty functions, in one guide.

**Will be:** this change describes the table itself in ten
requirements. A second change of the same capability takes the shared
build, partitions and the Bloom filter. The aggregate states go to the
capability of the grouping, and the items of a sort to the sort's.

### The status of a damaged table

**Was:** not written anywhere. The code reports a damaged table as
`TESS_ERROR_INVALID_ARGUMENT`, SQLSTATE `XX000`, the same as a wrong
argument.

**Will be:** the spec states it. A table lives in memory, so damage to
it is a bug of Tessera or of its caller, not bad data from a user or a
disk: an internal error. `XX001` stays for spilled bytes read back from
a file, as `spill-format` says.

### The bound of a walk

**Was:** a walk down a chain gives up when its steps reach the count of
records in the header. The count is read from the same memory that may
be damaged, and nothing checks it.

**Will be:** the walk is also bounded by the records the chunks given to
the call have room for, their bytes divided by the record size. A walk
longer than that must visit some record twice. The bound is computed
once a call; the loop compares against the smaller of the two numbers,
as it compared against the count before.

### Outputs checked first

**Was:** `tess_table_clear_key`, `tess_table_scan`, `tess_table_split`,
`tess_table_combine` and `tess_table_next_unmarked` did their work and
then refused a missing pointer for a result.

**Will be:** every pointer is checked before the work starts, as
`kernels.h` promises for all entry points.

### What we do not do

- The checks do not move to the used mark. A reference is checked
  against the length of its chunk; in a shared table another process
  may be moving the mark of its own chunk.
- The header does not get a checksum. The table lives in memory for one
  query, and the checks of its fields and references find what a bug
  leaves.
- Every call still checks every chunk it is given, so the fixed part of
  a call grows with the number of chunks. That is a question of speed,
  left for a change with its own measurement.

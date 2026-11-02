## Context

`deform` in `runtime/heap_batch.c` deforms one value of one row:
`tess_deform_advance` moves the row's `TessDeformCursor` from the
attributes it has passed to the requested one. A request for an
attribute the cursor has passed used a local cursor from the tuple's
start and dropped it. The batch remembers the values it deformed (a
mask a column), so only columns not read yet for a row cost a walk.

## Goals / Non-Goals

**Goals:**

- One walk of a row for columns read in ascending order, wherever the
  row's cursor stands.
- No cost on the common path, a column after the row's cursor, which
  every scan with a filter takes for every row.

**Non-Goals:**

- No change of `tess_deform_advance` or of the public C API. A walk
  that stores the values it passes would save the last restart too,
  but changes the public header.
- No reordering of a filter's clauses: their order is the planner's.

## Decisions

### A second cursor a row, set lazily

`back_cursors` holds a cursor a row and `back_set` a bit a row, cleared
when the batch is reset. A request behind the row's cursor takes the
second cursor when its bit is set and it lies before the column, else
starts it from the tuple's start and counts a restart. Both fields lie
at the end of the struct, so the offsets of the others stay, and the
work lives in an out-of-line `back_cursor`, called on an `unlikely`
branch.

Tried and dropped, by the measurements of the design's last section:

- Starting the second cursor at every append: one store a row more on
  every scan; Q3, Q6, Q10 and Q12 lost 2 % to 5 %.
- The new fields in the middle of the struct: the offsets of the hot
  `heap_get_datum_column` and of the page append moved, and their loops
  with them.

### The earlier of two compared columns first

`tess_expr_apply_filter`, before a comparison of two bare columns,
reads the operand's column first when it lies before; the batch keeps
it, so the read `build_args` makes later costs no walk.

## Risks / Trade-offs

- Same values: each cursor walks its own row's tuple; the tests compare
  every value of the four columns of a table with text, NULL and bigint.
- A request behind both cursors still starts from the tuple's start, as
  before.

## Measurement

TPC-H Q1, Q6, Q12, Q14 and Q21 serial with `cargo tpch`, base and
change, and `cargo ab measure --family win`, base and change
alternating, which reads one column a filter. A case slower beyond the
noise of the machine stops the work for the maintainer.

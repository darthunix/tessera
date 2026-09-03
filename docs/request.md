# Layouts and requests

`TessLayout` describes the logical columns that a batch producer can return.
These column numbers are local to one producer-to-consumer connection and do
not have to match table attribute numbers or plan target-list positions.

`target_columns` maps zero-based plan targets to zero-based batch columns. A
null map is an identity map. With an explicit map, `-1` means that a target is
not available from the batch. For example, plan targets `[a, a + b, c]` can
map to batch columns `[a, c]` as `{0, -1, 1}`.

`TessRequest` describes what the consumer needs from that producer. Layouts
and requests remain separate: the layout is a property of the producer, while
the request can differ for each consumer.

## Column phases

Both column masks use the compact numbers from `TessLayout`. A null
`Bitmapset` is an empty mask. Set bits must be less than `ncolumns`.

`filter_columns` are needed before the final active row set is known.
`projection_columns` are needed only for rows that survive filtering. A
column may occur in both masks. The producer can prepare filter columns first
and postpone projection-only work until fewer rows remain.

## Output

`TESS_OUTPUT_ROWS` asks the producer to expose ordinary PostgreSQL rows.
`TESS_OUTPUT_BATCH` asks it to keep the result as a batch attached to the
returned `TupleTableSlot`.

`max_batch_rows` limits the physical size of one returned batch. Zero means
that the consumer imposes no limit. This field is not a pushed-down SQL
`LIMIT` and does not limit the total number of rows returned.

## Ownership

Neither structure owns its pointer fields. Code that keeps a layout or
request after the call that supplied it must copy the target map and column
masks. `TessApi.attach()` copies the layout, and `TessApi.set_request()` copies
the request into the slot's memory context.

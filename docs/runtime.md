# Runtime library

`libtessera_runtime.a` (`runtime/`) holds the helpers a batch node needs
beyond the bridge's contract: building batches from rows, and later the
node's output and input sides, the plan-data codec and the unary node
helper. It is a static library, installed next to the bridge in
`pkglibdir` with its header `tessera/runtime.h`; a node module links it
rather than calling through the bridge, so the bridge stays a small contract
and the helpers can change with the nodes that use them:

```make
MODULE_big = my_node
SHLIB_LINK += $(shell $(PG_CONFIG) --pkglibdir)/libtessera_runtime.a
```

## Building a batch from rows

`TessBuilder` collects rows from tuple slots into an owned column-major
Datum batch. A node whose child returns rows, such as the pack node,
creates one builder for the scan:

```c
TessBuilderConfig config = TESS_STRUCT_INITIALIZER(TessBuilderConfig);

config.parent_context = estate->es_query_cxt;
config.tuple_desc = ExecGetResultType(child);
config.ncolumns = ncolumns;
config.capacity = 64;
builder = tess_builder_create(&config);
```

and fills a batch per `ExecProcNode` cycle:

```c
tess_builder_reset(builder);
while (!tess_builder_is_full(builder))
{
    TupleTableSlot *slot = ExecProcNode(child);

    if (TupIsNull(slot))
        break;
    tess_builder_append_slot(builder, slot);
}
batch = tess_builder_finish(builder, InvalidOid);   /* NULL without rows */
```

`append_slot` materializes the leading `ncolumns` attributes of the slot
with `slot_getsomeattrs`, which works for every kind of slot: a virtual slot
is already deformed, a heap or minimal-tuple slot deforms just that prefix.
Pass-by-reference values are copied into the builder's own memory context,
because a slot's values point into the tuple's memory, which the child's
next call may free; so the slot may be cleared or reused right after the
append. A batch of pass-by-value columns is copied without that context.

The finished batch selects all its rows, holds every row initialized (a NULL
row stores 0), exposes only `get_datum_column`, whose result covers the
whole column for any requested mask, and has no release callback: the
builder owns the storage and `reset` reuses it. Kernels therefore read the
builder's columns with `prepared = NULL`. Before `reset`, the node takes the
previous batch off its slot binding; the builder does not check that, the
bridge does when the next batch is published. Finishing again returns the
same batch; `is_full` is true after `capacity` rows or after finishing.

Errors are raised with `ERROR`: a configuration without a context or
descriptor, a column count outside `1..natts`, a capacity below one, an
append after finishing or into a full builder, a slot with fewer
attributes than columns, and a column request out of range or with an
undersized result structure.

# Porting map

The reference implementation is the adjacent `pg_batch` checkout at commit
`0de9c9506c7358ea32a3ec421c569efcfd560ac9`. Source links below refer to that
checkout, which remains unchanged during the port.

## Step 2.9: node registry

The reference defines `PgBatchNodeOps` in
[include/pg_batch/node.h](../../pg_batch/include/pg_batch/node.h). Registration,
removal, and lookup are implemented in
[bridge/bridge.c](../../pg_batch/bridge/bridge.c) by `register_node`,
`unregister_node`, `find_node`, and `get_node`.

Tessera exposes the node identity and a separate registry operation table in
[include/tessera/node.h](../include/tessera/node.h), with the implementation
in [bridge/node.c](../bridge/node.c) and usage in [node.md](node.md).

This step ports registration and lookup by stable name. Removal identifies
the exact object by pointer, matching Tessera's source registry. Names are
unique within the node registry; descriptions and name strings remain owned
by their providers.

The reference also uses `supports_path`, `get_layout`, and
`get_request_binding` to connect nodes during planning and execution.
Their Tessera interfaces will be added with the first real node consumer,
`TessPack`; the current registry contains no planning or execution callbacks.
Source planning from step 2.8 is a separate interface, scheduled with step
6.5. Transfer between two independent modules is tested in step 2.10 below.

## Step 2.10: independent producer and consumer

The reference describes batch ownership and Datum column access in
[include/pg_batch/batch.h](../../pg_batch/include/pg_batch/batch.h), implements
slot transfer in [bridge/bridge.c](../../pg_batch/bridge/bridge.c), and tests
requests and output in
[test/pg_batch_runtime_test.c](../../pg_batch/test/pg_batch_runtime_test.c).

Tessera adds a cross-module test, rather than copying a reference runtime
node. The [producer](../test/tessera_producer_test.c) and
[consumer](../test/tessera_consumer_test.c) are separate PGXS libraries. They
include only public Tessera headers and do not link to each other or to bridge
objects. Each discovers and validates the bridge API independently.

The [SQL test](../test/sql/modules.sql) passes the consumer function's identity
to the producer. PostgreSQL's `fmgr` carries a `TupleTableSlot *` as an
`internal` argument, with a boolean selecting preparation or consumption.
This test-only invocation mechanism does not add a Tessera API or expose
pointers to SQL. The consumer keeps its own state in `FmgrInfo.fn_extra` and
finds the slot binding once during preparation.

The producer attaches the slot; the consumer requests an `int4` filter column
and a `text` projection column. After the producer freezes the request and
publishes a batch, the consumer reads the filter, removes null and odd values,
then reads text only for surviving rows. Both modules know the test values:
physical row `r` in batch `s` holds `1000 * s + r + 1` and its `row-N` label;
integer and text nulls occur when `r % 5 == 0` and `r % 7 == 0`, respectively.
This agreement does not expose the producer's physical storage.

The consumer marks the batch consumed without freeing it or using it again.
The producer releases it, destroying its dedicated memory context, then
publishes another batch with different values through the same binding.
Counters outside batch storage verify requested rows and exactly one release.
The test also covers empty and sparse masks, the 64-row boundary, and a
consumer error after column access. Normal and error cleanup both detach
before destroying the slot; cleanup is checked before the error is rethrown.
All state is local to one test call in one backend, with no concurrent access
or state shared between SQL statements.

Source planning (2.8) remains separate. The bridge guide is covered below.

## Step 2.11: bridge guide

The [bridge guide](bridge.md) walks through the compiled modules from step
2.10, with commands for building, installing, and running the example in a
temporary PostgreSQL instance. It covers per-backend loading, API discovery
and ABI validation, requests, column access, exclusive batch use, pointer
lifetimes, and cleanup after an error.

The reference contracts are in
[include/pg_batch/bridge.h](../../pg_batch/include/pg_batch/bridge.h) and
[include/pg_batch/batch.h](../../pg_batch/include/pg_batch/batch.h), with the
implementation in [bridge/bridge.c](../../pg_batch/bridge/bridge.c).
The guide documents Tessera's current behavior rather than copying the
reference interfaces, including its separate consumption and release steps.
This documentation step adds no API or runtime behavior; source planning
remains scheduled with step 6.5.

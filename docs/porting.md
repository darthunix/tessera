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
6.5. Testing transfer between two independent modules remains step 2.10.

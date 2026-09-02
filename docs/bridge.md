# Tessera bridge

The Tessera bridge is a small backend-local service that lets independently
built PostgreSQL extensions find one shared API table. It uses PostgreSQL's
rendezvous variables, so components do not link directly to one another.

The extension is named `tessera`. Creating it loads the shared library, whose
`_PG_init()` publishes `TessApi` under `tessera.api.v0`.

The bridge will eventually own only cross-extension coordination:

- attaching a batch request to a tuple slot;
- transferring one active batch between a producer and a consumer;
- registering batch sources and executor nodes;
- validating the version and size of public operation tables.

It will not evaluate expressions, interpret a source-native column format,
choose scan or join algorithms, or own the physical buffers behind a batch.
Those responsibilities belong to runtime libraries, nodes, and sources.

## Version zero

`TessApi` currently contains only an ABI version and its actual structure
size. The ABI version changes when an existing contract becomes incompatible.
The size lets a consumer check whether a later field is present before reading
it, so compatible operations can be appended without breaking an older
provider. Operations will be added as their implementations and independent
consumers are introduced.

Version zero is an experimental development ABI, not a compatibility promise.
The first public release will assign a stable nonzero ABI version after the
interfaces have been reviewed together.

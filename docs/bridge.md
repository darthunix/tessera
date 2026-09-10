# Tessera bridge

The Tessera bridge is a small backend-local service that lets independently
built PostgreSQL extensions find one shared API table. It uses PostgreSQL's
rendezvous variables, so components do not link directly to one another.

The extension is named `tessera`. Creating it loads the shared library, whose
`_PG_init()` publishes `TessApi` under `tessera.api.v0`.

The bridge owns only cross-extension coordination:

- attaching a batch request to a tuple slot;
- transferring one active batch through that slot;
- registering batch sources;
- registering batch-producing node kinds.

It will not evaluate expressions, interpret a source-native column format,
choose scan or join algorithms, or own the physical buffers behind a batch.
Those responsibilities belong to runtime libraries, nodes, and sources.

## Subsystem tables

`TessApi` remains a small root containing pointers to independently versioned
subsystem tables. `TessBindingOps` implements the slot binding protocol
described in [binding.md](binding.md). `TessSourceRegistryOps` implements the
source registry described in [source.md](source.md). `TessNodeRegistryOps`
implements the node registry described in [node.md](node.md).

The current root requires `binding_ops`, `sources`, and `nodes` to be non-null.
A consumer first checks the root's ABI version and `TESS_API_MIN_SIZE`, which
includes all three fields. Before using a subsystem, it then checks that
table's pointer, ABI version, and minimum size.

Future optional fields can be appended to the root. A consumer checks
`TESS_ABI_HAS_FIELD` before reading such a field, then validates the subsystem
table itself. The current required fields are covered by `TESS_API_MIN_SIZE`
and do not need separate field checks.

## Version zero

`TessApi` and every subsystem operation table begin with an ABI version and
their actual structure size. A version changes when that contract becomes
incompatible. The size lets a consumer check whether a later field is present
before reading it, so compatible operations can be appended without breaking
an older provider.

All public size-tagged structures follow one rule: existing fields keep their
position, type, and meaning, while compatible fields are appended at the end.

`TESS_ABI_SIZE_INCLUDING_FIELD(type, field)` computes the size in bytes from
the start of the structure to the end of `field`, including that field. Each
`TESS_*_MIN_SIZE` definition selects the last required field for its structure.
`TESS_ABI_HAS_FIELD` uses the same calculation to check whether a supplied
structure includes a field, including an optional field, before use.

The two initializer macros fill either a version-and-size header or a
size-only header, so each module reports the structure layout against which
it was compiled.

Version zero is an experimental development ABI, not a compatibility promise.
The first public release will assign a stable nonzero ABI version after the
interfaces have been reviewed together.

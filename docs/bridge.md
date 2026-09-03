# Tessera bridge

The Tessera bridge is a small backend-local service that lets independently
built PostgreSQL extensions find one shared API table. It uses PostgreSQL's
rendezvous variables, so components do not link directly to one another.

The extension is named `tessera`. Creating it loads the shared library, whose
`_PG_init()` publishes `TessApi` under `tessera.api.v0`.

The bridge owns only cross-extension coordination:

- attaching a batch request to a tuple slot;
- later, transferring batches and registering sources and executor nodes.

It will not evaluate expressions, interpret a source-native column format,
choose scan or join algorithms, or own the physical buffers behind a batch.
Those responsibilities belong to runtime libraries, nodes, and sources.

## Slot bindings

A `TessBinding` associates one `TupleTableSlot` with a logical layout and a
request. It lets one extension recover batch information from a slot returned
by another extension through PostgreSQL's unchanged executor interface.

The producer attaches its slot and layout during executor initialization:

```c
binding = api->attach(slot, &layout);
```

A parent configures the columns and output form it needs, then freezes the
request before execution:

```c
const TessRequest *frozen;

api->set_request(binding, &request);
frozen = api->freeze_request(binding);
```

`set_request` may be called again before `freeze_request`. Freezing is
one-way, but repeated calls are safe and return the same request. The initial
request selects row output, no columns, and no batch-size limit.

At an extension boundary that receives only a slot, use `find_binding` once
and retain the result. The bridge currently searches a backend-local list, so
it must not be used for every row or batch. The list is private and can later
be replaced without changing the API.

The bridge copies the layout, target map, request, and column masks into
`slot->tts_mcxt`. Explicitly call `detach` before destroying the slot. A reset
or deletion of the slot's memory context removes the weak list entry
automatically. Returned pointers become invalid after either event.

## Version zero

`TessApi` begins with an ABI version and its actual structure size. The ABI
version changes when an existing contract becomes incompatible. The size lets
a consumer check whether a later field is present before reading it, so
compatible operations can be appended without breaking an older provider.
Operations are added with their implementations and independent consumers.

All public size-tagged structures follow one rule: existing fields keep their
position, type, and meaning, while compatible fields are appended at the end.
`TESS_ABI_HAS_FIELD` checks an appended field before use. The two initializer
macros fill either a version-and-size header or a size-only header, so each
module reports the structure layout against which it was compiled.

Version zero is an experimental development ABI, not a compatibility promise.
The first public release will assign a stable nonzero ABI version after the
interfaces have been reviewed together.

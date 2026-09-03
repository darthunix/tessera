# Slot bindings

`TessBindingOps` lets a producer and a consumer exchange batches through a
`TupleTableSlot`. It manages the connection, the consumer's request, and the
active batch.

A component gets `TessApi` from the rendezvous variable. It checks `TessApi`
and `binding_ops`, then saves the pointer:

```c
const TessBindingOps *ops = api->binding_ops;
```

This lookup is performed during initialization. Batch processing calls the
retained table directly and does not read the root API again.

## Binding lifetime

The producer attaches its slot and layout during executor initialization:

```c
binding = ops->attach(slot, &layout);
```

At an extension boundary that receives only a slot, use `find` once and
retain the result. The bridge currently searches a backend-local list, so it
must not be used for every row or batch. The list is private and can later be
replaced without changing the API.

The bridge copies the layout, target map, request, and column masks into
`slot->tts_mcxt`. Explicitly call `detach` before destroying the slot. A reset
or deletion of the slot's memory context removes the weak list entry
automatically. Returned pointers become invalid after either event.

## Request

A parent configures the columns and output form it needs, then freezes the
request before execution:

```c
const TessRequest *frozen;

ops->set_request(binding, &request);
frozen = ops->freeze_request(binding);
```

`set_request` may be called again before `freeze_request`. Freezing is
one-way, but repeated calls are safe and return the same request. The initial
request selects row output, no columns, and no batch-size limit.

## Active batch

`publish_batch` transfers exclusive use of a `TessBatch` to a binding and
freezes its request. A binding can hold only one active batch. The physical
row count must obey `max_batch_rows`, while its active row mask may be empty.

The consumer obtains the borrowed pointer with `get_batch`. When it is done,
`release_batch` removes the pointer and calls the optional
`TessBatchOps.release` operation. Releasing an empty binding is safe, and
`detach` releases an active batch before removing the binding.

These operations do not change the visible row in `TupleTableSlot`. A later
runtime adapter will handle row materialization and the requirements of
PostgreSQL's scalar executor interface.

A slot-context reset removes only the weak lookup entry. It cannot safely
call the batch release operation because the batch owner's context may
already have been deleted. A producer must therefore protect external
resources with its own memory-context callback or `ResourceOwner`. Normal
executor cleanup should explicitly release the batch or detach the binding.

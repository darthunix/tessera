# Source registry

The source registry lets independently built extensions publish and find
batch sources without linking to one another. A source currently has only a
stable name. Planning and execution interfaces will be added when a scan node
can use them.

Get the registry once during module initialization and keep its operation
table:

```c
const TessSourceRegistryOps *sources;

if (api == NULL || api->abi_version != TESS_API_ABI_VERSION ||
    api->struct_size < TESS_API_MIN_SIZE)
    elog(ERROR, "incompatible Tessera API");

sources = api->sources;
if (sources == NULL ||
    sources->abi_version != TESS_SOURCE_REGISTRY_OPS_ABI_VERSION ||
    sources->struct_size < TESS_SOURCE_REGISTRY_OPS_MIN_SIZE)
    elog(ERROR, "incompatible Tessera source registry");
```

`sources` is a required, non-null field of the current `TessApi`.
`TESS_API_MIN_SIZE` includes it, so validating the root's version and size
allows the consumer to read it. The registry's pointer, version, and size are
then checked separately. `TESS_ABI_HAS_FIELD` is needed only for future
optional fields.

## Registration

The provider defines a source with static storage and registers it:

```c
static const TessSource my_source = {
    TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
    .name = "my_extension.scan",
};

sources->add(&my_source);
```

Names are case-sensitive and must be unique in one PostgreSQL backend. Adding
the same pointer again is safe. Adding another source with the same name is an
error.

## Ownership and lifetime

The provider owns the `TessSource` structure and the string referenced by
`name`. The registry stores the provider's pointer without copying the
structure or name string. The structure and name string must stay valid from
registration until `sources->remove(&my_source)` returns. Removing the same
source more than once is safe while the object is still alive; removing
`NULL` is also safe.

`sources->find("my_extension.scan")` returns the registered pointer or `NULL`.
The returned pointer is borrowed: the consumer must not modify or free the
source or its name. Finding a source does not extend its lifetime, and the
consumer must not use a previously returned pointer after removal.

Before removing a source, the provider must ensure that all consumers have
finished using their borrowed pointers. `remove` only unregisters the source;
it does not wait for consumers or free the structure or its name. After
removal, the provider may free its own allocations. The static structure and
string literal in the example above need no explicit freeing.

For a dynamically allocated source, use a memory context that stays alive
until removal. For example, the provider can allocate and release both
objects as follows:

```c
TessSource *source = palloc(sizeof(*source));
char *name = pstrdup("my_extension.dynamic_scan");

*source = (TessSource) {
    TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
    .name = name,
};
sources->add(source);

/* Consumers may find and use the source while it is registered. */

/* After all consumers have finished using their borrowed pointers: */
sources->remove(source);
pfree(name);
pfree(source);
```

After freeing the source, do not pass its old pointer to `remove` again.

# Node registry

The node registry lets independently built extensions register and find
kinds of batch-producing nodes by stable name. A `TessNode` describes a kind
of node, not the state of one query execution. It currently contains only
identity; planning and execution interfaces will be added with the first
real node consumer, `TessPack`.

## Finding the registry

After the `tessera` bridge has been loaded, get the registry during module
initialization and keep its operation table:

```c
const TessApi *api = *find_rendezvous_variable(TESS_API_RENDEZVOUS);
const TessNodeRegistryOps *nodes;

if (api == NULL || api->abi_version != TESS_API_ABI_VERSION ||
    api->struct_size < TESS_API_MIN_SIZE)
    elog(ERROR, "incompatible Tessera API");

nodes = api->nodes;
if (nodes == NULL ||
    nodes->abi_version != TESS_NODE_REGISTRY_OPS_ABI_VERSION ||
    nodes->struct_size < TESS_NODE_REGISTRY_OPS_MIN_SIZE)
    elog(ERROR, "incompatible Tessera node registry");
```

`nodes` is a required, non-null field covered by `TESS_API_MIN_SIZE`.
Reading it after validating the root does not require a separate
`TESS_ABI_HAS_FIELD` check. The registry table has its own version and size.

## Registration and lookup

The provider defines a description with static storage and registers it:

```c
static const TessNode my_node = {
    TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
    .name = "my_extension.limit",
};

nodes->add(&my_node);
```

`nodes->find("my_extension.limit")` returns the registered pointer or `NULL`.
An absent, null, or empty lookup name returns `NULL`.

Names are nonempty, case-sensitive, and unique among nodes in one PostgreSQL
backend. Registering the same pointer again is safe. Another description with
the same name is an error. A null description, incompatible ABI version or
size, or a null or empty registration name is also an error. Descriptions
with additional trailing fields are accepted.

Source and node names belong to separate registries. A source and a node may
use the same name; changing one registry does not affect the other. Each
PostgreSQL backend has its own registrations, including parallel workers.

## Ownership and removal

The provider owns the `TessNode` structure and the string referenced by
`name`. The registry stores the pointer without copying the structure or name
string. The structure and name string must remain valid and unchanged from
registration until `remove` returns.

The pointer returned by `find` is borrowed. The consumer must not modify or
free the description or its name, and finding it does not extend its
lifetime. Before removing a description, the provider must ensure that all
consumers have finished using their borrowed pointers:

```c
nodes->remove(&my_node);
```

`remove` unregisters only this exact object. A different object with the same
name cannot remove it. An unregistered object, `NULL`, and repeated calls for
an object that is still alive are safe. Once an object has been removed, its
name can be registered again.

Removal does not wait for consumers or free the description or its name.
After removal, consumers must stop using previously returned pointers. The
provider may then free dynamic allocations with the matching allocator. The
static description and string literal above need no explicit freeing. Do
not pass an old pointer to `remove` after freeing its object.

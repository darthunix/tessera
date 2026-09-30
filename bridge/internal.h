/* Definitions shared within the Tessera bridge library. */
#ifndef TESSERA_BRIDGE_INTERNAL_H
#define TESSERA_BRIDGE_INTERNAL_H

#include "nodes/pg_list.h"

#include "tessera/binding.h"
#include "tessera/function.h"
#include "tessera/kernel_ops.h"
#include "tessera/node.h"
#include "tessera/source.h"

extern const TessBindingOps tess_binding_ops;
extern const TessFunctionRegistryOps tess_function_registry_ops;
extern const TessKernelRegistryOps tess_kernel_registry_ops;
extern const TessNodeRegistryOps tess_node_registry_ops;
extern const TessSourceRegistryOps tess_source_registry_ops;

/*
 * The list behind a registry of borrowed descriptions (registry.c): match
 * tells whether an entry has a key, a name or an OID.
 */
typedef bool (*TessRegistryMatch) (const void *entry, const void *key);

/* The entry with the key, or NULL. */
extern const void *tess_registry_find(List *entries, TessRegistryMatch match,
									  const void *key);

/*
 * Append entry, whose key is key, unless it is there already; false when
 * another entry has the key, which the caller reports.
 */
extern bool tess_registry_add(List **entries, const void *entry,
							  TessRegistryMatch match, const void *key);

/* Remove entry by its address; an absent or NULL one is ignored. */
extern void tess_registry_remove(List **entries, const void *entry);

#endif /* TESSERA_BRIDGE_INTERNAL_H */

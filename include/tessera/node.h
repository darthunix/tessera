/* Registry of batch-producing node kinds from independent extensions. */
#ifndef TESSERA_NODE_H
#define TESSERA_NODE_H

#include "postgres.h"

#include "nodes/execnodes.h"
#include "nodes/pathnodes.h"

#include "tessera/abi.h"

#define TESS_NODE_ABI_VERSION 0
#define TESS_NODE_REGISTRY_OPS_ABI_VERSION 0

/*
 * Stable identity of a kind of batch-producing node, not one execution.
 *
 * The provider owns the TessNode structure and the string referenced by
 * name. The registry stores the provider's pointer without copying the
 * structure or name string. The structure and name string must remain valid
 * and unchanged from add until remove returns. The provider must ensure that
 * all consumers have finished using their borrowed pointers before remove,
 * then may free its allocations. Planning callbacks are optional trailing
 * fields: a consumer checks them with TESS_ABI_HAS_FIELD.
 */
typedef struct TessNode
{
	uint32		abi_version;
	Size		struct_size;
	const char *name;
	/*
	 * Optional: build this kind's path over a child path that returns
	 * ordinary rows, or NULL when the kind cannot. The pack node publishes
	 * it, and the runtime's tess_batch_input_path calls it.
	 */
	CustomPath *(*wrap_rows) (PlannerInfo *root, Path *child);
	/*
	 * Optional: the node above needs at most tuples_needed rows from this
	 * node's execution, or -1 for no bound, as ExecSetTupleBound tells the
	 * core nodes. A node that passes rows through forwards the bound to its
	 * child; the unary helper calls it for a node's child.
	 */
	void		(*set_tuple_bound) (CustomScanState *node, int64 tuples_needed);
	/*
	 * Optional: build this kind's path reading the relation of a
	 * sequential scan path in batches, evaluating none of the relation's
	 * clauses, or NULL when the kind cannot. The heap scan node publishes
	 * it, and the runtime's tess_batch_scan_path calls it.
	 */
	CustomPath *(*scan_rows) (PlannerInfo *root, Path *path);
} TessNode;

#define TESS_NODE_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessNode, name)

/* Backend-local registry shared by independently built extensions. */
typedef struct TessNodeRegistryOps
{
	uint32		abi_version;
	Size		struct_size;
	/* Register without copying. Repeating the same registration is safe. */
	void		(*add) (const TessNode *node);
	/*
	 * Unregister this exact node without freeing it or waiting for users.
	 * NULL and repeated calls for an object that is still alive are safe.
	 */
	void		(*remove) (const TessNode *node);
	/*
	 * Find by case-sensitive name, or return NULL. The borrowed pointer does
	 * not extend the node's lifetime and must not be used after remove.
	 * The consumer must not modify or free the node or its name.
	 */
	const TessNode *(*find) (const char *name);
} TessNodeRegistryOps;

#define TESS_NODE_REGISTRY_OPS_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessNodeRegistryOps, find)

#endif /* TESSERA_NODE_H */

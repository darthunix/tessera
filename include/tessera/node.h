/* Registry of batch-producing node kinds from independent extensions. */
#ifndef TESSERA_NODE_H
#define TESSERA_NODE_H

#include "postgres.h"

#include "nodes/execnodes.h"
#include "nodes/pathnodes.h"

#include "tessera/abi.h"
#include "tessera/table_key.h"

#define TESS_NODE_ABI_VERSION 0
#define TESS_NODE_REGISTRY_OPS_ABI_VERSION 0

/*
 * A filter of key hashes a parent hands its child while executing: a hash
 * join's Bloom filter of its build side's keys, which a child may apply to
 * its rows before its costlier work, when a row the filter rejects cannot
 * reach the parent's output. The keys are columns of the child's batches
 * with their kinds, hashed as tess_int4_hash and tess_int8_hash and their
 * _next forms hash a join's keys, a NULL key rejected; the words are the
 * filter, as tess_bloom_probe reads it, or, when shared, a shared filter
 * the child checks rows against only once tess_bloom_shared_ready says it
 * is. Everything is the parent's. The struct and its arrays of columns and
 * kinds are valid only during the call that hands them, and a child copies
 * what it keeps of them; the words stay valid until the parent takes the
 * filter back.
 */
typedef struct TessKeyFilter
{
	Size		struct_size;
	int			nkeys;
	const int  *columns;
	const TessTableKeyKind *kinds;
	uint64	   *words;
	Size		nwords;
	bool		shared;
} TessKeyFilter;

#define TESS_KEY_FILTER_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessKeyFilter, shared)

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
	/*
	 * Optional: apply the parent's key filter to this node's batches from
	 * now on, copying what it keeps of the struct, or stop applying one when
	 * filter is NULL; false when the node does not take it, and then
	 * nothing changes, a filter it held kept. The runtime's
	 * tess_input_set_key_filter calls it for a node's child.
	 */
	bool		(*set_key_filter) (CustomScanState *node,
								   const TessKeyFilter *filter);
	/*
	 * Optional: build this kind's path in place of an Append path, over
	 * batch paths of the Append's children, or NULL when the kind cannot.
	 * The append node publishes it, and the runtime's tess_batch_input_path
	 * calls it before it packs an Append's rows.
	 */
	CustomPath *(*wrap_append) (PlannerInfo *root, Path *append);
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

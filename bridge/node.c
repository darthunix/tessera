#include "postgres.h"

#include "nodes/pg_list.h"
#include "utils/memutils.h"

#include "internal.h"

/* Borrowed node descriptions; only the list cells belong to the bridge. */
static List *nodes = NIL;

static void add_node(const TessNode *node);
static void remove_node(const TessNode *node);
static const TessNode *find_node(const char *name);

const TessNodeRegistryOps tess_node_registry_ops = {
	TESS_ABI_INITIALIZER(TESS_NODE_REGISTRY_OPS_ABI_VERSION,
		TessNodeRegistryOps),
	.add = add_node,
	.remove = remove_node,
	.find = find_node,
};

static void
validate_node(const TessNode *node)
{
	if (node == NULL)
		elog(ERROR, "Tessera cannot register a null node");
	if (node->abi_version != TESS_NODE_ABI_VERSION ||
		node->struct_size < TESS_NODE_MIN_SIZE)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("incompatible Tessera node ABI"),
				 errdetail("Expected version %u and at least %zu bytes, "
						   "got version %u and %zu bytes.",
						   TESS_NODE_ABI_VERSION, TESS_NODE_MIN_SIZE,
						   node->abi_version, node->struct_size)));
	if (node->name == NULL || node->name[0] == '\0')
		elog(ERROR, "Tessera node must have a name");
}

static void
add_node(const TessNode *node)
{
	MemoryContext oldcontext;

	validate_node(node);
	foreach_ptr(const TessNode, existing, nodes)
	{
		if (strcmp(existing->name, node->name) != 0)
			continue;
		if (existing == node)
			return;
		ereport(ERROR,
				(errcode(ERRCODE_DUPLICATE_OBJECT),
				 errmsg("Tessera node \"%s\" is already registered",
						node->name)));
	}

	oldcontext = MemoryContextSwitchTo(TopMemoryContext);
	nodes = lappend(nodes, (void *) node);
	MemoryContextSwitchTo(oldcontext);
}

static void
remove_node(const TessNode *node)
{
	ListCell   *cell;

	if (node == NULL)
		return;
	foreach(cell, nodes)
	{
		if (lfirst(cell) == node)
		{
			nodes = foreach_delete_current(nodes, cell);
			return;
		}
	}
}

static const TessNode *
find_node(const char *name)
{
	if (name == NULL || name[0] == '\0')
		return NULL;
	foreach_ptr(const TessNode, node, nodes)
	{
		if (strcmp(node->name, name) == 0)
			return node;
	}
	return NULL;
}

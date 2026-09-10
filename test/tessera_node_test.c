#include "postgres.h"

#include "fmgr.h"

#include "tessera/bridge.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_node_registry);
PG_FUNCTION_INFO_V1(tessera_test_node_sizes);
PG_FUNCTION_INFO_V1(tessera_test_node_ownership);
PG_FUNCTION_INFO_V1(tessera_test_node_source_names);
PG_FUNCTION_INFO_V1(tessera_test_invalid_node);
PG_FUNCTION_INFO_V1(tessera_test_duplicate_node);

static const TessNode node_one = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = "tessera_test.node.one",
};

static const TessNode node_two = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = "tessera_test.node.two",
};

static const TessNode node_case = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = "Tessera_test.node.one",
};

static const TessNode duplicate_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = "tessera_test.node.duplicate",
};

static const TessNode duplicate_name = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = "tessera_test.node.duplicate",
};

typedef struct ExtendedNode
{
	TessNode	base;
	void	   *future_field;
} ExtendedNode;

static const TessApi *
get_api(void)
{
	const TessApi *api;
	void	  **rendezvous;

	rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	api = *rendezvous;
	if (api == NULL || api->abi_version != TESS_API_ABI_VERSION ||
		api->struct_size < TESS_API_MIN_SIZE ||
		api->nodes == NULL ||
		api->nodes->abi_version != TESS_NODE_REGISTRY_OPS_ABI_VERSION ||
		api->nodes->struct_size < TESS_NODE_REGISTRY_OPS_MIN_SIZE)
		elog(ERROR, "Tessera test could not find a compatible node registry");
	return api;
}

Datum
tessera_test_node_registry(PG_FUNCTION_ARGS)
{
	const TessNodeRegistryOps *nodes = get_api()->nodes;
	TessNode	other = node_one;
	bool		result;

	result = nodes->find(NULL) == NULL && nodes->find("") == NULL &&
		nodes->find(node_one.name) == NULL;
	nodes->remove(NULL);
	nodes->remove(&node_one);
	nodes->add(&node_one);
	nodes->add(&node_one);
	nodes->add(&node_two);
	nodes->add(&node_case);
	result = result && nodes->find(node_one.name) == &node_one &&
		nodes->find(node_two.name) == &node_two &&
		nodes->find(node_case.name) == &node_case;

	/* A different object with the same name cannot remove the registration. */
	nodes->remove(&other);
	result = result && nodes->find(node_one.name) == &node_one;
	nodes->remove(&node_two);
	result = result && nodes->find(node_two.name) == NULL &&
		nodes->find(node_one.name) == &node_one &&
		nodes->find(node_case.name) == &node_case;
	nodes->remove(&node_one);
	nodes->remove(&node_one);
	result = result && nodes->find(node_one.name) == NULL &&
		nodes->find(node_case.name) == &node_case;

	/* The name can be reused without giving the old object control over it. */
	nodes->add(&other);
	nodes->remove(&node_one);
	result = result && nodes->find(other.name) == &other;
	nodes->remove(&other);
	nodes->remove(&node_case);
	result = result && nodes->find(node_one.name) == NULL &&
		nodes->find(node_case.name) == NULL;

	PG_RETURN_BOOL(result);
}

Datum
tessera_test_node_sizes(PG_FUNCTION_ARGS)
{
	const TessNodeRegistryOps *nodes = get_api()->nodes;
	TessNode	short_node = node_one;
	ExtendedNode extended = {
		.base = node_two,
	};
	bool		result;

	short_node.name = "tessera_test.node.short";
	short_node.struct_size = TESS_NODE_MIN_SIZE;
	extended.base.name = "tessera_test.node.extended";
	extended.base.struct_size = sizeof(extended);
	nodes->add(&short_node);
	nodes->add(&extended.base);
	result = nodes->find(short_node.name) == &short_node &&
		nodes->find(extended.base.name) == &extended.base;
	nodes->remove(&short_node);
	nodes->remove(&extended.base);

	PG_RETURN_BOOL(result);
}

Datum
tessera_test_node_ownership(PG_FUNCTION_ARGS)
{
	const TessNodeRegistryOps *nodes = get_api()->nodes;
	TessNode   *node = palloc(sizeof(*node));
	char	   *name = pstrdup("tessera_test.node.dynamic");
	bool		result;

	*node = node_one;
	node->name = name;
	nodes->add(node);
	result = nodes->find(name) == node;
	nodes->remove(node);
	result = result && nodes->find("tessera_test.node.dynamic") == NULL &&
		node->abi_version == TESS_NODE_ABI_VERSION &&
		node->struct_size == sizeof(*node) && node->name == name &&
		strcmp(name, "tessera_test.node.dynamic") == 0;
	nodes->remove(node);
	pfree(name);
	pfree(node);

	PG_RETURN_BOOL(result);
}

Datum
tessera_test_node_source_names(PG_FUNCTION_ARGS)
{
	const TessApi *api = get_api();
	const TessNodeRegistryOps *nodes = api->nodes;
	const TessSourceRegistryOps *sources = api->sources;
	TessNode	node = {
		TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
		.name = "tessera_test.shared",
	};
	TessSource	source = {
		TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
		.name = "tessera_test.shared",
	};
	bool		result;

	if (sources == NULL ||
		sources->abi_version != TESS_SOURCE_REGISTRY_OPS_ABI_VERSION ||
		sources->struct_size < TESS_SOURCE_REGISTRY_OPS_MIN_SIZE)
		elog(ERROR, "Tessera test could not find a compatible source registry");

	sources->add(&source);
	nodes->add(&node);
	result = sources->find(source.name) == &source &&
		nodes->find(node.name) == &node;
	sources->remove(&source);
	result = result && sources->find(source.name) == NULL &&
		nodes->find(node.name) == &node;
	sources->add(&source);
	nodes->remove(&node);
	result = result && nodes->find(node.name) == NULL &&
		sources->find(source.name) == &source;
	sources->remove(&source);

	PG_RETURN_BOOL(result);
}

Datum
tessera_test_invalid_node(PG_FUNCTION_ARGS)
{
	const TessNodeRegistryOps *nodes = get_api()->nodes;
	TessNode	invalid = node_one;
	int32		kind = PG_GETARG_INT32(0);

	invalid.name = "tessera_test.node.invalid";
	if (kind == 0)
		nodes->add(NULL);
	else if (kind == 1)
		invalid.abi_version++;
	else if (kind == 2)
		invalid.struct_size = TESS_NODE_MIN_SIZE - 1;
	else if (kind == 3)
		invalid.name = NULL;
	else if (kind == 4)
		invalid.name = "";
	else
		elog(ERROR, "unknown Tessera invalid-node test");
	nodes->add(&invalid);
	PG_RETURN_VOID();
}

Datum
tessera_test_duplicate_node(PG_FUNCTION_ARGS)
{
	const TessNodeRegistryOps *nodes = get_api()->nodes;

	nodes->add(&duplicate_node);
	nodes->add(&duplicate_name);
	PG_RETURN_VOID();
}

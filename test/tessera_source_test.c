#include "postgres.h"

#include "fmgr.h"

#include "tessera/bridge.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_source_registry);
PG_FUNCTION_INFO_V1(tessera_test_source_sizes);
PG_FUNCTION_INFO_V1(tessera_test_invalid_source);
PG_FUNCTION_INFO_V1(tessera_test_duplicate_source);

static const TessSource source_one = {
	TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
	.name = "tessera_test.one",
};

static const TessSource source_two = {
	TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
	.name = "tessera_test.two",
};

static const TessSource duplicate_source = {
	TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
	.name = "tessera_test.duplicate",
};

static const TessSource duplicate_name = {
	TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
	.name = "tessera_test.duplicate",
};

typedef struct ExtendedSource
{
	TessSource	base;
	void	   *future_field;
} ExtendedSource;

static const TessSourceRegistryOps *
get_sources(void)
{
	const TessApi *api;
	void	  **rendezvous;

	rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	api = *rendezvous;
	if (api == NULL || api->abi_version != TESS_API_ABI_VERSION ||
		api->struct_size < TESS_API_MIN_SIZE ||
		api->sources == NULL ||
		api->sources->abi_version != TESS_SOURCE_REGISTRY_OPS_ABI_VERSION ||
		api->sources->struct_size < TESS_SOURCE_REGISTRY_OPS_MIN_SIZE)
		elog(ERROR, "Tessera test could not find a compatible source registry");
	return api->sources;
}

Datum
tessera_test_source_registry(PG_FUNCTION_ARGS)
{
	const TessSourceRegistryOps *sources = get_sources();
	TessSource *dynamic_source;
	char	   *dynamic_name;
	bool		result;

	result = sources->find(NULL) == NULL && sources->find("") == NULL &&
		sources->find(source_one.name) == NULL;
	sources->add(&source_one);
	sources->add(&source_one);
	sources->add(&source_two);
	result = result && sources->find(source_one.name) == &source_one &&
		sources->find(source_two.name) == &source_two;
	sources->remove(&source_one);
	result = result && sources->find(source_one.name) == NULL &&
		sources->find(source_two.name) == &source_two;
	sources->remove(&source_one);
	sources->remove(NULL);
	sources->remove(&source_two);
	result = result && sources->find(source_two.name) == NULL;

	/* Removing a dynamic source leaves both allocations with the provider. */
	dynamic_source = palloc(sizeof(*dynamic_source));
	dynamic_name = pstrdup("tessera_test.dynamic");
	*dynamic_source = source_one;
	dynamic_source->name = dynamic_name;
	sources->add(dynamic_source);
	result = result && sources->find(dynamic_name) == dynamic_source;
	sources->remove(dynamic_source);
	result = result && sources->find("tessera_test.dynamic") == NULL &&
		dynamic_source->abi_version == TESS_SOURCE_ABI_VERSION &&
		dynamic_source->struct_size == sizeof(*dynamic_source) &&
		dynamic_source->name == dynamic_name &&
		strcmp(dynamic_name, "tessera_test.dynamic") == 0;
	sources->remove(dynamic_source);
	pfree(dynamic_name);
	pfree(dynamic_source);

	PG_RETURN_BOOL(result);
}

Datum
tessera_test_source_sizes(PG_FUNCTION_ARGS)
{
	const TessSourceRegistryOps *sources = get_sources();
	TessSource	short_source = source_one;
	ExtendedSource extended = {
		.base = source_two,
	};
	bool		result;

	short_source.name = "tessera_test.short";
	short_source.struct_size = TESS_SOURCE_MIN_SIZE;
	extended.base.name = "tessera_test.extended";
	extended.base.struct_size = sizeof(extended);
	sources->add(&short_source);
	sources->add(&extended.base);
	result = sources->find(short_source.name) == &short_source &&
		sources->find(extended.base.name) == &extended.base;
	sources->remove(&short_source);
	sources->remove(&extended.base);

	PG_RETURN_BOOL(result);
}

Datum
tessera_test_invalid_source(PG_FUNCTION_ARGS)
{
	const TessSourceRegistryOps *sources = get_sources();
	TessSource	invalid = source_one;
	int32		kind = PG_GETARG_INT32(0);

	invalid.name = "tessera_test.invalid";
	if (kind == 0)
		sources->add(NULL);
	else if (kind == 1)
		invalid.abi_version++;
	else if (kind == 2)
		invalid.struct_size = TESS_SOURCE_MIN_SIZE - 1;
	else if (kind == 3)
		invalid.name = NULL;
	else if (kind == 4)
		invalid.name = "";
	else
		elog(ERROR, "unknown Tessera invalid-source test");
	sources->add(&invalid);
	PG_RETURN_VOID();
}

Datum
tessera_test_duplicate_source(PG_FUNCTION_ARGS)
{
	const TessSourceRegistryOps *sources = get_sources();

	sources->add(&duplicate_source);
	sources->add(&duplicate_name);
	PG_RETURN_VOID();
}

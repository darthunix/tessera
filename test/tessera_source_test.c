#include "postgres.h"

#include "fmgr.h"
#include "lib/stringinfo.h"
#include "utils/builtins.h"

#include "tessera/bridge.h"
#include "tessera/runtime_api.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_source_registry);
PG_FUNCTION_INFO_V1(tessera_test_source_sizes);
PG_FUNCTION_INFO_V1(tessera_test_invalid_source);
PG_FUNCTION_INFO_V1(tessera_test_duplicate_source);
PG_FUNCTION_INFO_V1(tessera_test_duplicate_cleanup);
PG_FUNCTION_INFO_V1(tessera_test_source_list);
PG_FUNCTION_INFO_V1(tessera_test_source_remove);
PG_FUNCTION_INFO_V1(tessera_test_source_next_unknown);
PG_FUNCTION_INFO_V1(tessera_test_source_registry_check);

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

/* A source of the least size: the fields up to name, and no more memory. */
typedef struct ShortSource
{
	uint32		abi_version;
	Size		struct_size;
	const char *name;
} ShortSource;

/* The registry's table as a module built before next had it. */
typedef struct OldRegistryOps
{
	uint32		abi_version;
	Size		struct_size;
	void		(*add) (const TessSource *source);
	void		(*remove) (const TessSource *source);
	const TessSource *(*find) (const char *name);
} OldRegistryOps;

static const TessSourceRegistryOps *
get_sources(void)
{
	const TessApi *api;
	void	  **rendezvous;

	rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	api = *rendezvous;
	if (api == NULL || api->abi_version != TESS_API_ABI_VERSION ||
		api->struct_size < TESS_API_MIN_SIZE)
		elog(ERROR, "Tessera test could not find a compatible bridge");
	return tess_source_registry_check(api->sources);
}

Datum
tessera_test_source_registry(PG_FUNCTION_ARGS)
{
	const TessSourceRegistryOps *sources = get_sources();
	TessSource *dynamic_source;
	char	   *dynamic_name;
	bool		result;

	result = sources->find(NULL) == NULL && sources->find("") == NULL &&
		sources->find(source_one.name) == NULL && sources->next(NULL) == NULL;
	sources->add(&source_one);
	sources->add(&source_one);
	sources->add(&source_two);
	result = result && sources->find(source_one.name) == &source_one &&
		sources->find(source_two.name) == &source_two &&
		sources->find(NULL) == NULL && sources->find("") == NULL &&
		sources->next(NULL) == &source_one &&
		sources->next(&source_one) == &source_two &&
		sources->next(&source_two) == NULL;
	sources->remove(&source_one);
	result = result && sources->find(source_one.name) == NULL &&
		sources->find(source_two.name) == &source_two &&
		sources->next(NULL) == &source_two &&
		sources->next(&source_two) == NULL;
	sources->remove(&source_one);
	sources->remove(NULL);
	sources->remove(&duplicate_source);
	result = result && sources->next(NULL) == &source_two &&
		sources->next(&source_two) == NULL;
	sources->remove(&source_two);
	result = result && sources->find(source_two.name) == NULL &&
		sources->next(NULL) == NULL;

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
	/* Allocated at its own size, so that a read past name is caught. */
	ShortSource *short_memory = palloc(sizeof(ShortSource));
	const TessSource *short_source = (const TessSource *) short_memory;
	ExtendedSource extended = {
		.base = source_two,
	};
	bool		result;

	StaticAssertStmt(sizeof(ShortSource) == TESS_SOURCE_MIN_SIZE,
					 "a short source ends at name");
	short_memory->abi_version = TESS_SOURCE_ABI_VERSION;
	short_memory->struct_size = sizeof(ShortSource);
	short_memory->name = "tessera_test.short";
	extended.base.name = "tessera_test.extended";
	extended.base.struct_size = sizeof(extended);
	sources->add(short_source);
	sources->add(&extended.base);
	result = sources->find(short_memory->name) == short_source &&
		sources->find(extended.base.name) == &extended.base &&
		sources->next(NULL) == short_source &&
		sources->next(short_source) == &extended.base &&
		sources->next(&extended.base) == NULL;
	sources->remove(short_source);
	sources->remove(&extended.base);
	result = result && sources->next(NULL) == NULL;
	pfree(short_memory);

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

/*
 * After the refused duplicate: the first source of the name is still the
 * one registered, and alone; then it is removed.
 */
Datum
tessera_test_duplicate_cleanup(PG_FUNCTION_ARGS)
{
	const TessSourceRegistryOps *sources = get_sources();
	bool		result = sources->find(duplicate_name.name) == &duplicate_source &&
		sources->next(NULL) == &duplicate_source &&
		sources->next(&duplicate_source) == NULL;

	sources->remove(&duplicate_source);
	PG_RETURN_BOOL(result);
}

/* The names of the sources registered, in the order next lists them. */
Datum
tessera_test_source_list(PG_FUNCTION_ARGS)
{
	const TessSourceRegistryOps *sources = get_sources();
	StringInfoData names;

	initStringInfo(&names);
	for (const TessSource *source = sources->next(NULL); source != NULL;
		 source = sources->next(source))
		appendStringInfo(&names, "%s%s", names.len > 0 ? " " : "", source->name);
	PG_RETURN_TEXT_P(cstring_to_text(names.data));
}

/*
 * next given a pointer never registered, beside a registered source, which
 * stays registered: the suite lists it, then removes it.
 */
Datum
tessera_test_source_next_unknown(PG_FUNCTION_ARGS)
{
	const TessSourceRegistryOps *sources = get_sources();

	sources->add(&source_one);
	sources->next(&source_two);
	PG_RETURN_VOID();
}

/* Remove the source of the name; whether one was registered under it. */
Datum
tessera_test_source_remove(PG_FUNCTION_ARGS)
{
	const TessSourceRegistryOps *sources = get_sources();
	const TessSource *source = sources->find(text_to_cstring(PG_GETARG_TEXT_PP(0)));

	sources->remove(source);
	PG_RETURN_BOOL(source != NULL);
}

/*
 * tess_source_registry_check given the bridge's table (0), one that ends
 * at find (1), one of another version (2), and NULL (3). The tables made
 * here have no next to read.
 */
Datum
tessera_test_source_registry_check(PG_FUNCTION_ARGS)
{
	const TessSourceRegistryOps *bridge = get_sources();
	int32		kind = PG_GETARG_INT32(0);
	OldRegistryOps old = {
		.abi_version = TESS_SOURCE_REGISTRY_OPS_ABI_VERSION,
		.struct_size = sizeof(OldRegistryOps),
		.add = bridge->add,
		.remove = bridge->remove,
		.find = bridge->find,
	};
	TessSourceRegistryOps other = *bridge;

	other.abi_version++;
	other.next = NULL;
	if (kind == 0)
		PG_RETURN_BOOL(tess_source_registry_check(bridge) == bridge);
	if (kind == 1)
		tess_source_registry_check((const TessSourceRegistryOps *) &old);
	else if (kind == 2)
		tess_source_registry_check(&other);
	else if (kind == 3)
		tess_source_registry_check(NULL);
	else
		elog(ERROR, "unknown Tessera registry-check test");
	PG_RETURN_BOOL(false);
}

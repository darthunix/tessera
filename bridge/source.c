#include "postgres.h"

#include "nodes/pg_list.h"

#include "internal.h"

/* Borrowed source descriptions; only the list cells belong to the bridge. */
static List *sources = NIL;

static void add_source(const TessSource *source);
static void remove_source(const TessSource *source);
static const TessSource *find_source(const char *name);

const TessSourceRegistryOps tess_source_registry_ops = {
	TESS_ABI_INITIALIZER(TESS_SOURCE_REGISTRY_OPS_ABI_VERSION,
		TessSourceRegistryOps),
	.add = add_source,
	.remove = remove_source,
	.find = find_source,
};

static void
validate_source(const TessSource *source)
{
	if (source == NULL)
		elog(ERROR, "Tessera cannot register a null source");
	if (source->abi_version != TESS_SOURCE_ABI_VERSION ||
		source->struct_size < TESS_SOURCE_MIN_SIZE)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("incompatible Tessera source ABI"),
				 errdetail("Expected version %u and at least %zu bytes, "
						   "got version %u and %zu bytes.",
						   TESS_SOURCE_ABI_VERSION, TESS_SOURCE_MIN_SIZE,
						   source->abi_version, source->struct_size)));
	if (source->name == NULL || source->name[0] == '\0')
		elog(ERROR, "Tessera source must have a name");
}

/* Whether a registered source has the name. */
static bool
same_name(const void *entry, const void *name)
{
	return strcmp(((const TessSource *) entry)->name, name) == 0;
}

static void
add_source(const TessSource *source)
{
	validate_source(source);
	if (!tess_registry_add(&sources, source, same_name, source->name))
		ereport(ERROR,
				(errcode(ERRCODE_DUPLICATE_OBJECT),
				 errmsg("Tessera source \"%s\" is already registered",
						source->name)));
}

static void
remove_source(const TessSource *source)
{
	tess_registry_remove(&sources, source);
}

static const TessSource *
find_source(const char *name)
{
	if (name == NULL || name[0] == '\0')
		return NULL;
	return tess_registry_find(sources, same_name, name);
}

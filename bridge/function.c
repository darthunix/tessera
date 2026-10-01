#include "postgres.h"

#include "utils/hsearch.h"
#include "utils/memutils.h"

#include "internal.h"

/*
 * Borrowed function descriptions by OID; only the table's entries belong
 * to the bridge. Expressions look functions up many times while they are
 * planned, over two hundred registered: a hash table, not a list.
 */
typedef struct FunctionEntry
{
	Oid			funcid;
	const TessFunction *function;
} FunctionEntry;

static HTAB *functions = NULL;

static void add_function(const TessFunction *function);
static void remove_function(const TessFunction *function);
static const TessFunction *find_function(Oid funcid);

const TessFunctionRegistryOps tess_function_registry_ops = {
	TESS_ABI_INITIALIZER(TESS_FUNCTION_REGISTRY_OPS_ABI_VERSION,
		TessFunctionRegistryOps),
	.add = add_function,
	.remove = remove_function,
	.find = find_function,
};

static void
validate_function(const TessFunction *function)
{
	if (function == NULL)
		elog(ERROR, "Tessera cannot register a null function");
	if (function->abi_version != TESS_FUNCTION_ABI_VERSION ||
		function->struct_size < TESS_FUNCTION_MIN_SIZE)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("incompatible Tessera function ABI"),
				 errdetail("Expected version %u and at least %zu bytes, "
						   "got version %u and %zu bytes.",
						   TESS_FUNCTION_ABI_VERSION, TESS_FUNCTION_MIN_SIZE,
						   function->abi_version, function->struct_size)));
	if (!OidIsValid(function->funcid))
		elog(ERROR, "Tessera function must name a PostgreSQL function");
	if (function->kind == TESS_FUNCTION_EQUIVALENT)
	{
		/* Its target may be registered later; the consumer looks it up. */
		if (function->struct_size < TESS_FUNCTION_EQUIVALENT_MIN_SIZE)
			ereport(ERROR,
					(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
					 errmsg("incompatible Tessera function ABI"),
					 errdetail("An equivalent needs at least %zu bytes, got %zu.",
							   TESS_FUNCTION_EQUIVALENT_MIN_SIZE,
							   function->struct_size)));
		if (!OidIsValid(function->equivalent) ||
			function->equivalent == function->funcid)
			elog(ERROR, "Tessera equivalent must name another PostgreSQL function");
		if (function->evaluate != NULL)
			elog(ERROR, "Tessera equivalent has no evaluate callback");
	}
	else if (function->evaluate == NULL)
		elog(ERROR, "Tessera function must have an evaluate callback");
	if (function->kind != TESS_FUNCTION_PREDICATE &&
		function->kind != TESS_FUNCTION_VALUE &&
		function->kind != TESS_FUNCTION_AGGREGATE &&
		function->kind != TESS_FUNCTION_EQUIVALENT)
		elog(ERROR, "Tessera function has an unknown kind");
	if (function->result_format != TESS_RESULT_DATUM &&
		function->result_format != TESS_RESULT_INT32)
		elog(ERROR, "Tessera function has an unknown result format");
	if (function->kind == TESS_FUNCTION_AGGREGATE &&
		function->result_format != TESS_RESULT_DATUM)
		elog(ERROR, "Tessera aggregate must produce a Datum");
	if ((function->flags & TESS_FUNCTION_STRICT) == 0)
		elog(ERROR, "Tessera function must be strict");
}

static void
add_function(const TessFunction *function)
{
	FunctionEntry *entry;
	bool		found;

	validate_function(function);
	if (functions == NULL)
	{
		HASHCTL		ctl = {0};

		ctl.keysize = sizeof(Oid);
		ctl.entrysize = sizeof(FunctionEntry);
		ctl.hcxt = TopMemoryContext;
		functions = hash_create("Tessera functions", 256, &ctl,
								HASH_ELEM | HASH_BLOBS | HASH_CONTEXT);
	}
	entry = hash_search(functions, &function->funcid, HASH_ENTER, &found);
	/*
	 * The OID only: registration runs in _PG_init, in the postmaster too,
	 * where the catalog cannot be read to name the function. The same
	 * description again is no error.
	 */
	if (found && entry->function != function)
		ereport(ERROR,
				(errcode(ERRCODE_DUPLICATE_OBJECT),
				 errmsg("Tessera function with OID %u is already registered",
						function->funcid)));
	entry->function = function;
}

static void
remove_function(const TessFunction *function)
{
	FunctionEntry *entry;

	if (function == NULL || functions == NULL)
		return;
	entry = hash_search(functions, &function->funcid, HASH_FIND, NULL);
	if (entry != NULL && entry->function == function)
		hash_search(functions, &function->funcid, HASH_REMOVE, NULL);
}

/*
 * The registry answers first. A later version will also ask the function's
 * planner support function (pg_proc.prosupport) with an ExtensibleNode
 * request named "tessera.batch_function" and cache its answer by OID.
 */
static const TessFunction *
find_function(Oid funcid)
{
	FunctionEntry *entry;

	if (!OidIsValid(funcid) || functions == NULL)
		return NULL;
	entry = hash_search(functions, &funcid, HASH_FIND, NULL);
	return entry != NULL ? entry->function : NULL;
}

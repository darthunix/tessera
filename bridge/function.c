#include "postgres.h"

#include "nodes/pg_list.h"
#include "utils/memutils.h"
#include "utils/regproc.h"

#include "internal.h"

/* Borrowed function descriptions; only the list cells belong to the bridge. */
static List *functions = NIL;

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
	MemoryContext oldcontext;

	validate_function(function);
	foreach_ptr(const TessFunction, existing, functions)
	{
		if (existing->funcid != function->funcid)
			continue;
		if (existing == function)
			return;
		ereport(ERROR,
				(errcode(ERRCODE_DUPLICATE_OBJECT),
				 errmsg("Tessera function %s is already registered",
						format_procedure(function->funcid))));
	}

	oldcontext = MemoryContextSwitchTo(TopMemoryContext);
	functions = lappend(functions, (void *) function);
	MemoryContextSwitchTo(oldcontext);
}

static void
remove_function(const TessFunction *function)
{
	ListCell   *cell;

	if (function == NULL)
		return;
	foreach(cell, functions)
	{
		if (lfirst(cell) == function)
		{
			functions = foreach_delete_current(functions, cell);
			return;
		}
	}
}

/*
 * The registry answers first. A later version will also ask the function's
 * planner support function (pg_proc.prosupport) with an ExtensibleNode
 * request named "tessera.batch_function" and cache its answer by OID.
 */
static const TessFunction *
find_function(Oid funcid)
{
	if (!OidIsValid(funcid))
		return NULL;
	foreach_ptr(const TessFunction, function, functions)
	{
		if (function->funcid == funcid)
			return function;
	}
	return NULL;
}

#include "postgres.h"

#include "fmgr.h"
#include "utils/fmgroids.h"

#include "tessera/bridge.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_function_registry);
PG_FUNCTION_INFO_V1(tessera_test_function_sizes);
PG_FUNCTION_INFO_V1(tessera_test_invalid_function);
PG_FUNCTION_INFO_V1(tessera_test_duplicate_function);

static TessStatusCode
noop_evaluate(TessFunctionCall *call)
{
	return TESS_OK;
}

/* Descriptions for functions no real module registers in these tests. */
static const TessFunction function_one = {
	TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction),
	.funcid = F_INT8PL,
	.kind = TESS_FUNCTION_VALUE,
	.result_format = TESS_RESULT_DATUM,
	.flags = TESS_FUNCTION_STRICT | TESS_FUNCTION_ANY_SHAPE,
	.evaluate = noop_evaluate,
};

static const TessFunction function_two = {
	TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction),
	.funcid = F_INT8MI,
	.kind = TESS_FUNCTION_PREDICATE,
	.result_format = TESS_RESULT_DATUM,
	.flags = TESS_FUNCTION_STRICT,
	.evaluate = noop_evaluate,
};

typedef struct ExtendedFunction
{
	TessFunction base;
	void	   *future_field;
} ExtendedFunction;

static const TessApi *
get_api(void)
{
	const TessApi *api;
	void	  **rendezvous;

	rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	api = *rendezvous;
	if (api == NULL || api->abi_version != TESS_API_ABI_VERSION ||
		api->struct_size < TESS_API_MIN_SIZE ||
		api->functions == NULL ||
		api->functions->abi_version != TESS_FUNCTION_REGISTRY_OPS_ABI_VERSION ||
		api->functions->struct_size < TESS_FUNCTION_REGISTRY_OPS_MIN_SIZE)
		elog(ERROR, "Tessera test could not find a compatible function registry");
	return api;
}

Datum
tessera_test_function_registry(PG_FUNCTION_ARGS)
{
	const TessFunctionRegistryOps *functions = get_api()->functions;
	TessFunction other = function_one;
	bool		result;

	result = functions->find(InvalidOid) == NULL &&
		functions->find(function_one.funcid) == NULL;
	functions->remove(NULL);
	functions->remove(&function_one);
	functions->add(&function_one);
	functions->add(&function_one);
	functions->add(&function_two);
	result = result && functions->find(function_one.funcid) == &function_one &&
		functions->find(function_two.funcid) == &function_two;

	/* A different object for the same function cannot remove the registration. */
	functions->remove(&other);
	result = result && functions->find(function_one.funcid) == &function_one;
	functions->remove(&function_two);
	result = result && functions->find(function_two.funcid) == NULL &&
		functions->find(function_one.funcid) == &function_one;
	functions->remove(&function_one);
	functions->remove(&function_one);
	result = result && functions->find(function_one.funcid) == NULL;

	/* The function can be described again by another object. */
	functions->add(&other);
	functions->remove(&function_one);
	result = result && functions->find(other.funcid) == &other;
	functions->remove(&other);
	result = result && functions->find(other.funcid) == NULL;

	PG_RETURN_BOOL(result);
}

Datum
tessera_test_function_sizes(PG_FUNCTION_ARGS)
{
	const TessFunctionRegistryOps *functions = get_api()->functions;
	TessFunction short_function = function_one;
	ExtendedFunction extended = {
		.base = function_two,
	};
	bool		result;

	short_function.struct_size = TESS_FUNCTION_MIN_SIZE;
	extended.base.struct_size = sizeof(extended);
	functions->add(&short_function);
	functions->add(&extended.base);
	result = functions->find(short_function.funcid) == &short_function &&
		functions->find(extended.base.funcid) == &extended.base &&
		sizeof(TessFunctionCall) >= TESS_FUNCTION_CALL_MIN_SIZE &&
		sizeof(TessFunctionArg) >= TESS_FUNCTION_ARG_MIN_SIZE;
	functions->remove(&short_function);
	functions->remove(&extended.base);

	PG_RETURN_BOOL(result);
}

Datum
tessera_test_invalid_function(PG_FUNCTION_ARGS)
{
	const TessFunctionRegistryOps *functions = get_api()->functions;
	TessFunction invalid = function_one;
	int32		kind = PG_GETARG_INT32(0);

	if (kind == 0)
		functions->add(NULL);
	else if (kind == 1)
		invalid.abi_version = TESS_FUNCTION_ABI_VERSION + 1;
	else if (kind == 2)
		invalid.struct_size = TESS_FUNCTION_MIN_SIZE - 1;
	else if (kind == 3)
		invalid.funcid = InvalidOid;
	else if (kind == 4)
		invalid.evaluate = NULL;
	else if (kind == 5)
		invalid.kind = (TessFunctionKind) 7;
	else if (kind == 6)
		invalid.result_format = (TessResultFormat) 7;
	else
		invalid.flags = TESS_FUNCTION_ANY_SHAPE;
	functions->add(&invalid);
	functions->remove(&invalid);
	elog(ERROR, "Tessera test registered an invalid function");
	PG_RETURN_VOID();
}

Datum
tessera_test_duplicate_function(PG_FUNCTION_ARGS)
{
	const TessFunctionRegistryOps *functions = get_api()->functions;
	TessFunction duplicate = function_one;

	functions->add(&function_one);
	PG_TRY();
	{
		functions->add(&duplicate);
	}
	PG_FINALLY();
	{
		functions->remove(&function_one);
	}
	PG_END_TRY();
	elog(ERROR, "Tessera test registered a duplicate function");
	PG_RETURN_VOID();
}

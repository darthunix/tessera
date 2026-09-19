#include "postgres.h"

#include <string.h>

#include "catalog/namespace.h"
#include "catalog/pg_type_d.h"
#include "fmgr.h"
#include "nodes/value.h"
#include "utils/fmgroids.h"
#include "utils/lsyscache.h"

#include "tessera/bridge.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_kernels_module_registry);
PG_FUNCTION_INFO_V1(tessera_test_kernels_module_arithmetic);
PG_FUNCTION_INFO_V1(tessera_test_kernels_module_predicate);
PG_FUNCTION_INFO_V1(tessera_test_kernels_module_errors);

static const TessFunctionRegistryOps *
registry(void)
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
	return api->functions;
}

/* A three-row column: first, second, third; the second may be NULL. */
typedef struct Column
{
	Datum		values[3];
	bool		isnull[3];
	TessDatumColumn column;
} Column;

static void
init_column(Column *c, int32 first, int32 second, int32 third, bool second_null)
{
	c->values[0] = Int32GetDatum(first);
	c->values[1] = Int32GetDatum(second);
	c->values[2] = Int32GetDatum(third);
	c->isnull[0] = false;
	c->isnull[1] = second_null;
	c->isnull[2] = false;
	c->column.struct_size = sizeof(TessDatumColumn);
	c->column.values = c->values;
	c->column.isnull = c->isnull;
	c->column.nrows = 3;
}

static TessFunctionArg
column_arg(const Column *c)
{
	TessFunctionArg arg = TESS_STRUCT_INITIALIZER(TessFunctionArg);

	arg.column = &c->column;
	return arg;
}

static TessFunctionArg
scalar_arg(int32 value)
{
	TessFunctionArg arg = TESS_STRUCT_INITIALIZER(TessFunctionArg);

	arg.scalar = Int32GetDatum(value);
	return arg;
}

/* Evaluate a two-argument call over all three rows. */
static TessStatusCode
evaluate(const TessFunction *function, TessFunctionArg left,
		 TessFunctionArg right, uint64 *selection, int32 *values,
		 uint64 *non_null_word, TessStatus *status)
{
	TessFunctionArg args[2] = {left, right};
	TessRowMask rows = {3, selection};
	TessRowMask non_nulls = {3, non_null_word};
	TessFunctionCall call = TESS_STRUCT_INITIALIZER(TessFunctionCall);

	call.function = function;
	call.nargs = 2;
	call.args = args;
	call.inputcollid = InvalidOid;
	call.rows = &rows;
	call.values = values;
	call.non_nulls = &non_nulls;
	call.context = CurrentMemoryContext;
	call.status = status;
	return function->evaluate(&call);
}

Datum
tessera_test_kernels_module_registry(PG_FUNCTION_ARGS)
{
	const TessFunctionRegistryOps *functions = registry();
	const Oid	predicates[] = {F_INT4EQ, F_INT4NE, F_INT4LT, F_INT4LE,
	F_INT4GT, F_INT4GE};
	const Oid	values[] = {F_INT4PL, F_INT4MI, F_INT4MUL, F_INT4DIV, F_INT4MOD};
	int			i;

	for (i = 0; i < lengthof(predicates); i++)
	{
		const TessFunction *function = functions->find(predicates[i]);

		if (function == NULL || function->funcid != predicates[i] ||
			function->kind != TESS_FUNCTION_PREDICATE ||
			(function->flags & TESS_FUNCTION_STRICT) == 0 ||
			(function->flags & TESS_FUNCTION_ANY_SHAPE) != 0 ||
			function->evaluate == NULL)
			PG_RETURN_BOOL(false);
	}
	for (i = 0; i < lengthof(values); i++)
	{
		const TessFunction *function = functions->find(values[i]);

		if (function == NULL || function->funcid != values[i] ||
			function->kind != TESS_FUNCTION_VALUE ||
			function->result_format != TESS_RESULT_INT32 ||
			(function->flags & TESS_FUNCTION_ANY_SHAPE) == 0 ||
			function->evaluate == NULL)
			PG_RETURN_BOOL(false);
	}
	PG_RETURN_BOOL(functions->find(F_INT4UM) == NULL &&
				   functions->find(F_INT8PL) == NULL);
}

Datum
tessera_test_kernels_module_arithmetic(PG_FUNCTION_ARGS)
{
	const TessFunctionRegistryOps *functions = registry();
	Column		c;
	uint64		selection = 7;
	uint64		non_nulls = 0;
	int32		values[3];
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	init_column(&c, 10, 20, 30, true);
	/* x + 7 */
	if (evaluate(functions->find(F_INT4PL), column_arg(&c), scalar_arg(7),
				 &selection, values, &non_nulls, &status) != TESS_OK ||
		non_nulls != 5 || values[0] != 17 || values[2] != 37)
		PG_RETURN_BOOL(false);
	/* 100 - x */
	if (evaluate(functions->find(F_INT4MI), scalar_arg(100), column_arg(&c),
				 &selection, values, &non_nulls, &status) != TESS_OK ||
		non_nulls != 5 || values[0] != 90 || values[2] != 70)
		PG_RETURN_BOOL(false);
	/* x * x */
	if (evaluate(functions->find(F_INT4MUL), column_arg(&c), column_arg(&c),
				 &selection, values, &non_nulls, &status) != TESS_OK ||
		non_nulls != 5 || values[0] != 100 || values[2] != 900)
		PG_RETURN_BOOL(false);
	/* x % 7 with the selection narrowed to the last row */
	selection = 4;
	if (evaluate(functions->find(F_INT4MOD), column_arg(&c), scalar_arg(7),
				 &selection, values, &non_nulls, &status) != TESS_OK ||
		non_nulls != 4 || values[2] != 2)
		PG_RETURN_BOOL(false);
	PG_RETURN_BOOL(true);
}

Datum
tessera_test_kernels_module_predicate(PG_FUNCTION_ARGS)
{
	const TessFunctionRegistryOps *functions = registry();
	Column		c;
	uint64		selection = 7;
	uint64		non_nulls = 0;
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	Oid			less;
	Oid			greater;

	init_column(&c, 10, 20, 30, true);
	/* x < 25 keeps the first row; the NULL row never matches. */
	if (evaluate(functions->find(F_INT4LT), column_arg(&c), scalar_arg(25),
				 &selection, NULL, &non_nulls, &status) != TESS_OK ||
		selection != 1)
		PG_RETURN_BOOL(false);
	/* 25 < x is x > 25 through the catalog's commutator. */
	less = OpernameGetOprid(list_make1(makeString("<")), INT4OID, INT4OID);
	greater = get_commutator(less);
	if (!OidIsValid(less) || !OidIsValid(greater) ||
		functions->find(get_opcode(less)) != functions->find(F_INT4LT) ||
		functions->find(get_opcode(greater)) != functions->find(F_INT4GT))
		PG_RETURN_BOOL(false);
	selection = 7;
	if (evaluate(functions->find(get_opcode(greater)), column_arg(&c),
				 scalar_arg(25), &selection, NULL, &non_nulls,
				 &status) != TESS_OK || selection != 4)
		PG_RETURN_BOOL(false);
	PG_RETURN_BOOL(true);
}

Datum
tessera_test_kernels_module_errors(PG_FUNCTION_ARGS)
{
	const TessFunctionRegistryOps *functions = registry();
	Column		c;
	uint64		selection = 7;
	uint64		non_nulls = 0;
	int32		values[3];
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	init_column(&c, 10, 20, 30, false);
	/* x / 0 */
	if (evaluate(functions->find(F_INT4DIV), column_arg(&c), scalar_arg(0),
				 &selection, values, &non_nulls,
				 &status) != TESS_ERROR_DIVISION_BY_ZERO ||
		status.code != TESS_ERROR_DIVISION_BY_ZERO ||
		strcmp(status.sqlstate, "22012") != 0)
		PG_RETURN_BOOL(false);
	/* A comparison of two columns is not supported. */
	if (evaluate(functions->find(F_INT4EQ), column_arg(&c), column_arg(&c),
				 &selection, NULL, &non_nulls,
				 &status) != TESS_ERROR_INVALID_ARGUMENT ||
		status.code != TESS_ERROR_INVALID_ARGUMENT ||
		strstr(status.message, "scalar") == NULL)
		PG_RETURN_BOOL(false);
	/* Two scalars are folded by the consumer, not evaluated. */
	if (evaluate(functions->find(F_INT4PL), scalar_arg(1), scalar_arg(2),
				 &selection, values, &non_nulls,
				 &status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);
	PG_RETURN_BOOL(true);
}

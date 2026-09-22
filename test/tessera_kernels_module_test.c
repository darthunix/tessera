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
PG_FUNCTION_INFO_V1(tessera_test_kernels_module_aggregates);
PG_FUNCTION_INFO_V1(tessera_test_kernels_module_int8);

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

/* The same three rows as int8 Datums. */
static void
init_column8(Column *c, int64 first, int64 second, int64 third, bool second_null)
{
	init_column(c, 0, 0, 0, second_null);
	c->values[0] = Int64GetDatum(first);
	c->values[1] = Int64GetDatum(second);
	c->values[2] = Int64GetDatum(third);
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

static TessFunctionArg
scalar_arg8(int64 value)
{
	TessFunctionArg arg = TESS_STRUCT_INITIALIZER(TessFunctionArg);

	arg.scalar = Int64GetDatum(value);
	return arg;
}

/* Evaluate a call of one or two arguments over all three rows. */
static TessStatusCode
evaluate_args(const TessFunction *function, TessFunctionArg *args, int nargs,
			  uint64 *selection, void *values, uint64 *non_null_word,
			  TessStatus *status)
{
	TessRowMask rows = {3, selection};
	TessRowMask non_nulls = {3, non_null_word};
	TessFunctionCall call = TESS_STRUCT_INITIALIZER(TessFunctionCall);

	call.function = function;
	call.nargs = nargs;
	call.args = args;
	call.inputcollid = InvalidOid;
	call.rows = &rows;
	call.values = values;
	call.non_nulls = &non_nulls;
	call.context = CurrentMemoryContext;
	call.status = status;
	return function->evaluate(&call);
}

static TessStatusCode
evaluate(const TessFunction *function, TessFunctionArg left,
		 TessFunctionArg right, uint64 *selection, void *values,
		 uint64 *non_null_word, TessStatus *status)
{
	TessFunctionArg args[2] = {left, right};

	return evaluate_args(function, args, 2, selection, values, non_null_word,
						 status);
}

static TessStatusCode
evaluate_one(const TessFunction *function, TessFunctionArg arg,
			 uint64 *selection, void *values, uint64 *non_null_word,
			 TessStatus *status)
{
	return evaluate_args(function, &arg, 1, selection, values, non_null_word,
						 status);
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
	{
		/* Unary minus takes the column as its only argument. */
		const TessFunction *negate = functions->find(F_INT4UM);

		if (negate == NULL || negate->funcid != F_INT4UM ||
			negate->kind != TESS_FUNCTION_VALUE ||
			negate->result_format != TESS_RESULT_INT32 ||
			(negate->flags & TESS_FUNCTION_STRICT) == 0 ||
			(negate->flags & TESS_FUNCTION_ANY_SHAPE) != 0)
			PG_RETURN_BOOL(false);
	}
	/* A function no module registers. */
	PG_RETURN_BOOL(functions->find(F_FLOAT8PL) == NULL);
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
	/* -x */
	if (evaluate_one(functions->find(F_INT4UM), column_arg(&c),
					 &selection, values, &non_nulls, &status) != TESS_OK ||
		non_nulls != 5 || values[0] != -10 || values[2] != -30)
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

/* A partial aggregate over the selected rows of the three-row column. */
static TessStatusCode
aggregate(const TessFunction *function, const TessFunctionArg *arg, int nargs,
		  uint64 selection, Datum *result, bool *isnull, TessRowMask *present,
		  TessStatus *status)
{
	TessRowMask rows = {3, &selection};
	TessFunctionCall call = TESS_STRUCT_INITIALIZER(TessFunctionCall);
	TessStatusCode code;

	*present->bits = 0;
	call.function = function;
	call.nargs = nargs;
	call.args = arg;
	call.inputcollid = InvalidOid;
	call.rows = &rows;
	call.values = result;
	call.non_nulls = present;
	call.context = CurrentMemoryContext;
	call.status = status;
	code = function->evaluate(&call);
	*isnull = (*present->bits & 1) == 0;
	return code;
}

Datum
tessera_test_kernels_module_aggregates(PG_FUNCTION_ARGS)
{
	const TessFunctionRegistryOps *functions = registry();
	const Oid	aggregates[] = {F_COUNT_, F_COUNT_ANY, F_SUM_INT4, F_MIN_INT4,
	F_MAX_INT4};
	Column		c;
	TessFunctionArg arg;
	uint64		word;
	TessRowMask present = {1, &word};
	TessRowMask wide = {3, &word};
	Datum		result;
	bool		isnull;
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	int			i;

	for (i = 0; i < lengthof(aggregates); i++)
	{
		const TessFunction *function = functions->find(aggregates[i]);

		if (function == NULL || function->funcid != aggregates[i] ||
			function->kind != TESS_FUNCTION_AGGREGATE ||
			function->result_format != TESS_RESULT_DATUM ||
			(function->flags & TESS_FUNCTION_STRICT) == 0 ||
			function->evaluate == NULL)
			PG_RETURN_BOOL(false);
	}
	init_column(&c, 10, 20, 30, true);
	arg = column_arg(&c);
	/* All three rows: count(*) counts the NULL row, the others skip it. */
	if (aggregate(functions->find(F_COUNT_), NULL, 0, 7, &result, &isnull,
				  &present, &status) != TESS_OK ||
		isnull || DatumGetInt64(result) != 3 ||
		aggregate(functions->find(F_COUNT_ANY), &arg, 1, 7, &result, &isnull,
				  &present, &status) != TESS_OK ||
		isnull || DatumGetInt64(result) != 2 ||
		aggregate(functions->find(F_SUM_INT4), &arg, 1, 7, &result, &isnull,
				  &present, &status) != TESS_OK ||
		isnull || DatumGetInt64(result) != 40 ||
		aggregate(functions->find(F_MIN_INT4), &arg, 1, 7, &result, &isnull,
				  &present, &status) != TESS_OK ||
		isnull || DatumGetInt32(result) != 10 ||
		aggregate(functions->find(F_MAX_INT4), &arg, 1, 7, &result, &isnull,
				  &present, &status) != TESS_OK ||
		isnull || DatumGetInt32(result) != 30)
		PG_RETURN_BOOL(false);
	/* Only the NULL row selected: counts are 0, the rest NULL. */
	if (aggregate(functions->find(F_COUNT_), NULL, 0, 2, &result, &isnull,
				  &present, &status) != TESS_OK ||
		isnull || DatumGetInt64(result) != 1 ||
		aggregate(functions->find(F_COUNT_ANY), &arg, 1, 2, &result, &isnull,
				  &present, &status) != TESS_OK ||
		isnull || DatumGetInt64(result) != 0 ||
		aggregate(functions->find(F_SUM_INT4), &arg, 1, 2, &result, &isnull,
				  &present, &status) != TESS_OK || !isnull ||
		aggregate(functions->find(F_MIN_INT4), &arg, 1, 2, &result, &isnull,
				  &present, &status) != TESS_OK || !isnull ||
		aggregate(functions->find(F_MAX_INT4), &arg, 1, 2, &result, &isnull,
				  &present, &status) != TESS_OK || !isnull)
		PG_RETURN_BOOL(false);
	/* Nothing selected. */
	if (aggregate(functions->find(F_COUNT_), NULL, 0, 0, &result, &isnull,
				  &present, &status) != TESS_OK ||
		isnull || DatumGetInt64(result) != 0 ||
		aggregate(functions->find(F_SUM_INT4), &arg, 1, 0, &result, &isnull,
				  &present, &status) != TESS_OK || !isnull)
		PG_RETURN_BOOL(false);
	/* Wrong shapes: an argument to count(*), none to sum, a wide mask. */
	if (aggregate(functions->find(F_COUNT_), &arg, 1, 7, &result, &isnull,
				  &present, &status) != TESS_ERROR_INVALID_ARGUMENT ||
		aggregate(functions->find(F_SUM_INT4), NULL, 0, 7, &result, &isnull,
				  &present, &status) != TESS_ERROR_INVALID_ARGUMENT ||
		aggregate(functions->find(F_SUM_INT4), &arg, 1, 7, &result, &isnull,
				  &wide, &status) != TESS_ERROR_INVALID_ARGUMENT)
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
	/* Negating the smallest value overflows, as int4um does. */
	init_column(&c, PG_INT32_MIN, 20, 30, false);
	if (evaluate_one(functions->find(F_INT4UM), column_arg(&c),
					 &selection, values, &non_nulls,
					 &status) != TESS_ERROR_INTEGER_OUT_OF_RANGE ||
		strcmp(status.sqlstate, "22003") != 0)
		PG_RETURN_BOOL(false);
	/* Negation takes one argument, and it must be the column. */
	if (evaluate(functions->find(F_INT4UM), column_arg(&c), scalar_arg(1),
				 &selection, values, &non_nulls,
				 &status) != TESS_ERROR_INVALID_ARGUMENT ||
		evaluate_one(functions->find(F_INT4UM), scalar_arg(1),
					 &selection, values, &non_nulls,
					 &status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);
	PG_RETURN_BOOL(true);
}

/* The int8 family: registrations, the mixed operators and the cast. */
Datum
tessera_test_kernels_module_int8(PG_FUNCTION_ARGS)
{
	const TessFunctionRegistryOps *functions = registry();
	const Oid	predicates[] = {F_INT8EQ, F_INT8NE, F_INT8LT, F_INT8LE, F_INT8GT,
	F_INT8GE, F_INT84EQ, F_INT84NE, F_INT84LT, F_INT84LE, F_INT84GT, F_INT84GE};
	const Oid	values[] = {F_INT8PL, F_INT8MI, F_INT8MUL, F_INT8DIV, F_INT8MOD};
	const Oid	mixed[] = {F_INT84PL, F_INT84MI, F_INT84MUL, F_INT84DIV,
	F_INT8UM, F_INT8_INT4};
	const Oid	aggregates[] = {F_MIN_INT8, F_MAX_INT8};
	const int64 big = ((int64) 1) << 40;
	Column		c;
	TessFunctionArg arg;
	uint64		selection = 7;
	uint64		non_nulls = 0;
	uint64		word;
	TessRowMask present = {1, &word};
	int64		results[3];
	Datum		datums[3];
	Datum		result;
	bool		isnull;
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	Oid			less;
	Oid			greater;
	int			i;

	for (i = 0; i < lengthof(predicates); i++)
	{
		const TessFunction *function = functions->find(predicates[i]);

		if (function == NULL || function->kind != TESS_FUNCTION_PREDICATE ||
			(function->flags & TESS_FUNCTION_STRICT) == 0 ||
			(function->flags & TESS_FUNCTION_ANY_SHAPE) != 0)
			PG_RETURN_BOOL(false);
	}
	for (i = 0; i < lengthof(values); i++)
	{
		const TessFunction *function = functions->find(values[i]);

		if (function == NULL || function->kind != TESS_FUNCTION_VALUE ||
			function->result_format != TESS_RESULT_DATUM ||
			(function->flags & TESS_FUNCTION_ANY_SHAPE) == 0)
			PG_RETURN_BOOL(false);
	}
	/* The mixed operators, the negation and the cast keep the column first. */
	for (i = 0; i < lengthof(mixed); i++)
	{
		const TessFunction *function = functions->find(mixed[i]);

		if (function == NULL || function->kind != TESS_FUNCTION_VALUE ||
			function->result_format != TESS_RESULT_DATUM ||
			(function->flags & TESS_FUNCTION_ANY_SHAPE) != 0)
			PG_RETURN_BOOL(false);
	}
	for (i = 0; i < lengthof(aggregates); i++)
	{
		const TessFunction *function = functions->find(aggregates[i]);

		if (function == NULL || function->kind != TESS_FUNCTION_AGGREGATE ||
			function->result_format != TESS_RESULT_DATUM)
			PG_RETURN_BOOL(false);
	}
	/* The mixed arithmetic with the integer on the left is left to the core. */
	if (functions->find(F_INT48PL) != NULL || functions->find(F_INT48MUL) != NULL)
		PG_RETURN_BOOL(false);
	/* An integer column against a bigint scalar: within the int4 range as
	 * int4, beyond it constant for every non-NULL value. */
	init_column(&c, 10, 20, 30, true);
	selection = 7;
	if (evaluate(functions->find(F_INT48LT), column_arg(&c), scalar_arg8(25),
				 &selection, NULL, &non_nulls, &status) != TESS_OK ||
		selection != 1)
		PG_RETURN_BOOL(false);
	selection = 7;
	if (evaluate(functions->find(F_INT48LT), column_arg(&c),
				 scalar_arg8(((int64) 5) << 32), &selection, NULL, &non_nulls,
				 &status) != TESS_OK || selection != 5)
		PG_RETURN_BOOL(false);
	selection = 7;
	if (evaluate(functions->find(F_INT48GE), column_arg(&c),
				 scalar_arg8(((int64) 5) << 32), &selection, NULL, &non_nulls,
				 &status) != TESS_OK || selection != 0)
		PG_RETURN_BOOL(false);
	selection = 7;
	if (evaluate(functions->find(F_INT48NE), column_arg(&c),
				 scalar_arg8(-(((int64) 5) << 32)), &selection, NULL, &non_nulls,
				 &status) != TESS_OK || selection != 5)
		PG_RETURN_BOOL(false);
	selection = 7;
	if (evaluate(functions->find(F_INT48EQ), column_arg(&c),
				 scalar_arg8(-(((int64) 5) << 32)), &selection, NULL, &non_nulls,
				 &status) != TESS_OK || selection != 0)
		PG_RETURN_BOOL(false);
	selection = 7;

	/* x + 2^40 over int8 values past the int4 range, x * x, 100 * 2^33 - x. */
	init_column8(&c, 10 * big, 20 * big, 30 * big, true);
	if (evaluate(functions->find(F_INT8PL), column_arg(&c), scalar_arg8(big),
				 &selection, results, &non_nulls, &status) != TESS_OK ||
		non_nulls != 5 || results[0] != 11 * big || results[2] != 31 * big)
		PG_RETURN_BOOL(false);
	if (evaluate(functions->find(F_INT8MI), scalar_arg8(100 * big),
				 column_arg(&c), &selection, results, &non_nulls,
				 &status) != TESS_OK ||
		non_nulls != 5 || results[0] != 90 * big || results[2] != 70 * big)
		PG_RETURN_BOOL(false);
	/* x * x over values whose squares fit. */
	init_column8(&c, 10 << 20, 20 << 20, 30 << 20, true);
	if (evaluate(functions->find(F_INT8MUL), column_arg(&c), column_arg(&c),
				 &selection, results, &non_nulls, &status) != TESS_OK ||
		non_nulls != 5 || results[0] != (((int64) 100) << 40) ||
		results[2] != (((int64) 900) << 40))
		PG_RETURN_BOOL(false);
	/* Mixed: x / 7 with an integer scalar, and the column must come first. */
	init_column8(&c, 10 * big, 20, 30 * big + 3, true);
	if (evaluate(functions->find(F_INT84DIV), column_arg(&c), scalar_arg(7),
				 &selection, results, &non_nulls, &status) != TESS_OK ||
		non_nulls != 5 || results[0] != (10 * big) / 7 ||
		results[2] != (30 * big + 3) / 7 ||
		evaluate(functions->find(F_INT84PL), scalar_arg(7), column_arg(&c),
				 &selection, results, &non_nulls,
				 &status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);
	/* -x */
	if (evaluate_one(functions->find(F_INT8UM), column_arg(&c),
					 &selection, results, &non_nulls, &status) != TESS_OK ||
		non_nulls != 5 || results[0] != -10 * big || results[2] != -30 * big - 3)
		PG_RETURN_BOOL(false);
	/* Predicates: x < 25 * 2^40 as int8, and 25 < x through the commutator
	 * of the mixed operator with the integer on the left. */
	init_column8(&c, 10 * big, 20 * big, 30 * big, true);
	selection = 7;
	if (evaluate(functions->find(F_INT8LT), column_arg(&c), scalar_arg8(25 * big),
				 &selection, NULL, &non_nulls, &status) != TESS_OK ||
		selection != 1)
		PG_RETURN_BOOL(false);
	less = OpernameGetOprid(list_make1(makeString("<")), INT4OID, INT8OID);
	greater = get_commutator(less);
	if (!OidIsValid(less) || !OidIsValid(greater) ||
		functions->find(get_opcode(less)) != functions->find(F_INT48LT) ||
		functions->find(get_opcode(greater)) != functions->find(F_INT84GT))
		PG_RETURN_BOOL(false);
	/* 25 < x keeps both non-NULL rows; the NULL row never matches. */
	selection = 7;
	if (evaluate(functions->find(get_opcode(greater)), column_arg(&c),
				 scalar_arg(25), &selection, NULL, &non_nulls,
				 &status) != TESS_OK || selection != 5)
		PG_RETURN_BOOL(false);
	selection = 7;
	if (evaluate(functions->find(F_INT84LT), column_arg(&c), scalar_arg(25),
				 &selection, NULL, &non_nulls, &status) != TESS_OK ||
		selection != 0)
		PG_RETURN_BOOL(false);
	/* The cast: int4 Datums widened into int8 Datums. */
	init_column(&c, PG_INT32_MIN, 20, PG_INT32_MAX, true);
	selection = 7;
	if (evaluate_one(functions->find(F_INT8_INT4), column_arg(&c),
					 &selection, datums, &non_nulls, &status) != TESS_OK ||
		non_nulls != 5 || DatumGetInt64(datums[0]) != PG_INT32_MIN ||
		DatumGetInt64(datums[2]) != PG_INT32_MAX)
		PG_RETURN_BOOL(false);
	/* Aggregates over int8, and the count over them. */
	init_column8(&c, 10 * big, 20 * big, 30 * big, true);
	arg = column_arg(&c);
	if (aggregate(functions->find(F_MIN_INT8), &arg, 1, 7, &result, &isnull,
				  &present, &status) != TESS_OK ||
		isnull || DatumGetInt64(result) != 10 * big ||
		aggregate(functions->find(F_MAX_INT8), &arg, 1, 7, &result, &isnull,
				  &present, &status) != TESS_OK ||
		isnull || DatumGetInt64(result) != 30 * big ||
		aggregate(functions->find(F_COUNT_ANY), &arg, 1, 7, &result, &isnull,
				  &present, &status) != TESS_OK ||
		isnull || DatumGetInt64(result) != 2 ||
		aggregate(functions->find(F_MIN_INT8), &arg, 1, 2, &result, &isnull,
				  &present, &status) != TESS_OK || !isnull)
		PG_RETURN_BOOL(false);
	/* Errors: bigint out of range, and a zero divisor. */
	init_column8(&c, PG_INT64_MIN, 20, 30, false);
	selection = 7;
	if (evaluate_one(functions->find(F_INT8UM), column_arg(&c),
					 &selection, results, &non_nulls,
					 &status) != TESS_ERROR_INTEGER_OUT_OF_RANGE ||
		strcmp(status.sqlstate, "22003") != 0 ||
		strcmp(status.message, "bigint out of range") != 0 ||
		evaluate(functions->find(F_INT84DIV), column_arg(&c), scalar_arg(0),
				 &selection, results, &non_nulls,
				 &status) != TESS_ERROR_DIVISION_BY_ZERO ||
		strcmp(status.sqlstate, "22012") != 0)
		PG_RETURN_BOOL(false);
	PG_RETURN_BOOL(true);
}

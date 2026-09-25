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
PG_FUNCTION_INFO_V1(tessera_test_kernels_module_table);

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

		/* Comparisons take a column and a scalar on either side, or two columns. */
		if (function == NULL || function->funcid != predicates[i] ||
			function->kind != TESS_FUNCTION_PREDICATE ||
			(function->flags & TESS_FUNCTION_STRICT) == 0 ||
			(function->flags & TESS_FUNCTION_ANY_SHAPE) == 0 ||
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
	/* A scalar on the left: 25 > x keeps the first row. */
	selection = 7;
	if (evaluate(functions->find(F_INT4GT), scalar_arg(25), column_arg(&c),
				 &selection, NULL, &non_nulls, &status) != TESS_OK ||
		selection != 1)
		PG_RETURN_BOOL(false);
	{
		/* Two columns: 10, NULL, 30 against 15, 20, 30. */
		Column		d;

		init_column(&d, 15, 20, 30, false);
		selection = 7;
		if (evaluate(functions->find(F_INT4LT), column_arg(&c), column_arg(&d),
					 &selection, NULL, &non_nulls, &status) != TESS_OK ||
			selection != 1)
			PG_RETURN_BOOL(false);
		selection = 7;
		if (evaluate(functions->find(F_INT4EQ), column_arg(&c), column_arg(&d),
					 &selection, NULL, &non_nulls, &status) != TESS_OK ||
			selection != 4)
			PG_RETURN_BOOL(false);
	}
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
	/* A comparison needs a column: two scalars are refused. */
	if (evaluate(functions->find(F_INT4EQ), scalar_arg(1), scalar_arg(1),
				 &selection, NULL, &non_nulls,
				 &status) != TESS_ERROR_INVALID_ARGUMENT ||
		status.code != TESS_ERROR_INVALID_ARGUMENT ||
		strstr(status.message, "column") == NULL)
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
	F_INT8GE};
	const Oid	values[] = {F_INT8PL, F_INT8MI, F_INT8MUL, F_INT8DIV, F_INT8MOD};
	const Oid	mixed[] = {F_INT8UM, F_INT8_INT4};
	/* Each function over an int4 and an int8: its int8 function and casts. */
	const Oid	equivalents[][4] = {
		{F_INT84EQ, F_INT8EQ, InvalidOid, F_INT8_INT4},
		{F_INT84NE, F_INT8NE, InvalidOid, F_INT8_INT4},
		{F_INT84LT, F_INT8LT, InvalidOid, F_INT8_INT4},
		{F_INT84LE, F_INT8LE, InvalidOid, F_INT8_INT4},
		{F_INT84GT, F_INT8GT, InvalidOid, F_INT8_INT4},
		{F_INT84GE, F_INT8GE, InvalidOid, F_INT8_INT4},
		{F_INT48EQ, F_INT8EQ, F_INT8_INT4, InvalidOid},
		{F_INT48NE, F_INT8NE, F_INT8_INT4, InvalidOid},
		{F_INT48LT, F_INT8LT, F_INT8_INT4, InvalidOid},
		{F_INT48LE, F_INT8LE, F_INT8_INT4, InvalidOid},
		{F_INT48GT, F_INT8GT, F_INT8_INT4, InvalidOid},
		{F_INT48GE, F_INT8GE, F_INT8_INT4, InvalidOid},
		{F_INT84PL, F_INT8PL, InvalidOid, F_INT8_INT4},
		{F_INT84MI, F_INT8MI, InvalidOid, F_INT8_INT4},
		{F_INT84MUL, F_INT8MUL, InvalidOid, F_INT8_INT4},
		{F_INT84DIV, F_INT8DIV, InvalidOid, F_INT8_INT4},
		{F_INT48PL, F_INT8PL, F_INT8_INT4, InvalidOid},
		{F_INT48MI, F_INT8MI, F_INT8_INT4, InvalidOid},
		{F_INT48MUL, F_INT8MUL, F_INT8_INT4, InvalidOid},
		{F_INT48DIV, F_INT8DIV, F_INT8_INT4, InvalidOid}};
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
			(function->flags & TESS_FUNCTION_ANY_SHAPE) == 0)
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
	/* The negation and the cast keep the column first. */
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
	/* The functions over an int4 and an int8 are equivalents, no callback. */
	for (i = 0; i < lengthof(equivalents); i++)
	{
		const TessFunction *function = functions->find(equivalents[i][0]);

		if (function == NULL || function->kind != TESS_FUNCTION_EQUIVALENT ||
			function->struct_size < TESS_FUNCTION_EQUIVALENT_MIN_SIZE ||
			function->evaluate != NULL ||
			function->equivalent != equivalents[i][1] ||
			function->arg_casts[0] != equivalents[i][2] ||
			function->arg_casts[1] != equivalents[i][3])
			PG_RETURN_BOOL(false);
	}
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
	/* -x */
	init_column8(&c, 10 * big, 20, 30 * big + 3, true);
	if (evaluate_one(functions->find(F_INT8UM), column_arg(&c),
					 &selection, results, &non_nulls, &status) != TESS_OK ||
		non_nulls != 5 || results[0] != -10 * big || results[2] != -30 * big - 3)
		PG_RETURN_BOOL(false);
	/* Predicates: x < 25 * 2^40 as int8; the mixed operator with the
	 * integer on the left commutes into another equivalent. */
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
		evaluate(functions->find(F_INT8DIV), column_arg(&c), scalar_arg8(0),
				 &selection, results, &non_nulls,
				 &status) != TESS_ERROR_DIVISION_BY_ZERO ||
		strcmp(status.sqlstate, "22012") != 0)
		PG_RETURN_BOOL(false);
	PG_RETURN_BOOL(true);
}

/* The kernel operations the module installed, checked as a node checks them. */
static const TessKernelOps *
kernel_ops(void)
{
	void	  **rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	const TessApi *api = *rendezvous;
	const TessKernelOps *ops;

	if (api == NULL || !TESS_ABI_HAS_FIELD(api, TessApi, kernels) ||
		api->kernels == NULL ||
		api->kernels->abi_version != TESS_KERNEL_REGISTRY_OPS_ABI_VERSION ||
		api->kernels->struct_size < TESS_KERNEL_REGISTRY_OPS_MIN_SIZE)
		elog(ERROR, "Tessera test could not find a compatible kernel registry");
	ops = api->kernels->get();
	if (ops == NULL || ops->abi_version != TESS_KERNEL_OPS_ABI_VERSION ||
		ops->struct_size < TESS_KERNEL_OPS_MIN_SIZE ||
		ops->table_format_version != TESS_TABLE_FORMAT_VERSION)
		elog(ERROR, "Tessera test found no compatible kernels");
	return ops;
}

#define TABLE_ROWS 64

/* A column of 64 keys, row % modulo, as int4 or int8 Datums. */
static void
init_keys(TessDatumColumn *column, Datum *values, bool *isnull, int modulo,
		  bool int8)
{
	int			row;

	for (row = 0; row < TABLE_ROWS; row++)
	{
		values[row] = int8 ? Int64GetDatum(row % modulo) :
			Int32GetDatum(row % modulo);
		isnull[row] = false;
	}
	column->struct_size = sizeof(TessDatumColumn);
	column->values = values;
	column->isnull = isnull;
	column->nrows = TABLE_ROWS;
}

/*
 * The whole life of a table through the installed operations alone, as a
 * node module that does not link the kernels would run it: build from
 * int8 keys in a region too small, grow it by repalloc, probe with int4
 * keys, walk the chains, gather the payload, scan, then group.
 */
/* Raise the step of the table cycle that failed, with the kernel's message. */
pg_noreturn static void
cycle_failed(const char *step, const TessStatus *status)
{
	elog(ERROR, "table cycle failed at %s: %s", step, status->message);
	pg_unreachable();
}

Datum
tessera_test_kernels_module_table(PG_FUNCTION_ARGS)
{
	const TessKernelOps *ops = kernel_ops();
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	TessTableKeyKind kind = TESS_TABLE_KEY_INT8;
	Datum		build_values[TABLE_ROWS];
	Datum		probe_values[TABLE_ROWS];
	Datum		group_values[TABLE_ROWS];
	bool		isnull[TABLE_ROWS];
	TessDatumColumn build_column;
	TessDatumColumn probe_column;
	TessDatumColumn group_column;
	TessTableKey key = {.kind = TESS_TABLE_KEY_INT8, .column = &build_column};
	uint32		hashes[TABLE_ROWS];
	uint32		offsets[TABLE_ROWS];
	uint32		matches[TABLE_ROWS];
	uint64		payload[TABLE_ROWS];
	Datum		gathered[TABLE_ROWS];
	uint64		all = ~UINT64CONST(0);
	uint64		valid_word = 0;
	uint64		pending_word;
	uint64		found_word = 0;
	uint64		inserted_word = 0;
	TessRowMask all_rows = {TABLE_ROWS, &all};
	TessRowMask valid = {TABLE_ROWS, &valid_word};
	TessRowMask pending = {TABLE_ROWS, &pending_word};
	TessRowMask found = {TABLE_ROWS, &found_word};
	TessRowMask inserted = {TABLE_ROWS, &inserted_word};
	TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
	TessTableRecord record = TESS_STRUCT_INITIALIZER(TessTableRecord);
	Size		small;
	Size		size;
	void	   *region;
	uint64		cursor = 0;
	int			count = 0;
	uint8	   *state;
	int			row;

	/* Build: 64 rows, keys 0..15 as int8, the row number as payload. */
	init_keys(&build_column, build_values, isnull, 16, true);
	for (row = 0; row < TABLE_ROWS; row++)
		payload[row] = row;
	if (ops->int8_hash(&build_column, NULL, &all_rows, TESS_NULL_KEYS_REJECT,
					   hashes, &valid, &status) != TESS_OK ||
		valid_word != all ||
		ops->table_size(1, &kind, 8, 16, &small, &status) != TESS_OK ||
		ops->table_size(1, &kind, 8, TABLE_ROWS + 16, &size, &status) != TESS_OK ||
		small >= size)
		cycle_failed("build: hash and size", &status);
	region = palloc0(small);
	pending_word = all;
	if (ops->table_create(region, small, 1, &kind, 8, 16, &status) != TESS_OK ||
		ops->table_insert(region, small, hashes, 1, &key, (uint8 *) payload,
						  &pending, offsets, &status) != TESS_OK ||
		pending_word == 0)
		cycle_failed("build: first insertion", &status);

	/*
	 * The rows left pending go in after growth, which also leaves room
	 * for the records grouping adds below; offsets stay valid.
	 */
	region = repalloc(region, size);
	memset((char *) region + small, 0, size - small);
	if (ops->table_grow(region, size, &status) != TESS_OK ||
		ops->table_insert(region, size, hashes, 1, &key, (uint8 *) payload,
						  &pending, offsets, &status) != TESS_OK ||
		pending_word != 0 ||
		ops->table_stats(region, size, &stats, &status) != TESS_OK ||
		stats.records != TABLE_ROWS || stats.region_len != size)
		cycle_failed("growth and second insertion", &status);

	/*
	 * Probe with the same keys as int4: an int8 in the int4 range hashes
	 * as the int4, so every row finds the newest record of its key, and
	 * the payload names a row with that key.
	 */
	init_keys(&probe_column, probe_values, isnull, 16, false);
	key.kind = TESS_TABLE_KEY_INT4;
	key.column = &probe_column;
	if (ops->int4_hash(&probe_column, NULL, &all_rows, TESS_NULL_KEYS_REJECT,
					   hashes, &valid, &status) != TESS_OK ||
		ops->table_probe(region, size, hashes, 1, &key, &valid, matches,
						 &found, &status) != TESS_OK ||
		found_word != all ||
		ops->table_gather(region, size, matches, &found, 0, gathered,
						  &status) != TESS_OK)
		cycle_failed("probe with int4 keys", &status);
	for (row = 0; row < TABLE_ROWS; row++)
	{
		if (DatumGetUInt64(gathered[row]) % 16 != row % 16 ||
			ops->table_record(region, size, matches[row], &record,
							  &status) != TESS_OK ||
			record.keys[0] != row % 16)
			cycle_failed("gathered payload or record", &status);
	}

	/* Four records per key: three more steps down every chain. */
	for (count = 0; count < 3; count++)
	{
		valid_word = found_word;
		if (ops->table_next_match(region, size, matches, &valid, &found,
								  &status) != TESS_OK ||
			found_word != all)
			cycle_failed("chain step", &status);
	}
	valid_word = found_word;
	if (ops->table_next_match(region, size, matches, &valid, &found,
							  &status) != TESS_OK ||
		found_word != 0)
		cycle_failed("chain end", &status);

	/* A scan visits every record once. */
	if (ops->table_scan(region, size, &cursor, offsets, TABLE_ROWS, &count,
						&status) != TESS_OK || count != TABLE_ROWS ||
		ops->table_scan(region, size, &cursor, offsets, TABLE_ROWS, &count,
						&status) != TESS_OK || count != 0)
		cycle_failed("scan", &status);

	/*
	 * Grouping, in row order: keys 0..19 find the records of 0..15, rows 16
	 * to 19 create one record each and rows 36 to 39 find those; a payload
	 * set in place is read back through another row's offset.
	 */
	init_keys(&group_column, group_values, isnull, 20, false);
	key.column = &group_column;
	pending_word = all;
	if (ops->int4_hash(&group_column, NULL, &all_rows, TESS_NULL_KEYS_GROUP,
					   hashes, &valid, &status) != TESS_OK ||
		ops->table_find_or_insert(region, size, hashes, 1, &key, &pending,
								  offsets, &inserted, &status) != TESS_OK ||
		pending_word != 0 || inserted_word != (UINT64CONST(0xf) << 16) ||
		offsets[37] != offsets[17])
		cycle_failed("grouping", &status);
	if (ops->table_payload(region, size, offsets[17], &state, &status) != TESS_OK)
		cycle_failed("payload in place", &status);
	memset(state, 0x5a, 8);
	if (ops->table_record(region, size, offsets[37], &record,
						  &status) != TESS_OK ||
		record.payload[7] != 0x5a ||
		ops->table_stats(region, size, &stats, &status) != TESS_OK ||
		stats.records != TABLE_ROWS + 4)
		cycle_failed("payload read back", &status);
	pfree(region);
	PG_RETURN_BOOL(true);
}

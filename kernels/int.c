/*
 * The built-in integer kernels as batch functions: this module links the
 * Rust kernels and registers them in the bridge's function registry when
 * loaded, for int4 and int8 alike, and installs the key hashes and the
 * hash table in the bridge's kernel registry (table.c). Load the bridge first (CREATE EXTENSION
 * tessera), then LOAD 'tessera_kernels'.
 *
 * An int8 is its Datum: the int8 kernels write int64 values straight into
 * a Datum column, so their results are TESS_RESULT_DATUM. The mixed
 * comparisons of bigint against an integer constant (int84eq and its
 * siblings) widen the int4 scalar and run the int8 kernel; the mixed
 * comparisons of an integer column against a bigint constant (int48eq and
 * its siblings, also what `100 < c8` commutes into) run the int4 kernel
 * against the constant clamped into the int4 range, since an int8 kernel
 * must not read an int4 Datum as a whole word; the mixed arithmetic with
 * the integer on the left is left to the core.
 */
#include "postgres.h"

#include "fmgr.h"
#include "utils/fmgroids.h"

#include "tessera/bridge.h"
#include "tessera/kernels.h"

#include "internal.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

/* A description with the kernel operation it maps to. */
typedef struct Function
{
	TessFunction function;
	uint32		op;
} Function;

static TessStatusCode compare_evaluate(TessFunctionCall *call);
static TessStatusCode arith_evaluate(TessFunctionCall *call);
static TessStatusCode negate_evaluate(TessFunctionCall *call);
static TessStatusCode aggregate_evaluate(TessFunctionCall *call);
static TessStatusCode compare8_evaluate(TessFunctionCall *call);
static TessStatusCode compare84_evaluate(TessFunctionCall *call);
static TessStatusCode compare48_evaluate(TessFunctionCall *call);
static TessStatusCode arith8_evaluate(TessFunctionCall *call);
static TessStatusCode arith84_evaluate(TessFunctionCall *call);
static TessStatusCode negate8_evaluate(TessFunctionCall *call);
static TessStatusCode cast_evaluate(TessFunctionCall *call);

/* The aggregate an AGGREGATE description computes. */
typedef enum Aggregate
{
	AGG_COUNT_ROWS,				/* count(*): the selected rows */
	AGG_COUNT,					/* count(x): the selected non-NULL values */
	AGG_SUM_INT4,
	AGG_MIN_INT4,
	AGG_MAX_INT4,
	AGG_MIN_INT8,
	AGG_MAX_INT8
} Aggregate;

/* A comparison of a column with a scalar; the consumer puts the column first. */
#define COMPARE(oid, code, fn) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_PREDICATE, \
	  .result_format = TESS_RESULT_DATUM, \
	  .flags = TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE, \
	  .evaluate = (fn)}, (code)}

/* A value function of the given result format and extra flags. */
#define VALUE(oid, code, format, extra, fn) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_VALUE, \
	  .result_format = (format), \
	  .flags = TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE | \
	  (extra), \
	  .evaluate = (fn)}, (code)}

/* A partial aggregate over one batch, a Datum of the transition type. */
#define AGGREGATE(oid, code) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_AGGREGATE, \
	  .result_format = TESS_RESULT_DATUM, \
	  .flags = TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE, \
	  .evaluate = aggregate_evaluate}, (code)}

static const Function functions[] = {
	/* int4 */
	COMPARE(F_INT4EQ, TESS_CMP_EQ, compare_evaluate),
	COMPARE(F_INT4NE, TESS_CMP_NE, compare_evaluate),
	COMPARE(F_INT4LT, TESS_CMP_LT, compare_evaluate),
	COMPARE(F_INT4LE, TESS_CMP_LE, compare_evaluate),
	COMPARE(F_INT4GT, TESS_CMP_GT, compare_evaluate),
	COMPARE(F_INT4GE, TESS_CMP_GE, compare_evaluate),
	VALUE(F_INT4PL, TESS_ARITH_ADD, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith_evaluate),
	VALUE(F_INT4MI, TESS_ARITH_SUB, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith_evaluate),
	VALUE(F_INT4MUL, TESS_ARITH_MUL, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith_evaluate),
	VALUE(F_INT4DIV, TESS_ARITH_DIV, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith_evaluate),
	VALUE(F_INT4MOD, TESS_ARITH_MOD, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith_evaluate),
	/* Unary minus: the one argument is the column. */
	VALUE(F_INT4UM, TESS_ARITH_SUB, TESS_RESULT_INT32, 0, negate_evaluate),
	AGGREGATE(F_COUNT_, AGG_COUNT_ROWS),
	AGGREGATE(F_COUNT_ANY, AGG_COUNT),
	AGGREGATE(F_SUM_INT4, AGG_SUM_INT4),
	AGGREGATE(F_MIN_INT4, AGG_MIN_INT4),
	AGGREGATE(F_MAX_INT4, AGG_MAX_INT4),
	/* int8 */
	COMPARE(F_INT8EQ, TESS_CMP_EQ, compare8_evaluate),
	COMPARE(F_INT8NE, TESS_CMP_NE, compare8_evaluate),
	COMPARE(F_INT8LT, TESS_CMP_LT, compare8_evaluate),
	COMPARE(F_INT8LE, TESS_CMP_LE, compare8_evaluate),
	COMPARE(F_INT8GT, TESS_CMP_GT, compare8_evaluate),
	COMPARE(F_INT8GE, TESS_CMP_GE, compare8_evaluate),
	/* bigint against an integer scalar */
	COMPARE(F_INT84EQ, TESS_CMP_EQ, compare84_evaluate),
	COMPARE(F_INT84NE, TESS_CMP_NE, compare84_evaluate),
	COMPARE(F_INT84LT, TESS_CMP_LT, compare84_evaluate),
	COMPARE(F_INT84LE, TESS_CMP_LE, compare84_evaluate),
	COMPARE(F_INT84GT, TESS_CMP_GT, compare84_evaluate),
	COMPARE(F_INT84GE, TESS_CMP_GE, compare84_evaluate),
	/* integer column against a bigint scalar */
	COMPARE(F_INT48EQ, TESS_CMP_EQ, compare48_evaluate),
	COMPARE(F_INT48NE, TESS_CMP_NE, compare48_evaluate),
	COMPARE(F_INT48LT, TESS_CMP_LT, compare48_evaluate),
	COMPARE(F_INT48LE, TESS_CMP_LE, compare48_evaluate),
	COMPARE(F_INT48GT, TESS_CMP_GT, compare48_evaluate),
	COMPARE(F_INT48GE, TESS_CMP_GE, compare48_evaluate),
	VALUE(F_INT8PL, TESS_ARITH_ADD, TESS_RESULT_DATUM, TESS_FUNCTION_ANY_SHAPE,
		  arith8_evaluate),
	VALUE(F_INT8MI, TESS_ARITH_SUB, TESS_RESULT_DATUM, TESS_FUNCTION_ANY_SHAPE,
		  arith8_evaluate),
	VALUE(F_INT8MUL, TESS_ARITH_MUL, TESS_RESULT_DATUM, TESS_FUNCTION_ANY_SHAPE,
		  arith8_evaluate),
	VALUE(F_INT8DIV, TESS_ARITH_DIV, TESS_RESULT_DATUM, TESS_FUNCTION_ANY_SHAPE,
		  arith8_evaluate),
	VALUE(F_INT8MOD, TESS_ARITH_MOD, TESS_RESULT_DATUM, TESS_FUNCTION_ANY_SHAPE,
		  arith8_evaluate),
	/* bigint column with an integer scalar: the column stays first; there
	 * is no int84mod, the core casts the operand instead */
	VALUE(F_INT84PL, TESS_ARITH_ADD, TESS_RESULT_DATUM, 0, arith84_evaluate),
	VALUE(F_INT84MI, TESS_ARITH_SUB, TESS_RESULT_DATUM, 0, arith84_evaluate),
	VALUE(F_INT84MUL, TESS_ARITH_MUL, TESS_RESULT_DATUM, 0, arith84_evaluate),
	VALUE(F_INT84DIV, TESS_ARITH_DIV, TESS_RESULT_DATUM, 0, arith84_evaluate),
	VALUE(F_INT8UM, TESS_ARITH_SUB, TESS_RESULT_DATUM, 0, negate8_evaluate),
	/* int8(int4): the cast, a column of int8 Datums */
	VALUE(F_INT8_INT4, 0, TESS_RESULT_DATUM, 0, cast_evaluate),
	AGGREGATE(F_MIN_INT8, AGG_MIN_INT8),
	AGGREGATE(F_MAX_INT8, AGG_MAX_INT8),
};

/* The operation of the description a call names. */
static uint32
operation(const TessFunctionCall *call)
{
	const Function *entry = (const Function *)
		((const char *) call->function - offsetof(Function, function));

	return entry->op;
}

/* Fail a call with an invalid argument before any kernel runs. */
static TessStatusCode
invalid(TessFunctionCall *call, const char *message)
{
	if (call->status != NULL && call->status->struct_size >= TESS_STATUS_MIN_SIZE)
	{
		call->status->code = TESS_ERROR_INVALID_ARGUMENT;
		strlcpy(call->status->sqlstate, "XX000", sizeof(call->status->sqlstate));
		strlcpy(call->status->message, message, sizeof(call->status->message));
	}
	return TESS_ERROR_INVALID_ARGUMENT;
}

static bool
valid_call(const TessFunctionCall *call)
{
	return call != NULL && call->struct_size >= TESS_FUNCTION_CALL_MIN_SIZE &&
		call->nargs == 2 && call->args != NULL &&
		call->args[0].struct_size >= TESS_FUNCTION_ARG_MIN_SIZE &&
		call->args[1].struct_size >= TESS_FUNCTION_ARG_MIN_SIZE;
}

/* column op scalar: the consumer puts the column first. */
static TessStatusCode
compare_evaluate(TessFunctionCall *call)
{
	if (!valid_call(call))
		return invalid(call, "an int4 comparison takes two arguments");
	if (call->args[0].column == NULL || call->args[1].column != NULL)
		return invalid(call, "an int4 comparison takes a column and a scalar");
	return tess_int4_filter(call->args[0].column, call->args[0].prepared,
							call->rows, (TessCompareOp) operation(call),
							DatumGetInt32(call->args[1].scalar), call->status);
}

/* Any shape but two scalars, which the consumer folds itself. */
static TessStatusCode
arith_evaluate(TessFunctionCall *call)
{
	const TessFunctionArg *left;
	const TessFunctionArg *right;
	TessArithOp op;

	if (!valid_call(call))
		return invalid(call, "int4 arithmetic takes two arguments");
	left = &call->args[0];
	right = &call->args[1];
	op = (TessArithOp) operation(call);
	if (left->column != NULL && right->column != NULL)
		return tess_int4_arith_columns(op, left->column, left->prepared,
									   right->column, right->prepared,
									   call->rows, (int32 *) call->values,
									   call->non_nulls, call->status);
	if (left->column != NULL)
		return tess_int4_arith_scalar(op, left->column,
									  DatumGetInt32(right->scalar),
									  left->prepared, call->rows,
									  (int32 *) call->values, call->non_nulls,
									  call->status);
	if (right->column != NULL)
		return tess_int4_arith_scalar_left(op, DatumGetInt32(left->scalar),
										   right->column, right->prepared,
										   call->rows, (int32 *) call->values,
										   call->non_nulls, call->status);
	return invalid(call, "int4 arithmetic needs a column argument");
}

/* -x is 0 - x, including the overflow of the smallest value. */
static TessStatusCode
negate_evaluate(TessFunctionCall *call)
{
	if (call == NULL || call->struct_size < TESS_FUNCTION_CALL_MIN_SIZE ||
		call->nargs != 1 || call->args == NULL ||
		call->args[0].struct_size < TESS_FUNCTION_ARG_MIN_SIZE)
		return invalid(call, "int4 negation takes one argument");
	if (call->args[0].column == NULL)
		return invalid(call, "int4 negation takes a column");
	return tess_int4_arith_scalar_left(TESS_ARITH_SUB, 0, call->args[0].column,
									   call->args[0].prepared, call->rows,
									   (int32 *) call->values, call->non_nulls,
									   call->status);
}

/* column op scalar over int8 values. */
static TessStatusCode
compare8_evaluate(TessFunctionCall *call)
{
	if (!valid_call(call))
		return invalid(call, "an int8 comparison takes two arguments");
	if (call->args[0].column == NULL || call->args[1].column != NULL)
		return invalid(call, "an int8 comparison takes a column and a scalar");
	return tess_int8_filter(call->args[0].column, call->args[0].prepared,
							call->rows, (TessCompareOp) operation(call),
							DatumGetInt64(call->args[1].scalar), call->status);
}

/* A bigint column against an integer scalar: the scalar widens. */
static TessStatusCode
compare84_evaluate(TessFunctionCall *call)
{
	if (!valid_call(call))
		return invalid(call, "an int8 comparison takes two arguments");
	if (call->args[0].column == NULL || call->args[1].column != NULL)
		return invalid(call, "an int8 comparison takes a column and a scalar");
	return tess_int8_filter(call->args[0].column, call->args[0].prepared,
							call->rows, (TessCompareOp) operation(call),
							(int64) DatumGetInt32(call->args[1].scalar),
							call->status);
}

/*
 * An integer column against a bigint scalar: a scalar within the int4
 * range compares as int4; one beyond it makes the comparison constant for
 * every non-NULL value, which the int4 kernel expresses as a comparison
 * every value satisfies (>= the least int4) or none does (< the least).
 */
static TessStatusCode
compare48_evaluate(TessFunctionCall *call)
{
	TessCompareOp op;
	int64		scalar;
	int32		narrow;

	if (!valid_call(call))
		return invalid(call, "an int4 comparison takes two arguments");
	if (call->args[0].column == NULL || call->args[1].column != NULL)
		return invalid(call, "an int4 comparison takes a column and a scalar");
	op = (TessCompareOp) operation(call);
	scalar = DatumGetInt64(call->args[1].scalar);
	if (scalar > PG_INT32_MAX || scalar < PG_INT32_MIN)
	{
		bool		above = scalar > PG_INT32_MAX;
		bool		all;

		switch (op)
		{
			case TESS_CMP_NE:
				all = true;
				break;
			case TESS_CMP_LT:
			case TESS_CMP_LE:
				all = above;
				break;
			case TESS_CMP_GT:
			case TESS_CMP_GE:
				all = !above;
				break;
			default:
				all = false;
				break;
		}
		op = all ? TESS_CMP_GE : TESS_CMP_LT;
		narrow = PG_INT32_MIN;
	}
	else
		narrow = (int32) scalar;
	return tess_int4_filter(call->args[0].column, call->args[0].prepared,
							call->rows, op, narrow, call->status);
}

/* Any shape but two scalars, into a column of int8 Datums. */
static TessStatusCode
arith8_evaluate(TessFunctionCall *call)
{
	const TessFunctionArg *left;
	const TessFunctionArg *right;
	TessArithOp op;

	if (!valid_call(call))
		return invalid(call, "int8 arithmetic takes two arguments");
	left = &call->args[0];
	right = &call->args[1];
	op = (TessArithOp) operation(call);
	if (left->column != NULL && right->column != NULL)
		return tess_int8_arith_columns(op, left->column, left->prepared,
									   right->column, right->prepared,
									   call->rows, (int64 *) call->values,
									   call->non_nulls, call->status);
	if (left->column != NULL)
		return tess_int8_arith_scalar(op, left->column,
									  DatumGetInt64(right->scalar),
									  left->prepared, call->rows,
									  (int64 *) call->values, call->non_nulls,
									  call->status);
	if (right->column != NULL)
		return tess_int8_arith_scalar_left(op, DatumGetInt64(left->scalar),
										   right->column, right->prepared,
										   call->rows, (int64 *) call->values,
										   call->non_nulls, call->status);
	return invalid(call, "int8 arithmetic needs a column argument");
}

/* A bigint column with an integer scalar on the right, which widens. */
static TessStatusCode
arith84_evaluate(TessFunctionCall *call)
{
	if (!valid_call(call))
		return invalid(call, "int8 arithmetic takes two arguments");
	if (call->args[0].column == NULL || call->args[1].column != NULL)
		return invalid(call, "mixed int8 arithmetic takes a column and a scalar");
	return tess_int8_arith_scalar((TessArithOp) operation(call),
								  call->args[0].column,
								  (int64) DatumGetInt32(call->args[1].scalar),
								  call->args[0].prepared, call->rows,
								  (int64 *) call->values, call->non_nulls,
								  call->status);
}

/* -x is 0 - x over int8 values, including the overflow of the smallest. */
static TessStatusCode
negate8_evaluate(TessFunctionCall *call)
{
	if (call == NULL || call->struct_size < TESS_FUNCTION_CALL_MIN_SIZE ||
		call->nargs != 1 || call->args == NULL ||
		call->args[0].struct_size < TESS_FUNCTION_ARG_MIN_SIZE)
		return invalid(call, "int8 negation takes one argument");
	if (call->args[0].column == NULL)
		return invalid(call, "int8 negation takes a column");
	return tess_int8_arith_scalar_left(TESS_ARITH_SUB, 0, call->args[0].column,
									   call->args[0].prepared, call->rows,
									   (int64 *) call->values, call->non_nulls,
									   call->status);
}

/* int8(int4): the int4 column widened into int8 Datums. */
static TessStatusCode
cast_evaluate(TessFunctionCall *call)
{
	if (call == NULL || call->struct_size < TESS_FUNCTION_CALL_MIN_SIZE ||
		call->nargs != 1 || call->args == NULL ||
		call->args[0].struct_size < TESS_FUNCTION_ARG_MIN_SIZE)
		return invalid(call, "the cast to int8 takes one argument");
	if (call->args[0].column == NULL)
		return invalid(call, "the cast to int8 takes a column");
	return tess_int4_to_int8(call->args[0].column, call->args[0].prepared,
							 call->rows, call->values, call->non_nulls,
							 call->status);
}

/*
 * count(*) over the selection, or one of the column aggregates: the partial
 * goes into the call's one Datum, its presence into the one-row mask.
 */
static TessStatusCode
aggregate_evaluate(TessFunctionCall *call)
{
	const TessFunctionArg *arg;
	Datum	   *result;
	bool		isnull = false;
	TessStatusCode code = TESS_OK;

	if (call == NULL || call->struct_size < TESS_FUNCTION_CALL_MIN_SIZE ||
		call->rows == NULL || call->values == NULL ||
		call->non_nulls == NULL || call->non_nulls->nrows != 1 ||
		call->non_nulls->bits == NULL)
		return invalid(call, "an aggregate needs a Datum and a mask of one row");
	result = call->values;
	if (operation(call) == AGG_COUNT_ROWS)
	{
		if (call->nargs != 0)
			return invalid(call, "count(*) takes no argument");
		*result = Int64GetDatum((int64) tess_row_mask_count(call->rows));
		call->non_nulls->bits[0] = 1;
		return TESS_OK;
	}
	if (call->nargs != 1 || call->args == NULL ||
		call->args[0].struct_size < TESS_FUNCTION_ARG_MIN_SIZE ||
		call->args[0].column == NULL)
		return invalid(call, "an aggregate takes one column");
	arg = &call->args[0];
	switch (operation(call))
	{
		case AGG_COUNT:
			{
				int64		count = 0;

				/* The count reads the NULL flags alone, whatever the type. */
				code = tess_count(arg->column, arg->prepared, call->rows,
								  &count, call->status);
				*result = Int64GetDatum(count);
				break;
			}
		case AGG_SUM_INT4:
			{
				int64		sum = 0;

				code = tess_int4_sum(arg->column, arg->prepared, call->rows,
									 &isnull, &sum, call->status);
				*result = Int64GetDatum(sum);
				break;
			}
		case AGG_MIN_INT4:
		case AGG_MAX_INT4:
			{
				int32		value = 0;

				code = operation(call) == AGG_MIN_INT4 ?
					tess_int4_min(arg->column, arg->prepared, call->rows,
								  &isnull, &value, call->status) :
					tess_int4_max(arg->column, arg->prepared, call->rows,
								  &isnull, &value, call->status);
				*result = Int32GetDatum(value);
				break;
			}
		case AGG_MIN_INT8:
		case AGG_MAX_INT8:
			{
				int64		value = 0;

				code = operation(call) == AGG_MIN_INT8 ?
					tess_int8_min(arg->column, arg->prepared, call->rows,
								  &isnull, &value, call->status) :
					tess_int8_max(arg->column, arg->prepared, call->rows,
								  &isnull, &value, call->status);
				*result = Int64GetDatum(value);
				break;
			}
		default:
			return invalid(call, "unknown aggregate");
	}
	if (code == TESS_OK)
		call->non_nulls->bits[0] = isnull ? 0 : 1;
	return code;
}

void
_PG_init(void)
{
	const TessApi *api;
	void	  **rendezvous;
	int			i;

	rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	api = *rendezvous;
	if (api == NULL)
		ereport(ERROR,
				(errcode(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE),
				 errmsg("Tessera bridge must be loaded before tessera_kernels")));
	if (api->abi_version != TESS_API_ABI_VERSION ||
		api->struct_size < TESS_API_MIN_SIZE ||
		api->functions == NULL ||
		api->functions->abi_version != TESS_FUNCTION_REGISTRY_OPS_ABI_VERSION ||
		api->functions->struct_size < TESS_FUNCTION_REGISTRY_OPS_MIN_SIZE)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("incompatible Tessera function registry")));
	if (tess_kernels_abi_version() != TESS_KERNELS_ABI_VERSION ||
		tess_table_format_version() != TESS_TABLE_FORMAT_VERSION)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("incompatible Tessera kernels library")));
	/* A bridge without a kernel registry predates the nodes that use it. */
	if (TESS_ABI_HAS_FIELD(api, TessApi, kernels) && api->kernels != NULL)
	{
		if (api->kernels->abi_version != TESS_KERNEL_REGISTRY_OPS_ABI_VERSION ||
			api->kernels->struct_size < TESS_KERNEL_REGISTRY_OPS_MIN_SIZE)
			ereport(ERROR,
					(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
					 errmsg("incompatible Tessera kernel registry")));
		api->kernels->set(&tess_kernel_ops);
	}
	for (i = 0; i < lengthof(functions); i++)
		api->functions->add(&functions[i].function);
}

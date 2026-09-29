/*
 * The built-in integer kernels as batch functions: this module links the
 * Rust kernels and registers them in the bridge's function registry when
 * loaded, for int4 and int8 alike, and installs the key hashes and the
 * hash table in the bridge's kernel registry (table.c). Load the bridge first (CREATE EXTENSION
 * tessera), then LOAD 'tessera_kernels'.
 *
 * An int8 is its Datum: the int8 kernels write int64 values straight into
 * a Datum column, so their results are TESS_RESULT_DATUM. The functions
 * over an int4 and an int8 (int84eq, int48pl and their siblings) are
 * equivalents: the int8 function over the int4 argument cast by int8(int4),
 * which the consumer compiles as a step of its own, since an int8 kernel
 * must not read an int4 Datum as a whole word.
 *
 * A date is an int32 and a timestamp, with or without time zone, an int64,
 * whose integer order is the type's, infinities included (the extremes of
 * the integer): their comparisons are the int4 and int8 ones. A smallint
 * is its value sign-extended in the word and a boolean 0 or 1, which the
 * int4 kernels read as int32: their comparisons are the int4 ones, and a
 * smallint's arithmetic the int4 one, whose results a smallint result
 * checks against its range, 22003 "smallint out of range" past it (no
 * operation of two smallints overflows an int32 first).
 */
#include "postgres.h"

#include "datatype/timestamp.h"
#include "fmgr.h"
#include "port/pg_bitutils.h"
#include "utils/date.h"
#include "utils/fmgroids.h"
#include "utils/timestamp.h"

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
static TessStatusCode arith8_evaluate(TessFunctionCall *call);
static TessStatusCode negate8_evaluate(TessFunctionCall *call);
static TessStatusCode cast_evaluate(TessFunctionCall *call);
static TessStatusCode narrow_evaluate(TessFunctionCall *call);
static TessStatusCode arith2_evaluate(TessFunctionCall *call);
static TessStatusCode negate2_evaluate(TessFunctionCall *call);
static TessStatusCode widen2_evaluate(TessFunctionCall *call);
static TessStatusCode narrow2_evaluate(TessFunctionCall *call);
static TessStatusCode date_timestamp_evaluate(TessFunctionCall *call);

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

/*
 * A comparison in any shape: a column with a scalar on either side, or two
 * columns, which a join's residual clause compares.
 */
#define COMPARE(oid, code, fn) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_PREDICATE, \
	  .result_format = TESS_RESULT_DATUM, \
	  .flags = TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE | \
	  TESS_FUNCTION_ANY_SHAPE, \
	  .evaluate = (fn)}, (code)}

/* A comparison of a column, first, with a scalar, or through the commutator. */
#define COMPARE_SCALAR(oid, code, fn) \
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

/*
 * A function over an int4 and an int8: the int8 function target over the
 * arguments, the int4 one widened by the cast given for it.
 */
#define EQUIVALENT(oid, target, cast0, cast1) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_EQUIVALENT, \
	  .result_format = TESS_RESULT_DATUM, \
	  .flags = TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE, \
	  .equivalent = (target), .arg_casts = {(cast0), (cast1)}}, 0}

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
	/* date as int4, timestamp and timestamptz as int8 */
	COMPARE(F_DATE_EQ, TESS_CMP_EQ, compare_evaluate),
	COMPARE(F_DATE_NE, TESS_CMP_NE, compare_evaluate),
	COMPARE(F_DATE_LT, TESS_CMP_LT, compare_evaluate),
	COMPARE(F_DATE_LE, TESS_CMP_LE, compare_evaluate),
	COMPARE(F_DATE_GT, TESS_CMP_GT, compare_evaluate),
	COMPARE(F_DATE_GE, TESS_CMP_GE, compare_evaluate),
	COMPARE(F_TIMESTAMP_EQ, TESS_CMP_EQ, compare8_evaluate),
	COMPARE(F_TIMESTAMP_NE, TESS_CMP_NE, compare8_evaluate),
	COMPARE(F_TIMESTAMP_LT, TESS_CMP_LT, compare8_evaluate),
	COMPARE(F_TIMESTAMP_LE, TESS_CMP_LE, compare8_evaluate),
	COMPARE(F_TIMESTAMP_GT, TESS_CMP_GT, compare8_evaluate),
	COMPARE(F_TIMESTAMP_GE, TESS_CMP_GE, compare8_evaluate),
	COMPARE(F_TIMESTAMPTZ_EQ, TESS_CMP_EQ, compare8_evaluate),
	COMPARE(F_TIMESTAMPTZ_NE, TESS_CMP_NE, compare8_evaluate),
	COMPARE(F_TIMESTAMPTZ_LT, TESS_CMP_LT, compare8_evaluate),
	COMPARE(F_TIMESTAMPTZ_LE, TESS_CMP_LE, compare8_evaluate),
	COMPARE(F_TIMESTAMPTZ_GT, TESS_CMP_GT, compare8_evaluate),
	COMPARE(F_TIMESTAMPTZ_GE, TESS_CMP_GE, compare8_evaluate),
	/* bigint against an integer, an integer against bigint */
	EQUIVALENT(F_INT84EQ, F_INT8EQ, InvalidOid, F_INT8_INT4),
	EQUIVALENT(F_INT84NE, F_INT8NE, InvalidOid, F_INT8_INT4),
	EQUIVALENT(F_INT84LT, F_INT8LT, InvalidOid, F_INT8_INT4),
	EQUIVALENT(F_INT84LE, F_INT8LE, InvalidOid, F_INT8_INT4),
	EQUIVALENT(F_INT84GT, F_INT8GT, InvalidOid, F_INT8_INT4),
	EQUIVALENT(F_INT84GE, F_INT8GE, InvalidOid, F_INT8_INT4),
	EQUIVALENT(F_INT48EQ, F_INT8EQ, F_INT8_INT4, InvalidOid),
	EQUIVALENT(F_INT48NE, F_INT8NE, F_INT8_INT4, InvalidOid),
	EQUIVALENT(F_INT48LT, F_INT8LT, F_INT8_INT4, InvalidOid),
	EQUIVALENT(F_INT48LE, F_INT8LE, F_INT8_INT4, InvalidOid),
	EQUIVALENT(F_INT48GT, F_INT8GT, F_INT8_INT4, InvalidOid),
	EQUIVALENT(F_INT48GE, F_INT8GE, F_INT8_INT4, InvalidOid),
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
	/* there is no int84mod or int48mod: the core casts the operand instead */
	EQUIVALENT(F_INT84PL, F_INT8PL, InvalidOid, F_INT8_INT4),
	EQUIVALENT(F_INT84MI, F_INT8MI, InvalidOid, F_INT8_INT4),
	EQUIVALENT(F_INT84MUL, F_INT8MUL, InvalidOid, F_INT8_INT4),
	EQUIVALENT(F_INT84DIV, F_INT8DIV, InvalidOid, F_INT8_INT4),
	EQUIVALENT(F_INT48PL, F_INT8PL, F_INT8_INT4, InvalidOid),
	EQUIVALENT(F_INT48MI, F_INT8MI, F_INT8_INT4, InvalidOid),
	EQUIVALENT(F_INT48MUL, F_INT8MUL, F_INT8_INT4, InvalidOid),
	EQUIVALENT(F_INT48DIV, F_INT8DIV, F_INT8_INT4, InvalidOid),
	VALUE(F_INT8UM, TESS_ARITH_SUB, TESS_RESULT_DATUM, 0, negate8_evaluate),
	/* int8(int4): the cast, a column of int8 Datums */
	VALUE(F_INT8_INT4, 0, TESS_RESULT_DATUM, 0, cast_evaluate),
	/* int4(int8): the explicit cast, 22003 past the int4 range */
	VALUE(F_INT4_INT8, 0, TESS_RESULT_INT32, 0, narrow_evaluate),
	AGGREGATE(F_MIN_INT8, AGG_MIN_INT8),
	AGGREGATE(F_MAX_INT8, AGG_MAX_INT8),
	/* boolean as int4: 0 and 1 */
	COMPARE(F_BOOLEQ, TESS_CMP_EQ, compare_evaluate),
	COMPARE(F_BOOLNE, TESS_CMP_NE, compare_evaluate),
	COMPARE(F_BOOLLT, TESS_CMP_LT, compare_evaluate),
	COMPARE(F_BOOLLE, TESS_CMP_LE, compare_evaluate),
	COMPARE(F_BOOLGT, TESS_CMP_GT, compare_evaluate),
	COMPARE(F_BOOLGE, TESS_CMP_GE, compare_evaluate),
	/* smallint as int4, against smallint and integer */
	COMPARE(F_INT2EQ, TESS_CMP_EQ, compare_evaluate),
	COMPARE(F_INT2NE, TESS_CMP_NE, compare_evaluate),
	COMPARE(F_INT2LT, TESS_CMP_LT, compare_evaluate),
	COMPARE(F_INT2LE, TESS_CMP_LE, compare_evaluate),
	COMPARE(F_INT2GT, TESS_CMP_GT, compare_evaluate),
	COMPARE(F_INT2GE, TESS_CMP_GE, compare_evaluate),
	COMPARE(F_INT24EQ, TESS_CMP_EQ, compare_evaluate),
	COMPARE(F_INT24NE, TESS_CMP_NE, compare_evaluate),
	COMPARE(F_INT24LT, TESS_CMP_LT, compare_evaluate),
	COMPARE(F_INT24LE, TESS_CMP_LE, compare_evaluate),
	COMPARE(F_INT24GT, TESS_CMP_GT, compare_evaluate),
	COMPARE(F_INT24GE, TESS_CMP_GE, compare_evaluate),
	COMPARE(F_INT42EQ, TESS_CMP_EQ, compare_evaluate),
	COMPARE(F_INT42NE, TESS_CMP_NE, compare_evaluate),
	COMPARE(F_INT42LT, TESS_CMP_LT, compare_evaluate),
	COMPARE(F_INT42LE, TESS_CMP_LE, compare_evaluate),
	COMPARE(F_INT42GT, TESS_CMP_GT, compare_evaluate),
	COMPARE(F_INT42GE, TESS_CMP_GE, compare_evaluate),
	/* smallint arithmetic: the int4 one within the smallint range */
	VALUE(F_INT2PL, TESS_ARITH_ADD, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith2_evaluate),
	VALUE(F_INT2MI, TESS_ARITH_SUB, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith2_evaluate),
	VALUE(F_INT2MUL, TESS_ARITH_MUL, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith2_evaluate),
	VALUE(F_INT2DIV, TESS_ARITH_DIV, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith2_evaluate),
	VALUE(F_INT2MOD, TESS_ARITH_MOD, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith2_evaluate),
	VALUE(F_INT2UM, TESS_ARITH_SUB, TESS_RESULT_INT32, 0, negate2_evaluate),
	/* smallint with integer: the int4 arithmetic, an integer result */
	VALUE(F_INT24PL, TESS_ARITH_ADD, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith_evaluate),
	VALUE(F_INT24MI, TESS_ARITH_SUB, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith_evaluate),
	VALUE(F_INT24MUL, TESS_ARITH_MUL, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith_evaluate),
	VALUE(F_INT24DIV, TESS_ARITH_DIV, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith_evaluate),
	VALUE(F_INT42PL, TESS_ARITH_ADD, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith_evaluate),
	VALUE(F_INT42MI, TESS_ARITH_SUB, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith_evaluate),
	VALUE(F_INT42MUL, TESS_ARITH_MUL, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith_evaluate),
	VALUE(F_INT42DIV, TESS_ARITH_DIV, TESS_RESULT_INT32, TESS_FUNCTION_ANY_SHAPE,
		  arith_evaluate),
	/* smallint with bigint: the int8 function over the smallint widened */
	VALUE(F_INT8_INT2, 0, TESS_RESULT_DATUM, 0, cast_evaluate),
	EQUIVALENT(F_INT28EQ, F_INT8EQ, F_INT8_INT2, InvalidOid),
	EQUIVALENT(F_INT28NE, F_INT8NE, F_INT8_INT2, InvalidOid),
	EQUIVALENT(F_INT28LT, F_INT8LT, F_INT8_INT2, InvalidOid),
	EQUIVALENT(F_INT28LE, F_INT8LE, F_INT8_INT2, InvalidOid),
	EQUIVALENT(F_INT28GT, F_INT8GT, F_INT8_INT2, InvalidOid),
	EQUIVALENT(F_INT28GE, F_INT8GE, F_INT8_INT2, InvalidOid),
	EQUIVALENT(F_INT82EQ, F_INT8EQ, InvalidOid, F_INT8_INT2),
	EQUIVALENT(F_INT82NE, F_INT8NE, InvalidOid, F_INT8_INT2),
	EQUIVALENT(F_INT82LT, F_INT8LT, InvalidOid, F_INT8_INT2),
	EQUIVALENT(F_INT82LE, F_INT8LE, InvalidOid, F_INT8_INT2),
	EQUIVALENT(F_INT82GT, F_INT8GT, InvalidOid, F_INT8_INT2),
	EQUIVALENT(F_INT82GE, F_INT8GE, InvalidOid, F_INT8_INT2),
	EQUIVALENT(F_INT28PL, F_INT8PL, F_INT8_INT2, InvalidOid),
	EQUIVALENT(F_INT28MI, F_INT8MI, F_INT8_INT2, InvalidOid),
	EQUIVALENT(F_INT28MUL, F_INT8MUL, F_INT8_INT2, InvalidOid),
	EQUIVALENT(F_INT28DIV, F_INT8DIV, F_INT8_INT2, InvalidOid),
	EQUIVALENT(F_INT82PL, F_INT8PL, InvalidOid, F_INT8_INT2),
	EQUIVALENT(F_INT82MI, F_INT8MI, InvalidOid, F_INT8_INT2),
	EQUIVALENT(F_INT82MUL, F_INT8MUL, InvalidOid, F_INT8_INT2),
	EQUIVALENT(F_INT82DIV, F_INT8DIV, InvalidOid, F_INT8_INT2),
	/*
	 * a date column against a timestamp scalar, the timestamp made the
	 * date bound the comparison keeps (timestamp < date through the
	 * commutator)
	 */
	COMPARE_SCALAR(F_DATE_EQ_TIMESTAMP, TESS_CMP_EQ, date_timestamp_evaluate),
	COMPARE_SCALAR(F_DATE_NE_TIMESTAMP, TESS_CMP_NE, date_timestamp_evaluate),
	COMPARE_SCALAR(F_DATE_LT_TIMESTAMP, TESS_CMP_LT, date_timestamp_evaluate),
	COMPARE_SCALAR(F_DATE_LE_TIMESTAMP, TESS_CMP_LE, date_timestamp_evaluate),
	COMPARE_SCALAR(F_DATE_GT_TIMESTAMP, TESS_CMP_GT, date_timestamp_evaluate),
	COMPARE_SCALAR(F_DATE_GE_TIMESTAMP, TESS_CMP_GE, date_timestamp_evaluate),
	/* int4(int2) the same value, int2(int4) within the smallint range */
	VALUE(F_INT4_INT2, 0, TESS_RESULT_INT32, 0, widen2_evaluate),
	VALUE(F_INT2_INT4, 0, TESS_RESULT_INT32, 0, narrow2_evaluate),
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

/* The comparison with its sides swapped: scalar op column as column op' scalar. */
static TessCompareOp
flip(TessCompareOp op)
{
	switch (op)
	{
		case TESS_CMP_LT:
			return TESS_CMP_GT;
		case TESS_CMP_LE:
			return TESS_CMP_GE;
		case TESS_CMP_GT:
			return TESS_CMP_LT;
		case TESS_CMP_GE:
			return TESS_CMP_LE;
		default:
			return op;
	}
}

/* int4 against int4, in any shape. */
static TessStatusCode
compare_evaluate(TessFunctionCall *call)
{
	const TessFunctionArg *left;
	const TessFunctionArg *right;
	TessCompareOp op;

	if (!valid_call(call))
		return invalid(call, "an int4 comparison takes two arguments");
	left = &call->args[0];
	right = &call->args[1];
	op = (TessCompareOp) operation(call);
	if (left->column != NULL && right->column != NULL)
		return tess_int4_compare_columns(left->column, left->prepared,
										 right->column, right->prepared,
										 call->rows, op, call->status);
	if (left->column != NULL)
		return tess_int4_filter(left->column, left->prepared, call->rows, op,
								DatumGetInt32(right->scalar), call->status);
	if (right->column != NULL)
		return tess_int4_filter(right->column, right->prepared, call->rows,
								flip(op), DatumGetInt32(left->scalar),
								call->status);
	return invalid(call, "an int4 comparison needs a column argument");
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
	const TessFunctionArg *left;
	const TessFunctionArg *right;
	TessCompareOp op;

	if (!valid_call(call))
		return invalid(call, "an int8 comparison takes two arguments");
	left = &call->args[0];
	right = &call->args[1];
	op = (TessCompareOp) operation(call);
	if (left->column != NULL && right->column != NULL)
		return tess_int8_compare_columns(left->column, left->prepared,
										 right->column, right->prepared,
										 call->rows, op, call->status);
	if (left->column != NULL)
		return tess_int8_filter(left->column, left->prepared, call->rows, op,
								DatumGetInt64(right->scalar), call->status);
	if (right->column != NULL)
		return tess_int8_filter(right->column, right->prepared, call->rows,
								flip(op), DatumGetInt64(left->scalar),
								call->status);
	return invalid(call, "an int8 comparison needs a column argument");
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

/* int4(int8): the int8 column narrowed into int32 values. */
static TessStatusCode
narrow_evaluate(TessFunctionCall *call)
{
	if (call == NULL || call->struct_size < TESS_FUNCTION_CALL_MIN_SIZE ||
		call->nargs != 1 || call->args == NULL ||
		call->args[0].struct_size < TESS_FUNCTION_ARG_MIN_SIZE)
		return invalid(call, "the cast to int4 takes one argument");
	if (call->args[0].column == NULL)
		return invalid(call, "the cast to int4 takes a column");
	return tess_int8_to_int4(call->args[0].column, call->args[0].prepared,
							 call->rows, (int32 *) call->values,
							 call->non_nulls, call->status);
}

/*
 * A smallint result: the int4 kernel's values of the rows it computed,
 * 22003 "smallint out of range" for one past the smallint range.
 */
static TessStatusCode
smallint_range(TessFunctionCall *call, TessStatusCode code)
{
	const int32 *values = (const int32 *) call->values;
	int			nwords;
	bool		outside = false;

	if (code != TESS_OK)
		return code;
	nwords = tess_row_mask_word_count(call->non_nulls->nrows);
	/* A whole word without a branch a row; a partial one row by row. */
	for (int word = 0; word < nwords && !outside; word++)
	{
		uint64		bits = call->non_nulls->bits[word];
		const int32 *value = values + (Size) word * 64;

		if (bits == UINT64_MAX)
		{
			for (int bit = 0; bit < 64; bit++)
				outside |= (value[bit] < PG_INT16_MIN) | (value[bit] > PG_INT16_MAX);
		}
		else
		{
			for (; bits != 0; bits &= bits - 1)
			{
				int			bit = pg_rightmost_one_pos64(bits);

				outside |= (value[bit] < PG_INT16_MIN) | (value[bit] > PG_INT16_MAX);
			}
		}
	}
	if (outside)
	{
		if (call->status != NULL && call->status->struct_size >= TESS_STATUS_MIN_SIZE)
		{
			call->status->code = TESS_ERROR_INTEGER_OUT_OF_RANGE;
			strlcpy(call->status->sqlstate, "22003", sizeof(call->status->sqlstate));
			strlcpy(call->status->message, "smallint out of range",
					sizeof(call->status->message));
		}
		return TESS_ERROR_INTEGER_OUT_OF_RANGE;
	}
	return TESS_OK;
}

static TessStatusCode
arith2_evaluate(TessFunctionCall *call)
{
	return smallint_range(call, arith_evaluate(call));
}

static TessStatusCode
negate2_evaluate(TessFunctionCall *call)
{
	return smallint_range(call, negate_evaluate(call));
}

/* The column's int32 values as they are: x + 0. */
static TessStatusCode
widen2_evaluate(TessFunctionCall *call)
{
	if (call == NULL || call->struct_size < TESS_FUNCTION_CALL_MIN_SIZE ||
		call->nargs != 1 || call->args == NULL ||
		call->args[0].struct_size < TESS_FUNCTION_ARG_MIN_SIZE)
		return invalid(call, "the cast between smallint and integer takes one argument");
	if (call->args[0].column == NULL)
		return invalid(call, "the cast between smallint and integer takes a column");
	return tess_int4_arith_scalar(TESS_ARITH_ADD, call->args[0].column, 0,
								  call->args[0].prepared, call->rows,
								  (int32 *) call->values, call->non_nulls, call->status);
}

static TessStatusCode
narrow2_evaluate(TessFunctionCall *call)
{
	return smallint_range(call, widen2_evaluate(call));
}

/*
 * A date column against a timestamp scalar: the date d is the timestamp
 * d days since 2000-01-01, so d op T is d op' B for the date bound B of T,
 * once a call, and the int4 filter compares: d < T keeps d < ceil(T / day),
 * d <= T d <= floor, d > T d > floor, d >= T d >= ceil, d = T d = T / day
 * where T is a midnight and no date else, d <> T its complement. An
 * infinite timestamp is the infinite date of its sign, which a date
 * infinity equals; a finite date past the timestamps' range stays above
 * every finite bound and below infinity, as date_cmp_timestamp orders it.
 */
static TessStatusCode
date_timestamp_evaluate(TessFunctionCall *call)
{
	TessCompareOp op;
	Timestamp	timestamp;
	int32		bound;

	if (!valid_call(call))
		return invalid(call, "a date against a timestamp takes two arguments");
	if (call->args[0].column == NULL || call->args[1].column != NULL)
		return invalid(call, "a date against a timestamp takes a date column and a scalar");
	op = (TessCompareOp) operation(call);
	timestamp = DatumGetTimestamp(call->args[1].scalar);
	if (TIMESTAMP_IS_NOBEGIN(timestamp))
		bound = DATEVAL_NOBEGIN;
	else if (TIMESTAMP_IS_NOEND(timestamp))
		bound = DATEVAL_NOEND;
	else
	{
		int64		days = timestamp / USECS_PER_DAY;
		int64		rest = timestamp % USECS_PER_DAY;

		if (rest < 0)
		{
			days--;
			rest += USECS_PER_DAY;
		}
		bound = (int32) days;
		switch (op)
		{
			case TESS_CMP_LT:
			case TESS_CMP_GE:
				bound += rest > 0 ? 1 : 0;
				break;
			case TESS_CMP_EQ:
			case TESS_CMP_NE:
				/* No date is a timestamp past a midnight: none, or every one. */
				if (rest > 0)
				{
					op = op == TESS_CMP_EQ ? TESS_CMP_LT : TESS_CMP_GE;
					bound = PG_INT32_MIN;
				}
				break;
			case TESS_CMP_LE:
			case TESS_CMP_GT:
				break;
		}
	}
	return tess_int4_filter(call->args[0].column, call->args[0].prepared, call->rows, op,
							bound, call->status);
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
	tess_register_text_functions(api->functions);
}

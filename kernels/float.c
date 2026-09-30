/*
 * The float functions as batch functions, row by row in C over the
 * Datums: the comparisons of float8 and float4 and between them, their
 * arithmetic, negation and abs, and the casts between floats and
 * integers. The comparisons and the arithmetic are the core's inline
 * functions of utils/float.h, so NaN is equal to itself and above every
 * number, and overflow, underflow and division by zero raise the core's
 * errors (22003 "value out of range: overflow" and "underflow", 22012) at
 * the row that meets them. A float4 compares with a float8 widened, as
 * float48eq does, which loses nothing.
 */
#include "postgres.h"

#include <math.h>

#include "fmgr.h"
#include "port/pg_bitutils.h"
#include "utils/float.h"
#include "utils/fmgroids.h"

#include "tessera/bridge.h"

#include "internal.h"

typedef enum FloatOp
{
	FLOAT_EQ,
	FLOAT_NE,
	FLOAT_LT,
	FLOAT_LE,
	FLOAT_GT,
	FLOAT_GE,
	FLOAT_PL,
	FLOAT_MI,
	FLOAT_MUL,
	FLOAT_DIV,
	FLOAT_UM,
	FLOAT_ABS,
	FLOAT_CAST
} FloatOp;

/* How a Datum holds a number: float8, float4, an int4 word (int2 too), int8. */
typedef enum FloatType
{
	T_FLOAT8,
	T_FLOAT4,
	T_INT4,
	T_INT2,
	T_INT8
} FloatType;

typedef struct FloatFunction
{
	TessFunction function;
	FloatOp		op;
	FloatType	left;
	FloatType	right;
	FloatType	result;
} FloatFunction;

static TessStatusCode float_compare_evaluate(TessFunctionCall *call);
static TessStatusCode float_value_evaluate(TessFunctionCall *call);

#define FLOAT_FLAGS \
	(TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE | TESS_FUNCTION_ANY_SHAPE)

#define FLOAT_COMPARE(oid, code, l, r) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_PREDICATE, \
	  .result_format = TESS_RESULT_DATUM, .flags = FLOAT_FLAGS, \
	  .evaluate = float_compare_evaluate}, (code), (l), (r), T_FLOAT8}

#define FLOAT_VALUE(oid, code, l, r, res) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_VALUE, \
	  .result_format = (res) == T_INT4 || (res) == T_INT2 ? TESS_RESULT_INT32 : TESS_RESULT_DATUM, \
	  .flags = FLOAT_FLAGS, \
	  .evaluate = float_value_evaluate}, (code), (l), (r), (res)}

#define FLOAT_COMPARES(eq, ne, lt, le, gt, ge, l, r) \
	FLOAT_COMPARE(eq, FLOAT_EQ, l, r), FLOAT_COMPARE(ne, FLOAT_NE, l, r), \
	FLOAT_COMPARE(lt, FLOAT_LT, l, r), FLOAT_COMPARE(le, FLOAT_LE, l, r), \
	FLOAT_COMPARE(gt, FLOAT_GT, l, r), FLOAT_COMPARE(ge, FLOAT_GE, l, r)

#define FLOAT_ARITHMETIC(pl, mi, mul, div, l, r, res) \
	FLOAT_VALUE(pl, FLOAT_PL, l, r, res), FLOAT_VALUE(mi, FLOAT_MI, l, r, res), \
	FLOAT_VALUE(mul, FLOAT_MUL, l, r, res), FLOAT_VALUE(div, FLOAT_DIV, l, r, res)

static const FloatFunction float_functions[] = {
	FLOAT_COMPARES(F_FLOAT8EQ, F_FLOAT8NE, F_FLOAT8LT, F_FLOAT8LE, F_FLOAT8GT, F_FLOAT8GE,
				   T_FLOAT8, T_FLOAT8),
	FLOAT_COMPARES(F_FLOAT4EQ, F_FLOAT4NE, F_FLOAT4LT, F_FLOAT4LE, F_FLOAT4GT, F_FLOAT4GE,
				   T_FLOAT4, T_FLOAT4),
	FLOAT_COMPARES(F_FLOAT48EQ, F_FLOAT48NE, F_FLOAT48LT, F_FLOAT48LE, F_FLOAT48GT,
				   F_FLOAT48GE, T_FLOAT4, T_FLOAT8),
	FLOAT_COMPARES(F_FLOAT84EQ, F_FLOAT84NE, F_FLOAT84LT, F_FLOAT84LE, F_FLOAT84GT,
				   F_FLOAT84GE, T_FLOAT8, T_FLOAT4),
	FLOAT_ARITHMETIC(F_FLOAT8PL, F_FLOAT8MI, F_FLOAT8MUL, F_FLOAT8DIV,
					 T_FLOAT8, T_FLOAT8, T_FLOAT8),
	FLOAT_ARITHMETIC(F_FLOAT4PL, F_FLOAT4MI, F_FLOAT4MUL, F_FLOAT4DIV,
					 T_FLOAT4, T_FLOAT4, T_FLOAT4),
	FLOAT_ARITHMETIC(F_FLOAT48PL, F_FLOAT48MI, F_FLOAT48MUL, F_FLOAT48DIV,
					 T_FLOAT4, T_FLOAT8, T_FLOAT8),
	FLOAT_ARITHMETIC(F_FLOAT84PL, F_FLOAT84MI, F_FLOAT84MUL, F_FLOAT84DIV,
					 T_FLOAT8, T_FLOAT4, T_FLOAT8),
	FLOAT_VALUE(F_FLOAT8UM, FLOAT_UM, T_FLOAT8, T_FLOAT8, T_FLOAT8),
	FLOAT_VALUE(F_FLOAT8ABS, FLOAT_ABS, T_FLOAT8, T_FLOAT8, T_FLOAT8),
	FLOAT_VALUE(F_FLOAT4UM, FLOAT_UM, T_FLOAT4, T_FLOAT4, T_FLOAT4),
	FLOAT_VALUE(F_FLOAT4ABS, FLOAT_ABS, T_FLOAT4, T_FLOAT4, T_FLOAT4),
	FLOAT_VALUE(F_FLOAT8_INT4, FLOAT_CAST, T_INT4, T_INT4, T_FLOAT8),
	FLOAT_VALUE(F_FLOAT8_INT2, FLOAT_CAST, T_INT4, T_INT4, T_FLOAT8),
	FLOAT_VALUE(F_FLOAT8_INT8, FLOAT_CAST, T_INT8, T_INT8, T_FLOAT8),
	FLOAT_VALUE(F_FLOAT8_FLOAT4, FLOAT_CAST, T_FLOAT4, T_FLOAT4, T_FLOAT8),
	FLOAT_VALUE(F_FLOAT4_INT4, FLOAT_CAST, T_INT4, T_INT4, T_FLOAT4),
	FLOAT_VALUE(F_FLOAT4_INT2, FLOAT_CAST, T_INT4, T_INT4, T_FLOAT4),
	FLOAT_VALUE(F_FLOAT4_INT8, FLOAT_CAST, T_INT8, T_INT8, T_FLOAT4),
	FLOAT_VALUE(F_FLOAT4_FLOAT8, FLOAT_CAST, T_FLOAT8, T_FLOAT8, T_FLOAT4),
	FLOAT_VALUE(F_INT4_FLOAT8, FLOAT_CAST, T_FLOAT8, T_FLOAT8, T_INT4),
	FLOAT_VALUE(F_INT2_FLOAT8, FLOAT_CAST, T_FLOAT8, T_FLOAT8, T_INT2),
	FLOAT_VALUE(F_INT8_FLOAT8, FLOAT_CAST, T_FLOAT8, T_FLOAT8, T_INT8),
	FLOAT_VALUE(F_INT4_FLOAT4, FLOAT_CAST, T_FLOAT4, T_FLOAT4, T_INT4),
	FLOAT_VALUE(F_INT2_FLOAT4, FLOAT_CAST, T_FLOAT4, T_FLOAT4, T_INT2),
	FLOAT_VALUE(F_INT8_FLOAT4, FLOAT_CAST, T_FLOAT4, T_FLOAT4, T_INT8),
};

static const FloatFunction *
float_function(const TessFunctionCall *call)
{
	return (const FloatFunction *) ((const char *) call->function -
									offsetof(FloatFunction, function));
}

static bool
float_call_valid(const TessFunctionCall *call, int nargs)
{
	if (call == NULL || call->struct_size < TESS_FUNCTION_CALL_MIN_SIZE ||
		call->nargs != nargs || call->args == NULL || call->rows == NULL)
		return false;
	for (int arg = 0; arg < nargs; arg++)
		if (call->args[arg].struct_size < TESS_FUNCTION_ARG_MIN_SIZE)
			return false;
	return true;
}

static inline Datum
arg_datum(const TessFunctionArg *arg, int row)
{
	return arg->column != NULL ? arg->column->values[row] : arg->scalar;
}

static inline bool
arg_null(const TessFunctionArg *arg, int row)
{
	return arg->column != NULL && arg->column->isnull[row];
}

/* A float argument as a float8: a float4 widens exactly. */
static inline float8
as_float8(Datum value, FloatType type)
{
	return type == T_FLOAT4 ? (float8) DatumGetFloat4(value) : DatumGetFloat8(value);
}

/* Float comparisons of any shape, in the core's NaN-aware order. */
static TessStatusCode
float_compare_evaluate(TessFunctionCall *call)
{
	const FloatFunction *function;
	int			nwords;

	if (!float_call_valid(call, 2))
		return tess_call_invalid(call, "a float comparison takes two arguments");
	function = float_function(call);
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		keep = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			float8		left;
			float8		right;
			bool		result;

			if (arg_null(&call->args[0], row) || arg_null(&call->args[1], row))
				continue;
			left = as_float8(arg_datum(&call->args[0], row), function->left);
			right = as_float8(arg_datum(&call->args[1], row), function->right);
			switch (function->op)
			{
				case FLOAT_EQ:
					result = float8_eq(left, right);
					break;
				case FLOAT_NE:
					result = float8_ne(left, right);
					break;
				case FLOAT_LT:
					result = float8_lt(left, right);
					break;
				case FLOAT_LE:
					result = float8_le(left, right);
					break;
				case FLOAT_GT:
					result = float8_gt(left, right);
					break;
				default:
					result = float8_ge(left, right);
					break;
			}
			if (result)
				keep |= UINT64CONST(1) << bit;
		}
		call->rows->bits[word] = keep;
	}
	return TESS_OK;
}

/* One row's float4 arithmetic, as float4pl and its siblings. */
static inline float4
float4_value(FloatOp op, float4 left, float4 right)
{
	switch (op)
	{
		case FLOAT_PL:
			return float4_pl(left, right);
		case FLOAT_MI:
			return float4_mi(left, right);
		case FLOAT_MUL:
			return float4_mul(left, right);
		case FLOAT_DIV:
			return float4_div(left, right);
		case FLOAT_UM:
			return -left;
		default:
			return fabsf(left);
	}
}

/* One row's float8 arithmetic, as float8pl and its siblings. */
static inline float8
float8_value(FloatOp op, float8 left, float8 right)
{
	switch (op)
	{
		case FLOAT_PL:
			return float8_pl(left, right);
		case FLOAT_MI:
			return float8_mi(left, right);
		case FLOAT_MUL:
			return float8_mul(left, right);
		case FLOAT_DIV:
			return float8_div(left, right);
		case FLOAT_UM:
			return -left;
		default:
			return fabs(left);
	}
}

/* A float to an integer, as dtoi4, dtoi2, dtoi8 and the float4 ones round and check it. */
static inline Datum
float_to_integer(float8 number, FloatType from, FloatType to)
{
	bool		fits;

	/* rint of the float4 itself, as ftoi4 rounds it, then its range. */
	if (from == T_FLOAT4)
	{
		float4		single = rintf((float4) number);

		number = single;
		fits = !isnan(single) &&
			(to == T_INT4 ? FLOAT4_FITS_IN_INT32(single) :
			 to == T_INT2 ? FLOAT4_FITS_IN_INT16(single) : FLOAT4_FITS_IN_INT64(single));
	}
	else
	{
		number = rint(number);
		fits = !isnan(number) &&
			(to == T_INT4 ? FLOAT8_FITS_IN_INT32(number) :
			 to == T_INT2 ? FLOAT8_FITS_IN_INT16(number) : FLOAT8_FITS_IN_INT64(number));
	}
	if (unlikely(!fits))
		ereport(ERROR,
				(errcode(ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE),
				 errmsg(to == T_INT4 ? "integer out of range" :
						to == T_INT2 ? "smallint out of range" : "bigint out of range")));
	return to == T_INT8 ? Int64GetDatum((int64) number) : Int32GetDatum((int32) number);
}

/* A cast's result from one row's argument. */
static inline Datum
float_cast(Datum value, FloatType from, FloatType to)
{
	switch (to)
	{
		case T_FLOAT8:
			return Float8GetDatum(from == T_INT4 ? (float8) DatumGetInt32(value) :
								  from == T_INT8 ? (float8) DatumGetInt64(value) :
								  (float8) DatumGetFloat4(value));
		case T_FLOAT4:
			if (from == T_FLOAT8)
			{
				float8		number = DatumGetFloat8(value);
				float4		single = (float4) number;

				/* As dtof checks it. */
				if (unlikely(isinf(single)) && !isinf(number))
					float_overflow_error();
				if (unlikely(single == 0.0f) && number != 0.0)
					float_underflow_error();
				return Float4GetDatum(single);
			}
			return Float4GetDatum(from == T_INT4 ? (float4) DatumGetInt32(value) :
								  (float4) DatumGetInt64(value));
		default:
			return float_to_integer(as_float8(value, from), from, to);
	}
}

/* Float arithmetic, negation, abs and casts of any shape. */
static TessStatusCode
float_value_evaluate(TessFunctionCall *call)
{
	const FloatFunction *function;
	int			nargs;
	int			nwords;

	if (call == NULL || call->function == NULL)
		return tess_call_invalid(call, "a float function takes its arguments");
	function = float_function(call);
	nargs = function->op >= FLOAT_UM ? 1 : 2;
	if (!float_call_valid(call, nargs) || call->values == NULL || call->non_nulls == NULL)
		return tess_call_invalid(call, "a float function takes its arguments");
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		present = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			Datum		left;
			Datum		right = (Datum) 0;
			Datum		result;

			if (arg_null(&call->args[0], row) ||
				(nargs == 2 && arg_null(&call->args[1], row)))
				continue;
			left = arg_datum(&call->args[0], row);
			if (nargs == 2)
				right = arg_datum(&call->args[1], row);
			if (function->op == FLOAT_CAST)
				result = float_cast(left, function->left, function->result);
			else if (function->result == T_FLOAT4)
				result = Float4GetDatum(float4_value(function->op, DatumGetFloat4(left),
													 nargs == 2 ? DatumGetFloat4(right) : 0));
			else
				result = Float8GetDatum(float8_value(function->op,
													 as_float8(left, function->left),
													 nargs == 2 ?
													 as_float8(right, function->right) : 0));
			if (function->result == T_INT4 || function->result == T_INT2)
				((int32 *) call->values)[row] = DatumGetInt32(result);
			else
				((Datum *) call->values)[row] = result;
			present |= UINT64CONST(1) << bit;
		}
		call->non_nulls->bits[word] = present;
	}
	return TESS_OK;
}

/* Register the float functions. */
void
tess_register_float_functions(const TessFunctionRegistryOps *functions)
{
	for (int i = 0; i < lengthof(float_functions); i++)
		functions->add(&float_functions[i].function);
}

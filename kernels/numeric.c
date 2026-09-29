/*
 * The numeric functions as batch functions, row by row in C over the
 * Datums: the six comparisons by the core's numeric_cmp, whose result a
 * call keeps for the pairs of pointers it met when one is a numeric of the
 * cache below (which repeats as its pointer), and the casts of integers
 * to numeric. The decimal64 kernels of plan 4.21 ж)
 * take the comparisons where a column's typmod allows; these stay for the
 * rest.
 *
 * A numeric of a small integer comes from a cache of the process, one
 * block in TopMemoryContext with a slot per integer: the first one made
 * stays and every later one is the same pointer, read-only as any
 * argument, so a batch of years or months allocates nothing and compares
 * each value once. Others are made in the call's context.
 */
#include "postgres.h"

#include "fmgr.h"
#include "port/pg_bitutils.h"
#include "utils/fmgroids.h"
#include "utils/fmgrprotos.h"
#include "utils/memutils.h"
#include "utils/numeric.h"
#include "varatt.h"

#include "tessera/bridge.h"

#include "internal.h"

typedef enum NumericOp
{
	NUMERIC_EQ,
	NUMERIC_NE,
	NUMERIC_LT,
	NUMERIC_LE,
	NUMERIC_GT,
	NUMERIC_GE,
	NUMERIC_FROM_INT4,
	NUMERIC_FROM_INT8
} NumericOp;

typedef struct NumericFunction
{
	TessFunction function;
	NumericOp	op;
} NumericFunction;

static TessStatusCode numeric_compare_evaluate(TessFunctionCall *call);
static TessStatusCode numeric_cast_evaluate(TessFunctionCall *call);

#define NUMERIC_FLAGS \
	(TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE | TESS_FUNCTION_ANY_SHAPE)

#define NUMERIC_COMPARE(oid, code) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_PREDICATE, \
	  .result_format = TESS_RESULT_DATUM, .flags = NUMERIC_FLAGS, \
	  .evaluate = numeric_compare_evaluate}, (code)}

#define NUMERIC_CAST(oid, code) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_VALUE, \
	  .result_format = TESS_RESULT_DATUM, .flags = NUMERIC_FLAGS, \
	  .evaluate = numeric_cast_evaluate}, (code)}

static const NumericFunction numeric_functions[] = {
	NUMERIC_COMPARE(F_NUMERIC_EQ, NUMERIC_EQ),
	NUMERIC_COMPARE(F_NUMERIC_NE, NUMERIC_NE),
	NUMERIC_COMPARE(F_NUMERIC_LT, NUMERIC_LT),
	NUMERIC_COMPARE(F_NUMERIC_LE, NUMERIC_LE),
	NUMERIC_COMPARE(F_NUMERIC_GT, NUMERIC_GT),
	NUMERIC_COMPARE(F_NUMERIC_GE, NUMERIC_GE),
	/* An int2 is its value in the Datum as an int4 is. */
	NUMERIC_CAST(F_NUMERIC_INT2, NUMERIC_FROM_INT4),
	NUMERIC_CAST(F_NUMERIC_INT4, NUMERIC_FROM_INT4),
	NUMERIC_CAST(F_NUMERIC_INT8, NUMERIC_FROM_INT8),
};

/*
 * The small integers the cache keeps (days, months, years, and around),
 * each in a slot of the block: a numeric below 10000 takes 8 bytes.
 */
#define SMALL_NUMERIC_MIN (-1024)
#define SMALL_NUMERIC_MAX 4096
#define SMALL_NUMERIC_SLOT 16
#define SMALL_NUMERIC_BLOCK \
	((Size) (SMALL_NUMERIC_MAX - SMALL_NUMERIC_MIN) * SMALL_NUMERIC_SLOT)

static char *small_numerics;

static inline bool
small_numeric(Datum value)
{
	const char *pointer = DatumGetPointer(value);

	return pointer >= small_numerics && pointer < small_numerics + SMALL_NUMERIC_BLOCK;
}

Numeric
tess_numeric_from_int64(int64 value, MemoryContext context)
{
	MemoryContext old;
	Numeric		result;

	if (value >= SMALL_NUMERIC_MIN && value < SMALL_NUMERIC_MAX)
	{
		char	   *slot;

		if (small_numerics == NULL)
			small_numerics = MemoryContextAllocZero(TopMemoryContext, SMALL_NUMERIC_BLOCK);
		slot = small_numerics + (value - SMALL_NUMERIC_MIN) * SMALL_NUMERIC_SLOT;
		/* A slot not made yet is zeros, no varlena header. */
		if (*(uint32 *) slot == 0)
		{
			result = int64_to_numeric(value);
			Assert(VARSIZE(result) <= SMALL_NUMERIC_SLOT);
			memcpy(slot, result, VARSIZE(result));
			pfree(result);
		}
		return (Numeric) slot;
	}
	old = MemoryContextSwitchTo(context);
	result = int64_to_numeric(value);
	MemoryContextSwitchTo(old);
	return result;
}

static NumericOp
numeric_op(const TessFunctionCall *call)
{
	return ((const NumericFunction *) ((const char *) call->function -
									   offsetof(NumericFunction, function)))->op;
}

static TessStatusCode
numeric_invalid(TessFunctionCall *call, const char *message)
{
	if (call != NULL && call->status != NULL &&
		call->status->struct_size >= TESS_STATUS_MIN_SIZE)
	{
		call->status->code = TESS_ERROR_INVALID_ARGUMENT;
		strlcpy(call->status->sqlstate, "XX000", sizeof(call->status->sqlstate));
		strlcpy(call->status->message, message, sizeof(call->status->message));
	}
	return TESS_ERROR_INVALID_ARGUMENT;
}

static bool
numeric_call_valid(const TessFunctionCall *call, int nargs)
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

/* The comparisons a call remembers, by the pair of pointers. */
#define COMPARE_CACHE_SIZE 16

typedef struct ComparePair
{
	Datum		left;
	Datum		right;
	int32		result;
	bool		valid;
} ComparePair;

/*
 * numeric =, <>, <, <=, > and >= of columns or a column and a scalar,
 * NaN equal to itself and above every number as the core orders it.
 */
static TessStatusCode
numeric_compare_evaluate(TessFunctionCall *call)
{
	NumericOp	op;
	ComparePair cache[COMPARE_CACHE_SIZE] = {{0}};
	int			nwords;

	if (!numeric_call_valid(call, 2))
		return numeric_invalid(call, "a numeric comparison takes two arguments");
	op = numeric_op(call);
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		keep = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			Datum		left;
			Datum		right;
			ComparePair *pair;
			int32		last;
			bool		result;

			if (arg_null(&call->args[0], row) || arg_null(&call->args[1], row))
				continue;
			left = arg_datum(&call->args[0], row);
			right = arg_datum(&call->args[1], row);
			/* A table's values rarely repeat a pointer: only the cache's are kept. */
			if (!small_numeric(left) && !small_numeric(right))
				last = DatumGetInt32(DirectFunctionCall2(numeric_cmp, left, right));
			else
			{
				/* Slots are 16 bytes apart: the bits above tell them apart. */
				pair = &cache[((left ^ right) >> 4) % COMPARE_CACHE_SIZE];
				if (!pair->valid || pair->left != left || pair->right != right)
				{
					pair->result = DatumGetInt32(DirectFunctionCall2(numeric_cmp, left, right));
					pair->left = left;
					pair->right = right;
					pair->valid = true;
				}
				last = pair->result;
			}
			switch (op)
			{
				case NUMERIC_EQ:
					result = last == 0;
					break;
				case NUMERIC_NE:
					result = last != 0;
					break;
				case NUMERIC_LT:
					result = last < 0;
					break;
				case NUMERIC_LE:
					result = last <= 0;
					break;
				case NUMERIC_GT:
					result = last > 0;
					break;
				default:
					result = last >= 0;
					break;
			}
			if (result)
				keep |= UINT64CONST(1) << bit;
		}
		call->rows->bits[word] = keep;
	}
	return TESS_OK;
}

/* numeric(int2), numeric(int4) and numeric(int8) of a column. */
static TessStatusCode
numeric_cast_evaluate(TessFunctionCall *call)
{
	const TessDatumColumn *column;
	bool		wide;
	Datum	   *values;
	int			nwords;

	if (!numeric_call_valid(call, 1) || call->values == NULL ||
		call->non_nulls == NULL || call->context == NULL ||
		call->args[0].column == NULL)
		return numeric_invalid(call, "a numeric cast takes a column");
	column = call->args[0].column;
	wide = numeric_op(call) == NUMERIC_FROM_INT8;
	values = (Datum *) call->values;
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		present = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			int64		value;

			if (column->isnull[row])
				continue;
			value = wide ? DatumGetInt64(column->values[row]) :
				DatumGetInt32(column->values[row]);
			values[row] = NumericGetDatum(tess_numeric_from_int64(value, call->context));
			present |= UINT64CONST(1) << bit;
		}
		call->non_nulls->bits[word] = present;
	}
	return TESS_OK;
}

/* Register the numeric functions. */
void
tess_register_numeric_functions(const TessFunctionRegistryOps *functions)
{
	for (int i = 0; i < lengthof(numeric_functions); i++)
		functions->add(&numeric_functions[i].function);
}

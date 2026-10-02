/*
 * The numeric functions as batch functions: the six comparisons, + - *
 * and negation and abs, the casts of integers to numeric and of numeric
 * to int4 and int8.
 *
 * A value of at most 18 digits, which a column of numeric(18, s) or less
 * always holds, is a decimal (tessera/decimal.h): the Rust kernels read it
 * from the stored form without detoasting a short varlena, compare and
 * compute a batch of them exactly, and write a result of at most 18
 * digits as make_result writes it, into the call's context, or leave it a
 * decimal where the call asks for them. A row they leave (NaN, an
 * infinity, a longer value or result) goes to the core's function
 * (numeric_cmp, numeric_add, ...), whose comparison a call keeps for the
 * pairs of pointers it met when one is a numeric of the cache below
 * (which repeats as its pointer).
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
#include "tessera/decimal.h"

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
	NUMERIC_FROM_INT8,
	NUMERIC_ADD,
	NUMERIC_SUB,
	NUMERIC_MUL,
	NUMERIC_NEGATE,
	NUMERIC_ABS,
	NUMERIC_TO_INT4,
	NUMERIC_TO_INT8
} NumericOp;

typedef struct NumericFunction
{
	TessFunction function;
	NumericOp	op;
} NumericFunction;

static TessStatusCode numeric_compare_evaluate(TessFunctionCall *call);
static TessStatusCode numeric_cast_evaluate(TessFunctionCall *call);
static TessStatusCode numeric_value_evaluate(TessFunctionCall *call);

#define NUMERIC_FLAGS \
	(TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE | TESS_FUNCTION_ANY_SHAPE | \
	 TESS_FUNCTION_DECIMALS)

#define NUMERIC_COMPARE(oid, code) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_PREDICATE, \
	  .result_format = TESS_RESULT_DATUM, .flags = NUMERIC_FLAGS, \
	  .evaluate = numeric_compare_evaluate}, (code)}

#define NUMERIC_VALUE(oid, code, format) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_VALUE, \
	  .result_format = (format), .flags = NUMERIC_FLAGS, \
	  .evaluate = numeric_value_evaluate}, (code)}

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
	NUMERIC_VALUE(F_NUMERIC_ADD, NUMERIC_ADD, TESS_RESULT_DATUM),
	NUMERIC_VALUE(F_NUMERIC_SUB, NUMERIC_SUB, TESS_RESULT_DATUM),
	NUMERIC_VALUE(F_NUMERIC_MUL, NUMERIC_MUL, TESS_RESULT_DATUM),
	NUMERIC_VALUE(F_NUMERIC_UMINUS, NUMERIC_NEGATE, TESS_RESULT_DATUM),
	NUMERIC_VALUE(F_NUMERIC_ABS, NUMERIC_ABS, TESS_RESULT_DATUM),
	NUMERIC_VALUE(F_INT4_NUMERIC, NUMERIC_TO_INT4, TESS_RESULT_INT32),
	NUMERIC_VALUE(F_INT8_NUMERIC, NUMERIC_TO_INT8, TESS_RESULT_DATUM),
};

/*
 * The small integers the cache keeps (days, months, years, and around),
 * each in a slot of the block: a numeric below 10000 takes 8 bytes.
 */
/* 10^TESS_DECIMAL_DIGITS: the least magnitude a decimal cannot hold. */
#define DECIMAL_LIMIT INT64CONST(1000000000000000000)
StaticAssertDecl(TESS_DECIMAL_DIGITS == 18, "DECIMAL_LIMIT is 10^18");

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

/*
 * Scratch of a call: masks and scales on the stack for a batch of up to
 * this many rows, from the call's context (or the current one) beyond.
 */
#define SCRATCH_ROWS 2048

typedef struct Scratch
{
	uint64		words[3][SCRATCH_ROWS / 64];
	uint8		scales[SCRATCH_ROWS];
} Scratch;

static TessDecimalArg
decimal_arg(const TessFunctionArg *arg)
{
	return (TessDecimalArg) {arg->column, arg->scalar};
}

/* A numeric made on the stack, for the core's function to read. */
typedef union NumericBuffer
{
	char		bytes[TESS_DECIMAL_NUMERIC_MAX];
	int64		align;
} NumericBuffer;

/* A row's value as a numeric Datum: a decimal of the column's written into buffer. */
static Datum
arg_numeric(const TessFunctionArg *arg, int row, NumericBuffer *buffer)
{
	const uint64 *decimals;
	TessStatus	status = {.struct_size = sizeof(TessStatus)};
	Size		size;

	if (arg->column == NULL)
		return arg->scalar;
	decimals = tess_column_decimal_rows(arg->column);
	if (decimals == NULL || ((decimals[row / 64] >> (row % 64)) & 1) == 0)
		return arg->column->values[row];
	if (tess_decimal_write_datum(DatumGetInt64(arg->column->values[row]),
								 arg->column->decimal_scale, buffer->bytes,
								 sizeof(buffer->bytes), &size, &status) != TESS_OK)
		elog(ERROR, "Tessera could not write a decimal: %s", status.message);
	return PointerGetDatum(buffer->bytes);
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

static bool
compare_holds(NumericOp op, int32 order)
{
	switch (op)
	{
		case NUMERIC_EQ:
			return order == 0;
		case NUMERIC_NE:
			return order != 0;
		case NUMERIC_LT:
			return order < 0;
		case NUMERIC_LE:
			return order <= 0;
		case NUMERIC_GT:
			return order > 0;
		default:
			return order >= 0;
	}
}

/*
 * numeric =, <>, <, <=, > and >= of columns or a column and a scalar:
 * the decimals by the kernels, the rest by numeric_cmp, NaN equal to
 * itself and above every number as the core orders it.
 */
static TessStatusCode
numeric_compare_evaluate(TessFunctionCall *call)
{
	static const TessCompareOp compares[] = {
		[NUMERIC_EQ] = TESS_CMP_EQ, [NUMERIC_NE] = TESS_CMP_NE,
		[NUMERIC_LT] = TESS_CMP_LT, [NUMERIC_LE] = TESS_CMP_LE,
		[NUMERIC_GT] = TESS_CMP_GT, [NUMERIC_GE] = TESS_CMP_GE,
	};
	NumericOp	op;
	ComparePair cache[COMPARE_CACHE_SIZE] = {{0}};
	TessDecimalArg args[2];
	Scratch		space;
	TessRowMask rest;
	TessStatusCode code;
	int			nwords;

	if (!numeric_call_valid(call, 2))
		return tess_call_invalid(call, "a numeric comparison takes two arguments");
	op = numeric_op(call);
	args[0] = decimal_arg(&call->args[0]);
	args[1] = decimal_arg(&call->args[1]);
	rest = tess_scratch_mask(space.words[0], sizeof(space.words[0]), call->rows->nrows);
	code = tess_decimal_filter(compares[op], &args[0], &args[1], call->rows, &rest,
							   call->status);
	if (code != TESS_OK)
	{
		tess_scratch_release(rest.bits, space.words[0]);
		return code;
	}
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		for (uint64 look = rest.bits[word]; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			NumericBuffer left_buffer;
			NumericBuffer right_buffer;
			Datum		left = arg_numeric(&call->args[0], row, &left_buffer);
			Datum		right = arg_numeric(&call->args[1], row, &right_buffer);
			int32		order;

			/* A table's values rarely repeat a pointer: only the cache's are kept. */
			if (!small_numeric(left) && !small_numeric(right))
				order = DatumGetInt32(DirectFunctionCall2(numeric_cmp, left, right));
			else
			{
				/* Slots are 16 bytes apart: the bits above tell them apart. */
				ComparePair *pair = &cache[((left ^ right) >> 4) % COMPARE_CACHE_SIZE];

				if (!pair->valid || pair->left != left || pair->right != right)
				{
					pair->result = DatumGetInt32(DirectFunctionCall2(numeric_cmp, left, right));
					pair->left = left;
					pair->right = right;
					pair->valid = true;
				}
				order = pair->result;
			}
			if (compare_holds(op, order))
				call->rows->bits[word] |= UINT64CONST(1) << bit;
		}
	}
	tess_scratch_release(rest.bits, space.words[0]);
	return TESS_OK;
}

/*
 * numeric(int2), numeric(int4) and numeric(int8) of a column: decimals of
 * scale 0 where the call asks, else numerics.
 */
static TessStatusCode
numeric_cast_evaluate(TessFunctionCall *call)
{
	const TessDatumColumn *column;
	bool		wide;
	Datum	   *values;
	uint64	   *decimals = NULL;
	int			nwords;

	if (!numeric_call_valid(call, 1) || call->values == NULL ||
		call->non_nulls == NULL || call->context == NULL ||
		call->args[0].column == NULL)
		return tess_call_invalid(call, "a numeric cast takes a column");
	column = call->args[0].column;
	wide = numeric_op(call) == NUMERIC_FROM_INT8;
	values = (Datum *) call->values;
	if (call->struct_size >= TESS_FUNCTION_CALL_DECIMALS_SIZE && call->decimal_rows != NULL &&
		call->result_scale == 0)
		decimals = call->decimal_rows->bits;
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
			/*
			 * A decimal holds at most TESS_DECIMAL_DIGITS digits: an int8
			 * of 19 stays a numeric, as the rows the decimals do not take.
			 */
			if (decimals != NULL &&
				value > -DECIMAL_LIMIT && value < DECIMAL_LIMIT)
			{
				values[row] = Int64GetDatum(value);
				decimals[word] |= UINT64CONST(1) << bit;
			}
			else
				values[row] = NumericGetDatum(tess_numeric_from_int64(value, call->context));
			present |= UINT64CONST(1) << bit;
		}
		call->non_nulls->bits[word] = present;
	}
	return TESS_OK;
}

/* The core's function of the operation, for a row the decimals do not take. */
static Datum
numeric_core(NumericOp op, Datum left, Datum right)
{
	switch (op)
	{
		case NUMERIC_ADD:
			return DirectFunctionCall2(numeric_add, left, right);
		case NUMERIC_SUB:
			return DirectFunctionCall2(numeric_sub, left, right);
		case NUMERIC_MUL:
			return DirectFunctionCall2(numeric_mul, left, right);
		case NUMERIC_NEGATE:
			return DirectFunctionCall1(numeric_uminus, left);
		case NUMERIC_ABS:
			return DirectFunctionCall1(numeric_abs, left);
		case NUMERIC_TO_INT4:
			return DirectFunctionCall1(numeric_int4, left);
		default:
			return DirectFunctionCall1(numeric_int8, left);
	}
}

TessStatusCode
tess_numeric_results(MemoryContext context, Datum *values, const uint8 *scales,
					 TessRowMask *write, TessStatus *status)
{
	int			nwords = tess_row_mask_word_count(write->nrows);
	uint64		count = 0;
	char	   *space;
	Size		used;

	for (int word = 0; word < nwords; word++)
	{
		for (uint64 look = write->bits[word]; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			int64		value = DatumGetInt64(values[row]);

			if (scales[row] == 0 && value >= SMALL_NUMERIC_MIN && value < SMALL_NUMERIC_MAX)
			{
				values[row] = NumericGetDatum(tess_numeric_from_int64(value, context));
				write->bits[word] &= ~(UINT64CONST(1) << bit);
			}
		}
		count += pg_popcount64(write->bits[word]);
	}
	if (count == 0)
		return TESS_OK;
	space = MemoryContextAlloc(context, count * TESS_DECIMAL_NUMERIC_MAX);
	return tess_decimal_write(values, scales, -1, write, space,
							  count * TESS_DECIMAL_NUMERIC_MAX, &used, status);
}

/*
 * numeric + - * of any shape, unary - and abs, int4(numeric) and
 * int8(numeric): the decimals by the kernels where both arguments are
 * decimals and the result fits, the core's function in the call's context
 * for the rest.
 */
static TessStatusCode
numeric_value_evaluate(TessFunctionCall *call)
{
	static const TessDecimalOp operations[] = {
		[NUMERIC_ADD] = TESS_DECIMAL_ADD, [NUMERIC_SUB] = TESS_DECIMAL_SUB,
		[NUMERIC_MUL] = TESS_DECIMAL_MUL, [NUMERIC_NEGATE] = TESS_DECIMAL_NEGATE,
		[NUMERIC_ABS] = TESS_DECIMAL_ABS,
	};
	NumericOp	op;
	int			nargs;
	int			nrows;
	TessDecimalArg args[2];
	Scratch		space;
	TessRowMask rest;
	TessStatusCode code;
	MemoryContext old;
	int			nwords;

	if (call == NULL || call->function == NULL)
		return tess_call_invalid(call, "a numeric function takes its arguments");
	op = numeric_op(call);
	nargs = op == NUMERIC_ADD || op == NUMERIC_SUB || op == NUMERIC_MUL ? 2 : 1;
	if (!numeric_call_valid(call, nargs) || call->values == NULL ||
		call->non_nulls == NULL || call->context == NULL)
		return tess_call_invalid(call, "a numeric function takes its arguments");
	args[0] = decimal_arg(&call->args[0]);
	args[1] = nargs == 2 ? decimal_arg(&call->args[1]) : args[0];
	nrows = call->rows->nrows;
	rest = tess_scratch_mask(space.words[0], sizeof(space.words[0]), nrows);
	if (op == NUMERIC_TO_INT4)
		code = tess_decimal_to_int4(&args[0], call->rows, (int32 *) call->values,
									call->non_nulls, &rest, call->status);
	else if (op == NUMERIC_TO_INT8)
		code = tess_decimal_to_int8(&args[0], call->rows, (int64 *) call->values,
									call->non_nulls, &rest, call->status);
	else
	{
		bool		asks = call->struct_size >= TESS_FUNCTION_CALL_DECIMALS_SIZE &&
			call->decimal_rows != NULL;
		TessRowMask decimals = asks ? *call->decimal_rows :
			tess_scratch_mask(space.words[1], sizeof(space.words[1]), nrows);
		TessRowMask write = tess_scratch_mask(space.words[2], sizeof(space.words[2]), nrows);
		uint8	   *scales = tess_scratch_alloc(nrows, space.scales, sizeof(space.scales));

		code = tess_decimal_compute(operations[op], &args[0], &args[1], call->rows,
									asks ? call->result_scale : -1,
									(int64 *) call->values, scales, call->non_nulls,
									&decimals, &rest, call->status);
		if (code == TESS_OK)
		{
			for (int word = 0; word < tess_row_mask_word_count(nrows); word++)
				write.bits[word] = call->non_nulls->bits[word] & ~rest.bits[word] &
					~decimals.bits[word];
			code = tess_numeric_results(call->context, (Datum *) call->values, scales, &write,
										call->status);
		}
		tess_scratch_release(scales, space.scales);
		tess_scratch_release(write.bits, space.words[2]);
		if (!asks)
			tess_scratch_release(decimals.bits, space.words[1]);
	}
	if (code != TESS_OK)
	{
		tess_scratch_release(rest.bits, space.words[0]);
		return code;
	}
	old = MemoryContextSwitchTo(call->context);
	nwords = tess_row_mask_word_count(nrows);
	for (int word = 0; word < nwords; word++)
	{
		for (uint64 look = rest.bits[word]; look != 0; look &= look - 1)
		{
			int			row = word * 64 + pg_rightmost_one_pos64(look);
			NumericBuffer left_buffer;
			NumericBuffer right_buffer;
			Datum		result;

			result = numeric_core(op, arg_numeric(&call->args[0], row, &left_buffer),
								  nargs == 2 ? arg_numeric(&call->args[1], row, &right_buffer) :
								  (Datum) 0);
			if (op == NUMERIC_TO_INT4)
				((int32 *) call->values)[row] = DatumGetInt32(result);
			else
				((Datum *) call->values)[row] = result;
		}
	}
	MemoryContextSwitchTo(old);
	tess_scratch_release(rest.bits, space.words[0]);
	return TESS_OK;
}

/* Register the numeric functions. */
void
tess_register_numeric_functions(const TessFunctionRegistryOps *functions)
{
	for (int i = 0; i < lengthof(numeric_functions); i++)
		functions->add(&numeric_functions[i].function);
}

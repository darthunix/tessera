/*
 * The numeric functions as batch functions, row by row in C over the
 * Datums: the six comparisons, + - * and negation and abs, the casts of
 * integers to numeric and of numeric to int4 and int8.
 *
 * A value of at most 18 digits, which a column of numeric(18, s) or less
 * always holds, is read as a decimal: an int64 of its display scale, from
 * the stored header and digits of base 10000 without detoasting a short
 * varlena. Two decimals compare in int128 at the larger scale; + and -
 * keep the larger scale and * the sum of scales, as the core's add_var
 * and mul_var, and a result of at most 18 digits is written as
 * make_result writes it (leading and trailing zero digits dropped, the
 * short header) into blocks of the call's context. A NaN, an infinity, a
 * longer value or a longer result goes to the core's function a row
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
#include "common/int.h"
#include "common/int128.h"
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

/* A decimal at a larger scale, exactly: at most 36 digits fit int128. */
static inline INT128
decimal_at(TessDecimal decimal, int scale)
{
	INT128		result = int64_to_int128(0);

	int128_add_int64_mul_int64(&result, decimal.value,
							   tess_powers_of_ten[scale - decimal.scale]);
	return result;
}

/* Memory for a call's results, carved from blocks of its context. */
typedef struct NumericArena
{
	MemoryContext context;
	char	   *next;
	Size		left;
} NumericArena;

static void *
arena_alloc(NumericArena *arena, Size size)
{
	void	   *result;

	size = MAXALIGN(size);
	if (size > arena->left)
	{
		Size		block = Max(size, 8192);

		arena->next = MemoryContextAlloc(arena->context, block);
		arena->left = block;
	}
	result = arena->next;
	arena->next += size;
	arena->left -= size;
	return result;
}

/*
 * The numeric of value / 10^scale with that display scale, as make_result
 * writes it (tess_decimal_write_numeric), in the arena: a small integer
 * from the process's cache.
 */
static Datum
numeric_of(int64 value, int scale, NumericArena *arena)
{
	char	   *result;
	Size		size;

	if (scale == 0)
		return NumericGetDatum(tess_numeric_from_int64(value, arena->context));
	result = arena_alloc(arena, TESS_DECIMAL_NUMERIC_MAX);
	size = MAXALIGN(tess_decimal_write_numeric(value, scale, result));
	/* The last block's unused tail goes back to the arena. */
	arena->next -= TESS_DECIMAL_NUMERIC_MAX - size;
	arena->left += TESS_DECIMAL_NUMERIC_MAX - size;
	return PointerGetDatum(result);
}

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

/*
 * An argument of a call: a column, with the rows whose Datum is a decimal
 * when it has them, or a scalar, read as a decimal once.
 */
typedef struct NumericArg
{
	const TessDatumColumn *column;
	const uint64 *decimal_rows;
	int			decimal_scale;
	Datum		scalar;
	bool		scalar_decimal;
	TessDecimal scalar_value;
} NumericArg;

/* A numeric made on the stack, for the core's function to read. */
typedef union NumericBuffer
{
	char		bytes[TESS_DECIMAL_NUMERIC_MAX];
	int64		align;
} NumericBuffer;

static void
numeric_arg(const TessFunctionArg *arg, NumericArg *result)
{
	result->column = arg->column;
	result->decimal_rows = arg->column != NULL ? tess_column_decimal_rows(arg->column) : NULL;
	result->decimal_scale = result->decimal_rows != NULL ? arg->column->decimal_scale : 0;
	result->scalar = arg->scalar;
	result->scalar_decimal = arg->column == NULL &&
		tess_decimal_of(arg->scalar, &result->scalar_value);
}

static inline bool
arg_null(const NumericArg *arg, int row)
{
	return arg->column != NULL && arg->column->isnull[row];
}

static inline bool
arg_is_decimal(const NumericArg *arg, int row)
{
	return arg->decimal_rows != NULL && ((arg->decimal_rows[row / 64] >> (row % 64)) & 1) != 0;
}

/* A row's value as a decimal: the column's own, its numeric read, the scalar's. */
static inline bool
arg_decimal(const NumericArg *arg, int row, TessDecimal *decimal)
{
	if (arg->column == NULL)
	{
		*decimal = arg->scalar_value;
		return arg->scalar_decimal;
	}
	if (arg_is_decimal(arg, row))
	{
		decimal->value = DatumGetInt64(arg->column->values[row]);
		decimal->scale = arg->decimal_scale;
		return true;
	}
	return tess_decimal_of(arg->column->values[row], decimal);
}

/* A row's value as a numeric Datum: a decimal written into buffer. */
static inline Datum
arg_numeric(const NumericArg *arg, int row, NumericBuffer *buffer)
{
	if (arg->column == NULL)
		return arg->scalar;
	if (arg_is_decimal(arg, row))
	{
		(void) tess_decimal_write_numeric(DatumGetInt64(arg->column->values[row]),
										  arg->decimal_scale, buffer->bytes);
		return PointerGetDatum(buffer->bytes);
	}
	return arg->column->values[row];
}

/* Where a call writes its results: numerics, or decimals where it asks. */
typedef struct NumericResults
{
	Datum	   *values;
	uint64	   *decimal_bits;
	int			scale;
	NumericArena arena;
} NumericResults;

static void
results_init(TessFunctionCall *call, NumericResults *out)
{
	out->values = (Datum *) call->values;
	out->decimal_bits = NULL;
	out->scale = 0;
	if (call->struct_size >= TESS_FUNCTION_CALL_DECIMALS_SIZE && call->decimal_rows != NULL)
	{
		out->decimal_bits = call->decimal_rows->bits;
		out->scale = call->result_scale;
	}
	out->arena.context = call->context;
	out->arena.next = NULL;
	out->arena.left = 0;
}

/*
 * A row's exact result at a scale: a decimal where the call takes one of
 * that scale, else its numeric; false past 18 digits, for the core's
 * function.
 */
static inline bool
out_result(NumericResults *out, int row, INT128 value, int scale)
{
	int64		result;

	if (int128_compare(value, int64_to_int128(-tess_powers_of_ten[TESS_DECIMAL_DIGITS] + 1)) < 0 ||
		int128_compare(value, int64_to_int128(tess_powers_of_ten[TESS_DECIMAL_DIGITS] - 1)) > 0)
		return false;
	result = int128_to_int64(value);
	if (out->decimal_bits != NULL && scale == out->scale)
	{
		out->values[row] = Int64GetDatum(result);
		out->decimal_bits[row / 64] |= UINT64CONST(1) << (row % 64);
	}
	else
		out->values[row] = numeric_of(result, scale, &out->arena);
	return true;
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
	NumericArg	args[2];
	int			nwords;

	if (!numeric_call_valid(call, 2))
		return numeric_invalid(call, "a numeric comparison takes two arguments");
	op = numeric_op(call);
	numeric_arg(&call->args[0], &args[0]);
	numeric_arg(&call->args[1], &args[1]);
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		keep = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			TessDecimal left_decimal;
			TessDecimal right_decimal;
			int32		last;
			bool		result;

			if (arg_null(&args[0], row) || arg_null(&args[1], row))
				continue;
			if (arg_decimal(&args[0], row, &left_decimal) &&
				arg_decimal(&args[1], row, &right_decimal))
			{
				int			scale = Max(left_decimal.scale, right_decimal.scale);

				last = int128_compare(decimal_at(left_decimal, scale),
									  decimal_at(right_decimal, scale));
			}
			else
			{
				NumericBuffer left_buffer;
				NumericBuffer right_buffer;
				Datum		left = arg_numeric(&args[0], row, &left_buffer);
				Datum		right = arg_numeric(&args[1], row, &right_buffer);

				/* A table's values rarely repeat a pointer: only the cache's are kept. */
				if (!small_numeric(left) && !small_numeric(right))
					last = DatumGetInt32(DirectFunctionCall2(numeric_cmp, left, right));
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
					last = pair->result;
				}
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

/*
 * numeric(int2), numeric(int4) and numeric(int8) of a column: decimals of
 * scale 0 where the call asks, else numerics.
 */
static TessStatusCode
numeric_cast_evaluate(TessFunctionCall *call)
{
	const TessDatumColumn *column;
	bool		wide;
	NumericResults	out;
	int			nwords;

	if (!numeric_call_valid(call, 1) || call->values == NULL ||
		call->non_nulls == NULL || call->context == NULL ||
		call->args[0].column == NULL)
		return numeric_invalid(call, "a numeric cast takes a column");
	column = call->args[0].column;
	wide = numeric_op(call) == NUMERIC_FROM_INT8;
	results_init(call, &out);
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
			if (out.decimal_bits != NULL && out.scale == 0)
			{
				out.values[row] = Int64GetDatum(value);
				out.decimal_bits[word] |= UINT64CONST(1) << bit;
			}
			else
				out.values[row] = NumericGetDatum(tess_numeric_from_int64(value, call->context));
			present |= UINT64CONST(1) << bit;
		}
		call->non_nulls->bits[word] = present;
	}
	return TESS_OK;
}

/*
 * One row's result by the decimals: + and - at the larger scale, * at the
 * sum of scales, as add_var and mul_var; int4 and int8 rounded half away
 * from zero, as round_var. False for the core's function.
 */
static inline bool
decimal_value(NumericOp op, TessDecimal left, TessDecimal right, NumericResults *out, int row)
{
	INT128		value = int64_to_int128(0);
	int			scale;

	switch (op)
	{
		case NUMERIC_ADD:
		case NUMERIC_SUB:
			scale = Max(left.scale, right.scale);
			int128_add_int64_mul_int64(&value, left.value, tess_powers_of_ten[scale - left.scale]);
			if (op == NUMERIC_ADD)
				int128_add_int64_mul_int64(&value, right.value,
										   tess_powers_of_ten[scale - right.scale]);
			else
				int128_sub_int64_mul_int64(&value, right.value,
										   tess_powers_of_ten[scale - right.scale]);
			return out_result(out, row, value, scale);
		case NUMERIC_MUL:
			int128_add_int64_mul_int64(&value, left.value, right.value);
			return out_result(out, row, value, left.scale + right.scale);
		case NUMERIC_NEGATE:
			int128_sub_int64_mul_int64(&value, left.value, 1);
			return out_result(out, row, value, left.scale);
		case NUMERIC_ABS:
			if (left.value < 0)
				int128_sub_int64_mul_int64(&value, left.value, 1);
			else
				value = int64_to_int128(left.value);
			return out_result(out, row, value, left.scale);
		default:
			{
				int64		unit = tess_powers_of_ten[left.scale];
				int64		whole = left.value / unit;
				int64		rest = left.value % unit;

				if (rest * 2 >= unit)
					whole++;
				else if (rest * 2 <= -unit)
					whole--;
				if (op == NUMERIC_TO_INT4)
				{
					if (whole < PG_INT32_MIN || whole > PG_INT32_MAX)
						return false;
					((int32 *) out->values)[row] = (int32) whole;
				}
				else
					out->values[row] = Int64GetDatum(whole);
				return true;
			}
	}
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

/*
 * numeric + - * of any shape, unary - and abs, int4(numeric) and
 * int8(numeric): the decimals where both arguments are and the result
 * fits, the core's function in the call's context otherwise.
 */
static TessStatusCode
numeric_value_evaluate(TessFunctionCall *call)
{
	NumericOp	op;
	int			nargs;
	NumericArg	args[2];
	NumericResults	out;
	MemoryContext old;
	int			nwords;

	if (call == NULL || call->function == NULL)
		return numeric_invalid(call, "a numeric function takes its arguments");
	op = numeric_op(call);
	nargs = op == NUMERIC_ADD || op == NUMERIC_SUB || op == NUMERIC_MUL ? 2 : 1;
	if (!numeric_call_valid(call, nargs) || call->values == NULL ||
		call->non_nulls == NULL || call->context == NULL)
		return numeric_invalid(call, "a numeric function takes its arguments");
	numeric_arg(&call->args[0], &args[0]);
	if (nargs == 2)
		numeric_arg(&call->args[1], &args[1]);
	results_init(call, &out);
	old = MemoryContextSwitchTo(call->context);
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		present = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			TessDecimal left = {0};
			TessDecimal right = {0};

			if (arg_null(&args[0], row) || (nargs == 2 && arg_null(&args[1], row)))
				continue;
			if (!arg_decimal(&args[0], row, &left) ||
				(nargs == 2 && !arg_decimal(&args[1], row, &right)) ||
				!decimal_value(op, left, right, &out, row))
			{
				NumericBuffer left_buffer;
				NumericBuffer right_buffer;
				Datum		result;

				result = numeric_core(op, arg_numeric(&args[0], row, &left_buffer),
									  nargs == 2 ? arg_numeric(&args[1], row, &right_buffer) :
									  (Datum) 0);
				if (op == NUMERIC_TO_INT4)
					((int32 *) call->values)[row] = DatumGetInt32(result);
				else
					out.values[row] = result;
			}
			present |= UINT64CONST(1) << bit;
		}
		call->non_nulls->bits[word] = present;
	}
	MemoryContextSwitchTo(old);
	return TESS_OK;
}

/* Register the numeric functions. */
void
tess_register_numeric_functions(const TessFunctionRegistryOps *functions)
{
	for (int i = 0; i < lengthof(numeric_functions); i++)
		functions->add(&numeric_functions[i].function);
}

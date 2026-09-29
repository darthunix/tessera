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
	(TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE | TESS_FUNCTION_ANY_SHAPE)

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
 * The numeric of value / 10^scale (at most 18 digits, a scale up to 36)
 * with that display scale, as make_result writes it: the digits of base
 * 10000 aligned to the decimal point, leading and trailing zero digits
 * dropped, zero positive at weight 0, the short header, which such a
 * weight and scale always fit.
 */
static Datum
numeric_of(int64 value, int scale, NumericArena *arena)
{
	int16		digits[16];
	int			ndigits = 0;
	int			first;
	int			weight;
	int			part = scale % 4;
	int			pad = (4 - part) % 4;
	uint64		magnitude = value < 0 ? -(uint64) value : (uint64) value;
	Numeric		result;

	if (scale == 0)
		return NumericGetDatum(tess_numeric_from_int64(value, arena->context));
	/*
	 * Digits from the lowest: first the fraction's partial group, its part
	 * digits padded to four, then whole groups; the lowest (scale + pad) / 4
	 * are the fraction's.
	 */
	if (part != 0)
	{
		digits[ndigits++] = (int16) ((magnitude % tess_powers_of_ten[part]) * tess_powers_of_ten[pad]);
		magnitude /= tess_powers_of_ten[part];
	}
	while (magnitude != 0)
	{
		digits[ndigits++] = (int16) (magnitude % 10000);
		magnitude /= 10000;
	}
	/* A partial group of zeros under nothing else: the value is zero. */
	while (ndigits > 0 && digits[ndigits - 1] == 0)
		ndigits--;
	weight = ndigits - (scale + pad) / 4 - 1;
	/* Trailing zero digits are the lowest: skip them. */
	first = 0;
	while (first < ndigits && digits[first] == 0)
		first++;
	if (first == ndigits)
	{
		ndigits = 0;
		weight = 0;
	}
	result = (Numeric) arena_alloc(arena, VARHDRSZ + sizeof(uint16) +
								   (ndigits - first) * sizeof(int16));
	SET_VARSIZE(result, VARHDRSZ + sizeof(uint16) + (ndigits - first) * sizeof(int16));
	{
		uint16		header = 0x8000 | (value < 0 && ndigits > 0 ? 0x2000 : 0) |
			(scale << 7) | (weight < 0 ? 0x0040 : 0) | (weight & 0x003F);
		char	   *data = (char *) result + VARHDRSZ;

		memcpy(data, &header, sizeof(uint16));
		/* The highest digit first. */
		for (int at = ndigits - 1, out = 0; at >= first; at--, out++)
			memcpy(data + sizeof(uint16) + out * sizeof(int16), &digits[at], sizeof(int16));
	}
	return NumericGetDatum(result);
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
	TessDecimal		scalars[2];
	bool		decimal_scalar[2] = {false, false};
	int			nwords;

	if (!numeric_call_valid(call, 2))
		return numeric_invalid(call, "a numeric comparison takes two arguments");
	op = numeric_op(call);
	for (int arg = 0; arg < 2; arg++)
		if (call->args[arg].column == NULL)
			decimal_scalar[arg] = tess_decimal_of(call->args[arg].scalar, &scalars[arg]);
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
			if ((call->args[0].column != NULL ? tess_decimal_of(left, &scalars[0]) : decimal_scalar[0]) &&
				(call->args[1].column != NULL ? tess_decimal_of(right, &scalars[1]) : decimal_scalar[1]))
			{
				int			scale = Max(scalars[0].scale, scalars[1].scale);

				last = int128_compare(decimal_at(scalars[0], scale),
									  decimal_at(scalars[1], scale));
			}
			/* A table's values rarely repeat a pointer: only the cache's are kept. */
			else if (!small_numeric(left) && !small_numeric(right))
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

/* One row's result by the decimals, or false for the core's function. */
static bool
decimal_value(NumericOp op, TessDecimal left, TessDecimal right, NumericArena *arena,
			  Datum *result)
{
	INT128		sum = int64_to_int128(0);
	int			scale;

	switch (op)
	{
		case NUMERIC_ADD:
		case NUMERIC_SUB:
			scale = Max(left.scale, right.scale);
			int128_add_int64_mul_int64(&sum, left.value, tess_powers_of_ten[scale - left.scale]);
			if (op == NUMERIC_ADD)
				int128_add_int64_mul_int64(&sum, right.value,
										   tess_powers_of_ten[scale - right.scale]);
			else
				int128_sub_int64_mul_int64(&sum, right.value,
										   tess_powers_of_ten[scale - right.scale]);
			break;
		case NUMERIC_MUL:
			scale = left.scale + right.scale;
			int128_add_int64_mul_int64(&sum, left.value, right.value);
			break;
		case NUMERIC_NEGATE:
			*result = numeric_of(-left.value, left.scale, arena);
			return true;
		case NUMERIC_ABS:
			*result = numeric_of(i64abs(left.value), left.scale, arena);
			return true;
		default:
			{
				/* int4 and int8: rounded half away from zero, as round_var. */
				int64		unit = tess_powers_of_ten[left.scale];
				int64		whole = left.value / unit;
				int64		rest = left.value % unit;

				if (rest * 2 >= unit)
					whole++;
				else if (rest * 2 <= -unit)
					whole--;
				if (op == NUMERIC_TO_INT4 && (whole < PG_INT32_MIN || whole > PG_INT32_MAX))
					return false;
				*result = op == NUMERIC_TO_INT4 ? Int32GetDatum((int32) whole) :
					Int64GetDatum(whole);
				return true;
			}
	}
	/* A result of at most 18 digits, as a numeric of that scale. */
	if (int128_compare(sum, int64_to_int128(-tess_powers_of_ten[TESS_DECIMAL_DIGITS] + 1)) < 0 ||
		int128_compare(sum, int64_to_int128(tess_powers_of_ten[TESS_DECIMAL_DIGITS] - 1)) > 0)
		return false;
	*result = numeric_of(int128_to_int64(sum), scale, arena);
	return true;
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
 * int8(numeric): the decimals where both arguments are, the core's
 * function in the call's context otherwise.
 */
static TessStatusCode
numeric_value_evaluate(TessFunctionCall *call)
{
	NumericOp	op;
	int			nargs;
	TessDecimal		scalars[2] = {{0}};
	bool		decimal_scalar[2] = {false, false};
	NumericArena arena = {0};
	MemoryContext old;
	int			nwords;

	if (call == NULL || call->function == NULL)
		return numeric_invalid(call, "a numeric function takes its arguments");
	op = numeric_op(call);
	nargs = op == NUMERIC_ADD || op == NUMERIC_SUB || op == NUMERIC_MUL ? 2 : 1;
	if (!numeric_call_valid(call, nargs) || call->values == NULL ||
		call->non_nulls == NULL || call->context == NULL)
		return numeric_invalid(call, "a numeric function takes its arguments");
	for (int arg = 0; arg < nargs; arg++)
		if (call->args[arg].column == NULL)
			decimal_scalar[arg] = tess_decimal_of(call->args[arg].scalar, &scalars[arg]);
	arena.context = call->context;
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
			Datum		left;
			Datum		right = (Datum) 0;
			Datum		result;
			bool		decimals;

			if (arg_null(&call->args[0], row) ||
				(nargs == 2 && arg_null(&call->args[1], row)))
				continue;
			left = arg_datum(&call->args[0], row);
			decimals = call->args[0].column != NULL ? tess_decimal_of(left, &scalars[0]) :
				decimal_scalar[0];
			if (nargs == 2)
			{
				right = arg_datum(&call->args[1], row);
				decimals = decimals &&
					(call->args[1].column != NULL ? tess_decimal_of(right, &scalars[1]) :
					 decimal_scalar[1]);
			}
			if (!decimals ||
				!decimal_value(op, scalars[0], scalars[1], &arena, &result))
				result = numeric_core(op, left, right);
			if (op == NUMERIC_TO_INT4)
				((int32 *) call->values)[row] = DatumGetInt32(result);
			else
				((Datum *) call->values)[row] = result;
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

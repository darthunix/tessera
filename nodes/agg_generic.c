/*
 * TessAgg's generic aggregates (AGG_GENERIC): the core's transition,
 * combine and final functions over a batch's rows, and the fast path,
 * the node's own states for sum, avg, min and max of numbers, with the
 * sum states of a GROUP BY. See agg.c.
 */
#include "postgres.h"

#include "access/htup_details.h"
#include "catalog/pg_aggregate.h"
#include "executor/executor.h"
#include "parser/parse_agg.h"
#include "utils/array.h"
#include "utils/float.h"
#include "utils/fmgroids.h"
#include "utils/syscache.h"
#include "utils/lsyscache.h"
#include "utils/datum.h"
#include "utils/builtins.h"
#include "utils/numeric.h"

#include "agg.h"
#include "agg_node.h"

/* A numeric of at most 18 digits (tessera/decimal.h): its value at its display scale. */
typedef struct FastDecimal
{
	int64		value;
	int			scale;
} FastDecimal;

/*
 * The state of such an aggregate, in the states' context. sum and avg: the
 * decimals' sum (numeric values of at most 18 digits, integers at scale 0)
 * at the largest scale met, below 10^36 in magnitude, the count, and the
 * numeric sum of the rest, NaN, infinities, longer values and the
 * decimals' sums past the bound. min and max: a copy of the extreme and its
 * decimal, when it has one. A float's: the count, sum and sum of squared
 * deviations float8_accum keeps (the sum of a float4 in float4), or the
 * extreme.
 */
typedef struct FastState
{
#ifdef HAVE_INT128
	int128		sum;
#endif
	int			scale;
	int64		count;
	bool		has_rest;
	Datum		rest;
	bool		has_extreme;
	Datum		extreme;
	bool		decimal_valid;
	FastDecimal decimal;
	float8		n;
	float8		sx;
	float8		sxx;
	float4		sx4;
} FastState;

/*
 * Whether the node folds an aggregate itself, and over which argument: a
 * whole one, or a partial one whose state the node writes as the final
 * aggregation above reads it: the core's own transition value, for a
 * state that is not internal, or the node's own format (own_states), for
 * the node's final aggregation (fast_partial, agg_sum_state_partial).
 */
static FastKind
fast_kind(const Aggref *agg, GenericAgg *generic, bool own_states)
{
#ifdef HAVE_INT128
	if ((agg->aggsplit != AGGSPLIT_SIMPLE &&
		 !(agg->aggsplit == AGGSPLIT_INITIAL_SERIAL &&
		   (agg->aggtranstype != INTERNALOID || own_states))) ||
		list_length(agg->args) != 1)
		return FAST_NONE;
	generic->fast_numeric = false;
	generic->fast_wide = false;
	generic->fast_int8_result = false;
	generic->fast_float = InvalidOid;
	switch (agg->aggfnoid)
	{
		case F_SUM_FLOAT8:
		case F_AVG_FLOAT8:
		case F_MIN_FLOAT8:
		case F_MAX_FLOAT8:
			generic->fast_float = FLOAT8OID;
			break;
		case F_SUM_FLOAT4:
		case F_AVG_FLOAT4:
		case F_MIN_FLOAT4:
		case F_MAX_FLOAT4:
			generic->fast_float = FLOAT4OID;
			break;
		default:
			break;
	}
	switch (agg->aggfnoid)
	{
		case F_SUM_FLOAT8:
		case F_SUM_FLOAT4:
			return FAST_SUM;
		case F_AVG_FLOAT8:
		case F_AVG_FLOAT4:
			return FAST_AVG;
		case F_MIN_FLOAT8:
		case F_MIN_FLOAT4:
			return FAST_MIN;
		case F_MAX_FLOAT8:
		case F_MAX_FLOAT4:
			return FAST_MAX;
		case F_SUM_NUMERIC:
		case F_AVG_NUMERIC:
		case F_MIN_NUMERIC:
		case F_MAX_NUMERIC:
			/* The decimals are the kernels': without them, the core's functions. */
			generic->kernels = tess_runtime_kernels();
			if (generic->kernels == NULL)
				return FAST_NONE;
			generic->fast_numeric = true;
			return agg->aggfnoid == F_SUM_NUMERIC ? FAST_SUM :
				agg->aggfnoid == F_AVG_NUMERIC ? FAST_AVG :
				agg->aggfnoid == F_MIN_NUMERIC ? FAST_MIN : FAST_MAX;
		case F_SUM_INT8:
			generic->fast_wide = true;
			return FAST_SUM;
		case F_AVG_INT8:
			generic->fast_wide = true;
			return FAST_AVG;
		case F_SUM_INT2:
			generic->fast_int8_result = true;
			return FAST_SUM;
		case F_AVG_INT4:
		case F_AVG_INT2:
			return FAST_AVG;
		default:
			break;
	}
#endif
	return FAST_NONE;
}

#ifdef HAVE_INT128

/* 10^0 through 10^18: a decimal's scale changed exactly. */
static const int64 fast_powers[TESS_DECIMAL_DIGITS + 1] = {
	INT64CONST(1), INT64CONST(10), INT64CONST(100), INT64CONST(1000),
	INT64CONST(10000), INT64CONST(100000), INT64CONST(1000000),
	INT64CONST(10000000), INT64CONST(100000000), INT64CONST(1000000000),
	INT64CONST(10000000000), INT64CONST(100000000000),
	INT64CONST(1000000000000), INT64CONST(10000000000000),
	INT64CONST(100000000000000), INT64CONST(1000000000000000),
	INT64CONST(10000000000000000), INT64CONST(100000000000000000),
	INT64CONST(1000000000000000000)
};

/* The bound of a decimals' sum: 10^36, with room for one more term. */
#define FAST_BOUND ((int128) INT64CONST(1000000000000000000) * INT64CONST(1000000000000000000))

/*
 * The numeric of an int128 at a scale, with that display scale, as the
 * core makes one of an int128 sum: a part of 18 digits and the rest.
 */
static Datum
fast_numeric(int128 value, int scale)
{
	int64		unit = fast_powers[TESS_DECIMAL_DIGITS];

	if (value >= PG_INT64_MIN && value <= PG_INT64_MAX)
		return NumericGetDatum(int64_div_fast_to_numeric((int64) value, scale));
	return DirectFunctionCall2(numeric_add,
							   NumericGetDatum(int64_div_fast_to_numeric((int64) (value / unit),
																		 scale - TESS_DECIMAL_DIGITS)),
							   NumericGetDatum(int64_div_fast_to_numeric((int64) (value % unit),
																		 scale)));
}

/* A numeric added to the rest's sum, in the states' context. */
static void
fast_rest(FastState *fast, Datum value, MemoryContext states)
{
	MemoryContext old = MemoryContextSwitchTo(states);

	if (!fast->has_rest)
		fast->rest = PointerGetDatum(pg_detoast_datum_copy((struct varlena *) DatumGetPointer(value)));
	else
	{
		Datum		sum = DirectFunctionCall2(numeric_add, fast->rest, value);

		pfree(DatumGetPointer(fast->rest));
		fast->rest = sum;
	}
	fast->has_rest = true;
	MemoryContextSwitchTo(old);
}

/* The decimals' sum moved to the rest's, before it passes the bound. */
static void
fast_flush(FastState *fast, MemoryContext states)
{
	MemoryContext old = MemoryContextSwitchTo(states);
	Datum		sum = fast_numeric(fast->sum, fast->scale);

	MemoryContextSwitchTo(old);
	fast_rest(fast, sum, states);
	pfree(DatumGetPointer(sum));
	fast->sum = 0;
}

/*
 * A decimal added at the larger of its scale and the sum's, as the core's
 * accumulation keeps the largest display scale; a sum a larger scale or a
 * term would take past the bound goes to the rest first.
 */
static void
fast_add(FastState *fast, FastDecimal decimal, MemoryContext states)
{
	int128		term = decimal.value;

	if (decimal.scale > fast->scale)
	{
		int128		factor = fast_powers[decimal.scale - fast->scale];

		if (fast->sum >= FAST_BOUND / factor || fast->sum <= -FAST_BOUND / factor)
			fast_flush(fast, states);
		fast->sum *= factor;
		fast->scale = decimal.scale;
	}
	else
		term *= fast_powers[fast->scale - decimal.scale];
	fast->sum += term;
	if (fast->sum >= FAST_BOUND || fast->sum <= -FAST_BOUND)
		fast_flush(fast, states);
}

/*
 * min and max: a value that beats the extreme, or equals it, replaces it,
 * as numeric_smaller and numeric_larger return their second argument on a
 * tie; two decimals compare at the larger scale, anything else by
 * numeric_cmp.
 */
static void
fast_extreme(GenericAgg *generic, FastState *fast, Datum value,
			 const FastDecimal *decimal, MemoryContext states)
{
	MemoryContext old;

	if (fast->has_extreme)
	{
		int			cmp;

		if (decimal != NULL && fast->decimal_valid)
		{
			int			scale = Max(decimal->scale, fast->decimal.scale);
			int128		left = (int128) decimal->value *
				fast_powers[scale - decimal->scale];
			int128		right = (int128) fast->decimal.value *
				fast_powers[scale - fast->decimal.scale];

			cmp = left < right ? -1 : left > right;
		}
		else
			cmp = DatumGetInt32(DirectFunctionCall2(numeric_cmp, value, fast->extreme));
		if (generic->fast == FAST_MAX ? cmp < 0 : cmp > 0)
			return;
		pfree(DatumGetPointer(fast->extreme));
	}
	old = MemoryContextSwitchTo(states);
	fast->extreme = PointerGetDatum(pg_detoast_datum_copy((struct varlena *) DatumGetPointer(value)));
	MemoryContextSwitchTo(old);
	fast->has_extreme = true;
	fast->decimal_valid = decimal != NULL;
	if (decimal != NULL)
		fast->decimal = *decimal;
}

/*
 * A float row into the state, as the core's functions take it: sum the
 * first value, then float8pl or float4pl (22003 on overflow); avg as
 * float8_accum and float4_accum, the Youngs-Cramer sums whose overflow
 * from finite values fails; min and max as float8smaller and
 * float8larger, the new value unless the state beats it.
 */
static void
fast_float_advance(GenericAgg *generic, FastState *fast, bool first, Datum value)
{
	bool		single = generic->fast_float == FLOAT4OID;
	float8		number = single ? (float8) DatumGetFloat4(value) : DatumGetFloat8(value);

	switch (generic->fast)
	{
		case FAST_SUM:
			if (single)
				fast->sx4 = first ? DatumGetFloat4(value) :
					float4_pl(fast->sx4, DatumGetFloat4(value));
			else
				fast->sx = first ? number : float8_pl(fast->sx, number);
			break;
		case FAST_AVG:
			{
				float8		previous_n = fast->n;
				float8		previous_sx = fast->sx;

				fast->n += 1.0;
				fast->sx += number;
				if (previous_n > 0.0)
				{
					float8		deviation = number * fast->n - fast->sx;

					fast->sxx += deviation * deviation / (fast->n * previous_n);
					if (isinf(fast->sx) || isinf(fast->sxx))
					{
						if (!isinf(previous_sx) && !isinf(number))
							float_overflow_error();
						fast->sxx = get_float8_nan();
					}
				}
				else if (isnan(number) || isinf(number))
					fast->sxx = get_float8_nan();
				break;
			}
		default:
			if (first ||
				!(generic->fast == FAST_MAX ?
				  (single ? float4_gt(fast->sx4, DatumGetFloat4(value)) : float8_gt(fast->sx, number)) :
				  (single ? float4_lt(fast->sx4, DatumGetFloat4(value)) : float8_lt(fast->sx, number))))
			{
				fast->sx = number;
				fast->sx4 = single ? DatumGetFloat4(value) : 0;
			}
			break;
	}
}

/* The numeric of a decimal into out, TESS_DECIMAL_NUMERIC_MAX bytes, by the kernels. */
static void
fast_write_numeric(GenericAgg *generic, const FastDecimal *decimal, void *out)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	Size		size;

	if (generic->kernels->decimal_write_datum(decimal->value, decimal->scale, out,
											  TESS_DECIMAL_NUMERIC_MAX, &size,
											  &status) != TESS_OK)
		tess_status_report(&status);
}

/*
 * min and max of a decimal whose numeric was never made: compared as
 * fast_extreme compares, the numeric of a new extreme made in the states'
 * context from the decimal, as the core would have kept it.
 */
static void
fast_decimal_extreme(GenericAgg *generic, FastState *fast, const FastDecimal *decimal,
					 MemoryContext states)
{
	char	   *numeric;

	if (fast->has_extreme)
	{
		int			cmp;

		if (fast->decimal_valid)
		{
			int			scale = Max(decimal->scale, fast->decimal.scale);
			int128		left = (int128) decimal->value *
				fast_powers[scale - decimal->scale];
			int128		right = (int128) fast->decimal.value *
				fast_powers[scale - fast->decimal.scale];

			cmp = left < right ? -1 : left > right;
		}
		else
		{
			char		buffer[TESS_DECIMAL_NUMERIC_MAX] pg_attribute_aligned(MAXIMUM_ALIGNOF);

			fast_write_numeric(generic, decimal, buffer);
			cmp = DatumGetInt32(DirectFunctionCall2(numeric_cmp, PointerGetDatum(buffer),
													 fast->extreme));
		}
		if (generic->fast == FAST_MAX ? cmp < 0 : cmp > 0)
			return;
		pfree(DatumGetPointer(fast->extreme));
	}
	numeric = MemoryContextAlloc(states, TESS_DECIMAL_NUMERIC_MAX);
	fast_write_numeric(generic, decimal, numeric);
	fast->extreme = PointerGetDatum(numeric);
	fast->has_extreme = true;
	fast->decimal_valid = true;
	fast->decimal = *decimal;
}

/* The node's arrays of a batch's decimals, for rows rows. */
static void
fast_scratch(GenericAgg *generic, int rows)
{
	MemoryContext context = GetMemoryChunkContext(generic);
	int			capacity = Max(rows, 64);
	int			words = tess_row_mask_word_count(capacity);

	if (rows <= generic->decimal_capacity)
		return;
	if (generic->decimal_values != NULL)
	{
		pfree(generic->decimal_values);
		pfree(generic->decimal_scales);
		pfree(generic->decimal_bits);
		pfree(generic->decimal_pending);
		pfree(generic->decimal_rest);
	}
	generic->decimal_values = MemoryContextAlloc(context, sizeof(Datum) * capacity);
	generic->decimal_scales = MemoryContextAlloc(context, capacity);
	generic->decimal_bits = MemoryContextAlloc(context, sizeof(uint64) * words);
	generic->decimal_pending = MemoryContextAlloc(context, sizeof(uint64) * words);
	generic->decimal_rest = MemoryContextAlloc(context, sizeof(uint64) * words);
	generic->decimal_capacity = capacity;
	generic->decimal_status = (TessStatus) TESS_STRUCT_INITIALIZER(TessStatus);
}

/*
 * A numeric argument's decimals over the rows of a batch, read by the
 * kernels once, for the rows the column does not hold as decimals itself.
 */
static void
fast_read(GenericAgg *generic, const TessRowMask *rows)
{
	const TessDatumColumn *column = &generic->columns[0];
	const uint64 *side = tess_column_decimal_rows(column);
	int			nwords = tess_row_mask_word_count(rows->nrows);
	TessRowMask pending = {rows->nrows, NULL};
	TessRowMask decimals = {rows->nrows, NULL};
	uint64		any = 0;

	fast_scratch(generic, rows->nrows);
	pending.bits = generic->decimal_pending;
	decimals.bits = generic->decimal_bits;
	for (int word = 0; word < nwords; word++)
	{
		pending.bits[word] = rows->bits[word] & ~(side != NULL ? side[word] : 0);
		decimals.bits[word] = 0;
		any |= pending.bits[word];
	}
	if (any != 0 &&
		generic->kernels->decimal_read(column, &pending, NULL, generic->decimal_values,
									   generic->decimal_scales, &decimals,
									   &generic->decimal_status) != TESS_OK)
		tess_status_report(&generic->decimal_status);
}

/*
 * sum and avg of a numeric argument without groups: a batch's decimals
 * added to the state by the kernels in one pass, as fast_add adds them
 * (the largest scale, below 10^36); the rows they leave (NaN, longer
 * values, a sum at its bound) are returned for the row-by-row path. The
 * state is made when the batch has a decimal.
 */
static TessRowMask
fast_sum(GenericAgg *generic, const TessRowMask *rows, MemoryContext states)
{
	FastState  *fast = generic->state_null ? NULL : (FastState *) DatumGetPointer(generic->state);
	TessDecimalSum sum = {0};
	TessRowMask rest = {rows->nrows, NULL};

	fast_scratch(generic, rows->nrows);
	rest.bits = generic->decimal_rest;
	/* An output mask comes clean: a smaller batch after a larger one. */
	memset(rest.bits, 0, sizeof(uint64) * tess_row_mask_word_count(rows->nrows));
	if (fast != NULL)
	{
		sum.low = (uint64) fast->sum;
		sum.high = (int64) (fast->sum >> 64);
		sum.scale = fast->scale;
		sum.count = fast->count;
	}
	if (generic->kernels->decimal_sum(&generic->columns[0], rows, &sum, &rest,
									  &generic->decimal_status) != TESS_OK)
		tess_status_report(&generic->decimal_status);
	if (fast == NULL && sum.count > 0)
	{
		generic->state = PointerGetDatum(MemoryContextAllocZero(states, sizeof(FastState)));
		generic->state_null = false;
		fast = (FastState *) DatumGetPointer(generic->state);
	}
	if (fast != NULL)
	{
		fast->sum = (int128) (((uint128) (uint64) sum.high << 64) | sum.low);
		fast->scale = sum.scale;
		fast->count = sum.count;
	}
	return rest;
}

/* The order of two decimals, exactly: at the larger scale in int128. */
static inline int
fast_decimal_cmp(const FastDecimal *left, const FastDecimal *right)
{
	int128		a;
	int128		b;
	int			scale;

	if (left->scale == right->scale)
		return left->value < right->value ? -1 : left->value > right->value;
	scale = Max(left->scale, right->scale);
	a = (int128) left->value * fast_powers[scale - left->scale];
	b = (int128) right->value * fast_powers[scale - right->scale];
	return a < b ? -1 : a > b;
}

/* A row's decimal: the column's own, or the one the kernels read (fast_read). */
static inline bool
fast_row_decimal(const GenericAgg *generic, uint64 side, uint64 read, int row, FastDecimal *decimal)
{
	uint64		bit = UINT64CONST(1) << (row % 64);

	if ((side & bit) != 0)
	{
		decimal->value = DatumGetInt64(generic->columns[0].values[row]);
		decimal->scale = generic->columns[0].decimal_scale;
		return true;
	}
	if ((read & bit) != 0)
	{
		decimal->value = DatumGetInt64(generic->decimal_values[row]);
		decimal->scale = generic->decimal_scales[row];
		return true;
	}
	return false;
}

/*
 * min and max of a numeric argument without groups: the batch's extreme
 * decimal found in one pass, a later row taking an equal value as
 * numeric_larger and numeric_smaller do, and folded in once. A batch with
 * a non-NULL row that is not a decimal goes row by row, in its order.
 */
static bool
fast_extreme_batch(GenericAgg *generic, const TessRowMask *rows, MemoryContext states)
{
	const TessDatumColumn *column = &generic->columns[0];
	const uint64 *side = tess_column_decimal_rows(column);
	bool		max = generic->fast == FAST_MAX;
	int			nwords = tess_row_mask_word_count(rows->nrows);
	FastDecimal best = {0};
	int			best_row = -1;
	FastState  *fast;

	for (int word = 0; word < nwords; word++)
	{
		uint64		side_bits = side != NULL ? side[word] : 0;
		uint64		read_bits = generic->decimal_bits[word];

		for (uint64 look = rows->bits[word]; look != 0; look &= look - 1)
		{
			int			row = word * 64 + pg_rightmost_one_pos64(look);
			FastDecimal decimal;
			int			cmp;

			if (!fast_row_decimal(generic, side_bits, read_bits, row, &decimal))
			{
				if (column->isnull[row])
					continue;
				return false;
			}
			if (best_row >= 0)
			{
				cmp = fast_decimal_cmp(&decimal, &best);
				if (max ? cmp < 0 : cmp > 0)
					continue;
			}
			best = decimal;
			best_row = row;
		}
	}
	if (best_row < 0)
		return true;
	if (generic->state_null)
	{
		generic->state = PointerGetDatum(MemoryContextAllocZero(states, sizeof(FastState)));
		generic->state_null = false;
	}
	fast = (FastState *) DatumGetPointer(generic->state);
	if (side != NULL && ((side[best_row / 64] >> (best_row % 64)) & 1) != 0)
		fast_decimal_extreme(generic, fast, &best, states);
	else
		fast_extreme(generic, fast, column->values[best_row], &best, states);
	return true;
}

/* One row into the state, the first non-NULL one making it. */
static void
fast_advance(GenericAgg *generic, int row, MemoryContext states)
{
	const TessDatumColumn *column = &generic->columns[0];
	const uint64 *decimal_rows = tess_column_decimal_rows(column);
	FastState  *fast;
	Datum		value;
	FastDecimal decimal;
	bool		decimal_valid = true;

	if (column->isnull[row])
		return;
	value = column->values[row];
	if (generic->state_null)
	{
		generic->state = PointerGetDatum(MemoryContextAllocZero(states, sizeof(FastState)));
		generic->state_null = false;
		if (OidIsValid(generic->fast_float))
		{
			fast_float_advance(generic, (FastState *) DatumGetPointer(generic->state), true,
							   value);
			return;
		}
	}
	fast = (FastState *) DatumGetPointer(generic->state);
	if (OidIsValid(generic->fast_float))
	{
		fast_float_advance(generic, fast, false, value);
		return;
	}
	if (generic->fast_numeric && decimal_rows != NULL &&
		((decimal_rows[row / 64] >> (row % 64)) & 1) != 0)
	{
		/* A decimal of the argument's chain: its numeric was never made. */
		decimal.value = DatumGetInt64(value);
		decimal.scale = column->decimal_scale;
		if (generic->fast == FAST_MIN || generic->fast == FAST_MAX)
		{
			fast_decimal_extreme(generic, fast, &decimal, states);
			return;
		}
	}
	else if (generic->fast_numeric)
	{
		/* A numeric the kernels read for the batch (fast_read), or the rest. */
		decimal_valid = ((generic->decimal_bits[row / 64] >> (row % 64)) & 1) != 0;
		if (decimal_valid)
		{
			decimal.value = DatumGetInt64(generic->decimal_values[row]);
			decimal.scale = generic->decimal_scales[row];
		}
	}
	else
	{
		decimal.value = generic->fast_wide ? DatumGetInt64(value) : DatumGetInt32(value);
		decimal.scale = 0;
	}
	if (generic->fast == FAST_MIN || generic->fast == FAST_MAX)
	{
		fast_extreme(generic, fast, value, decimal_valid ? &decimal : NULL, states);
		return;
	}
	fast->count++;
	if (decimal_valid)
		fast_add(fast, decimal, states);
	else
		fast_rest(fast, value, states);
}

/*
 * The value, as the core's final functions make it: sum the decimals'
 * sum at its scale plus the rest's (numeric_add keeps NaN and the
 * infinities as the core's sum does), sum(int2) its int8, avg that sum
 * divided by the count as numeric_avg and int8_avg divide it, min and max
 * the extreme.
 */
static Datum
fast_value(GenericAgg *generic, bool *isnull)
{
	FastState  *fast;
	Datum		sum;

	*isnull = generic->state_null;
	if (generic->state_null)
		return (Datum) 0;
	fast = (FastState *) DatumGetPointer(generic->state);
	/* A float's: float8_avg's Sx / N, the sum or the extreme of its type. */
	if (OidIsValid(generic->fast_float))
	{
		if (generic->fast == FAST_AVG)
			return Float8GetDatum(fast->sx / fast->n);
		return generic->fast_float == FLOAT4OID ? Float4GetDatum(fast->sx4) :
			Float8GetDatum(fast->sx);
	}
	if (generic->fast == FAST_MIN || generic->fast == FAST_MAX)
		return fast->extreme;
	if (generic->fast_int8_result)
		return Int64GetDatum((int64) fast->sum);
	sum = fast_numeric(fast->sum, fast->scale);
	if (fast->has_rest)
		sum = DirectFunctionCall2(numeric_add, fast->rest, sum);
	if (generic->fast == FAST_SUM)
		return sum;
	return DirectFunctionCall2(numeric_div, sum,
							   NumericGetDatum(int64_to_numeric(fast->count)));
}

/*
 * A sum state as the node's own partial value (TessTableSumInput): a bytea
 * of the tag, the words and the rest whole with its header.
 */
static Datum
sum_state_bytes(const uint64 *words, const struct varlena *rest)
{
	uint32		tag = TESS_TABLE_SUM_STATE_TAG;
	Size		len = VARHDRSZ + TESS_TABLE_SUM_STATE_BYTES +
		(rest != NULL ? VARSIZE_ANY(rest) : 0);
	bytea	   *result = palloc(len);

	SET_VARSIZE(result, len);
	memcpy(VARDATA(result), &tag, sizeof(tag));
	memcpy(VARDATA(result) + sizeof(tag), words, sizeof(uint64) * TESS_TABLE_SUM_WORDS);
	if (rest != NULL)
		memcpy(VARDATA(result) + TESS_TABLE_SUM_STATE_BYTES, rest, VARSIZE_ANY(rest));
	return PointerGetDatum(result);
}

/*
 * A plain partial aggregate's value from its state, for the final
 * aggregation above: a numeric or bigint sum or average (an internal state
 * in the core) in the node's own format, its sum's words, the count of
 * every value taken and its scale, NaN and the infinities in its rest, NULL
 * for an empty state; the others as the core's own transition value, which
 * any final aggregation reads, the initial value for an empty state: a
 * float's sum or extreme, the float8[] of avg's N, Sx and Sxx, the int8[]
 * of avg of integers' count and sum (its int8 wrapping as the core's
 * does), sum(int2)'s int8, a numeric extreme.
 */
static Datum
fast_partial(GenericAgg *generic, bool *isnull)
{
	FastState  *fast = generic->state_null ? NULL :
		(FastState *) DatumGetPointer(generic->state);
	uint64		words[TESS_TABLE_SUM_WORDS];

	*isnull = false;
	if (OidIsValid(generic->fast_float) && generic->fast == FAST_AVG)
	{
		Datum		items[3] = {
			Float8GetDatum(fast != NULL ? fast->n : 0),
			Float8GetDatum(fast != NULL ? fast->sx : 0),
			Float8GetDatum(fast != NULL ? fast->sxx : 0)
		};

		return PointerGetDatum(construct_array(items, 3, FLOAT8OID, sizeof(float8),
											   FLOAT8PASSBYVAL, TYPALIGN_DOUBLE));
	}
	if (generic->fast == FAST_AVG && !generic->fast_numeric && !generic->fast_wide &&
		!OidIsValid(generic->fast_float))
	{
		Datum		items[2] = {
			Int64GetDatum(fast != NULL ? fast->count : 0),
			Int64GetDatum(fast != NULL ? (int64) fast->sum : 0)
		};

		return PointerGetDatum(construct_array(items, 2, INT8OID, sizeof(int64),
											   FLOAT8PASSBYVAL, TYPALIGN_DOUBLE));
	}
	if (fast == NULL)
	{
		*isnull = true;
		return (Datum) 0;
	}
	if (OidIsValid(generic->fast_float))
		return generic->fast_float == FLOAT4OID ? Float4GetDatum(fast->sx4) :
			Float8GetDatum(fast->sx);
	if (generic->fast == FAST_MIN || generic->fast == FAST_MAX)
		return fast->extreme;
	if (generic->fast_int8_result)
		return Int64GetDatum((int64) fast->sum);
	words[0] = (uint64) fast->sum;
	words[1] = (uint64) ((uint128) fast->sum >> 64);
	words[2] = (uint64) fast->count;
	words[3] = (uint64) fast->scale;
	return sum_state_bytes(words, fast->has_rest ?
						   (const struct varlena *) DatumGetPointer(fast->rest) : NULL);
}

#endif							/* HAVE_INT128 */

/*
 * The aggregate's functions and initial value, as the core's ExecInitAgg
 * reads them; the states live in the context the stand-in AggState gives
 * the transition functions, one for every generic aggregate of the node.
 */
GenericAgg *
agg_generic_init(TessAggState *state, Aggref *agg)
{
	EState	   *estate = state->css.ss.ps.state;
	GenericAgg *generic = palloc0_object(GenericAgg);
	HeapTuple	tuple;
	Form_pg_aggregate form;
	Datum		initval;
	bool		isnull;
	Oid			inputs[FUNC_MAX_ARGS];
	int			ninputs = get_aggregate_argtypes(agg, inputs);
	Expr	   *expr;

	if (state->generic_agg == NULL)
	{
		state->generic_agg = makeNode(AggState);
		state->generic_agg->ss.ps.state = estate;
		state->generic_agg->curaggcontext = CreateExprContext(estate);
		state->generic_agg->aggcontexts = palloc_array(ExprContext *, 1);
		state->generic_agg->aggcontexts[0] = state->generic_agg->curaggcontext;
		/* AggGetTempMemoryContext: the node's own, reset a batch at a time. */
		state->generic_agg->tmpcontext = state->css.ss.ps.ps_ExprContext;
	}
	tuple = SearchSysCache1(AGGFNOID, ObjectIdGetDatum(agg->aggfnoid));
	if (!HeapTupleIsValid(tuple))
		elog(ERROR, "cache lookup failed for aggregate %u", agg->aggfnoid);
	form = (Form_pg_aggregate) GETSTRUCT(tuple);
	generic->nargs = list_length(agg->args);
	/* A function of polymorphic arguments asks their types of its call. */
	fmgr_info_cxt(form->aggtransfn, &generic->transfn, estate->es_query_cxt);
	build_aggregate_transfn_expr(inputs, ninputs, 0, agg->aggvariadic, agg->aggtranstype,
								 agg->inputcollid, form->aggtransfn, InvalidOid,
								 &expr, NULL);
	fmgr_info_set_expr((Node *) expr, &generic->transfn);
	/* Above a gather without groups, the participants' partial values. */
	if (state->finalize && state->nkeys == 0)
	{
		/* Two arguments of the transition type, as the core's ExecInitAgg builds it. */
		Oid			states[2] = {agg->aggtranstype, agg->aggtranstype};

		if (!OidIsValid(form->aggcombinefn))
			elog(ERROR, "TessAgg received a foreign plan");
		fmgr_info_cxt(form->aggcombinefn, &generic->combinefn, estate->es_query_cxt);
		build_aggregate_transfn_expr(states, 2, 0, agg->aggvariadic, agg->aggtranstype,
									 agg->inputcollid, form->aggcombinefn, InvalidOid,
									 &expr, NULL);
		fmgr_info_set_expr((Node *) expr, &generic->combinefn);
		generic->combine_call = palloc0(SizeForFunctionCallInfo(2));
		InitFunctionCallInfoData(*generic->combine_call, &generic->combinefn, 2,
								 agg->inputcollid, (Node *) state->generic_agg, NULL);
		generic->has_deserial = OidIsValid(form->aggdeserialfn);
		if (generic->has_deserial)
		{
			fmgr_info_cxt(form->aggdeserialfn, &generic->deserialfn, estate->es_query_cxt);
			build_aggregate_deserialfn_expr(form->aggdeserialfn, &expr);
			fmgr_info_set_expr((Node *) expr, &generic->deserialfn);
			generic->deserial_call = palloc0(SizeForFunctionCallInfo(2));
			InitFunctionCallInfoData(*generic->deserial_call, &generic->deserialfn, 2,
									 InvalidOid, (Node *) state->generic_agg, NULL);
		}
	}
	/* A partial aggregate goes to the Finalize Aggregate unfinished. */
	if (DO_AGGSPLIT_SKIPFINAL(agg->aggsplit))
	{
		generic->has_serial = DO_AGGSPLIT_SERIALIZE(agg->aggsplit) &&
			OidIsValid(form->aggserialfn);
		if (generic->has_serial)
		{
			fmgr_info_cxt(form->aggserialfn, &generic->serialfn, estate->es_query_cxt);
			build_aggregate_serialfn_expr(form->aggserialfn, &expr);
			fmgr_info_set_expr((Node *) expr, &generic->serialfn);
		}
	}
	else if (OidIsValid(form->aggfinalfn))
	{
		generic->has_final = true;
		fmgr_info_cxt(form->aggfinalfn, &generic->finalfn, estate->es_query_cxt);
		generic->final_nargs = form->aggfinalextra ? ninputs + 1 : 1;
		build_aggregate_finalfn_expr(inputs, generic->final_nargs, agg->aggtranstype,
									 agg->aggtype, agg->inputcollid, form->aggfinalfn,
									 &expr);
		fmgr_info_set_expr((Node *) expr, &generic->finalfn);
	}
	get_typlenbyval(agg->aggtranstype, &generic->translen, &generic->transbyval);
	initval = SysCacheGetAttr(AGGFNOID, tuple, Anum_pg_aggregate_agginitval, &isnull);
	generic->init_null = isnull;
	if (!isnull)
	{
		Oid			input;
		Oid			ioparam;
		char	   *string = TextDatumGetCString(initval);

		getTypeInputInfo(agg->aggtranstype, &input, &ioparam);
		generic->init = OidInputFunctionCall(input, string, ioparam, -1);
	}
	ReleaseSysCache(tuple);
	generic->trans_call = palloc0(SizeForFunctionCallInfo(generic->nargs + 1));
	InitFunctionCallInfoData(*generic->trans_call, &generic->transfn, generic->nargs + 1,
							 agg->inputcollid, (Node *) state->generic_agg, NULL);
	if (generic->has_final)
	{
		generic->final_call = palloc0(SizeForFunctionCallInfo(generic->final_nargs));
		InitFunctionCallInfoData(*generic->final_call, &generic->finalfn,
								 generic->final_nargs, agg->inputcollid,
								 (Node *) state->generic_agg, NULL);
	}
	if (generic->has_serial)
	{
		generic->serial_call = palloc0(SizeForFunctionCallInfo(1));
		InitFunctionCallInfoData(*generic->serial_call, &generic->serialfn, 1,
								 InvalidOid, (Node *) state->generic_agg, NULL);
	}
	generic->columns = palloc0_array(TessDatumColumn, generic->nargs);
	/*
	 * A final plain aggregate combines the core's partial states, but the
	 * node's own (own_states) of a numeric or bigint sum or average.
	 */
	if (state->finalize && state->nkeys == 0)
		generic->fast = state->own_states && own_partial_aggregate(agg) ?
			fast_kind(agg, generic, false) : FAST_NONE;
	else
		generic->fast = fast_kind(agg, generic, state->own_states);
	generic->partial = DO_AGGSPLIT_SKIPFINAL(agg->aggsplit);
	/* Its state starts empty, as the core's of these but avg(int4)'s. */
	if (generic->fast != FAST_NONE)
		generic->init_null = true;
#ifdef HAVE_INT128
	/* A group's sum or average of a number, whole: words of its record. */
	generic->sum_state = state->nkeys > 0 && generic->fast != FAST_NONE &&
		sum_state_aggregate(agg);
	generic->sum_pair = generic->sum_state &&
		(agg->aggfnoid == F_AVG_INT4 || agg->aggfnoid == F_AVG_INT2);
	/* Above a gather, the participants' partial values of the state. */
	generic->sum_input = state->finalize ?
		(generic->sum_pair ? TESS_TABLE_SUM_OF_PAIR : TESS_TABLE_SUM_OF_STATE) :
		generic->fast_numeric ? TESS_TABLE_SUM_OF_NUMERIC :
		generic->fast_wide ? TESS_TABLE_SUM_OF_INT8 : TESS_TABLE_SUM_OF_INT4;
#endif
	return generic;
}

/* The initial state, in the states' context. */
void
agg_generic_reset(TessAggState *state, GenericAgg *generic)
{
	MemoryContext old =
		MemoryContextSwitchTo(state->generic_agg->curaggcontext->ecxt_per_tuple_memory);

	generic->state_null = generic->init_null;
	generic->state = generic->init_null ? (Datum) 0 :
		datumCopy(generic->init, generic->transbyval, generic->translen);
	MemoryContextSwitchTo(old);
}

/*
 * The transition function over the selected rows of the arguments'
 * columns, as the core's Aggregate calls it per row: a strict function
 * skips a row with a NULL argument and, without an initial value, takes
 * the first argument of the first row it keeps as the state; a new
 * by-reference state is copied into the states' context and the old one
 * freed. What a call allocates besides goes with the batch's memory.
 */
static void
generic_advance(GenericAgg *generic, int row, MemoryContext states, MemoryContext temporary)
{
	FunctionCallInfo call = generic->trans_call;
	Datum		result;
	bool		skip = false;

#ifdef HAVE_INT128
	if (generic->fast != FAST_NONE)
	{
		fast_advance(generic, row, states);
		return;
	}
#endif

	for (int arg = 0; arg < generic->nargs; arg++)
	{
		call->args[arg + 1].value = generic->columns[arg].values[row];
		call->args[arg + 1].isnull = generic->columns[arg].isnull[row];
		skip |= call->args[arg + 1].isnull;
	}
	if (generic->transfn.fn_strict)
	{
		if (skip)
			return;
		if (generic->state_null)
		{
			MemoryContextSwitchTo(states);
			generic->state = datumCopy(call->args[1].value, generic->transbyval,
									   generic->translen);
			generic->state_null = false;
			MemoryContextSwitchTo(temporary);
			return;
		}
	}
	call->args[0].value = generic->state;
	call->args[0].isnull = generic->state_null;
	call->isnull = false;
	result = FunctionCallInvoke(call);
	if (!generic->transbyval &&
		DatumGetPointer(result) != DatumGetPointer(generic->state))
	{
		if (!call->isnull)
		{
			MemoryContextSwitchTo(states);
			result = datumCopy(result, generic->transbyval, generic->translen);
			MemoryContextSwitchTo(temporary);
		}
		if (!generic->state_null)
			pfree(DatumGetPointer(generic->state));
	}
	generic->state = result;
	generic->state_null = call->isnull;
}

/*
 * A participant's partial value into a generic aggregate's state, as the
 * core's Finalize Aggregate combines one: deserialized first when the
 * state is internal (a strict deserialization function keeps NULL), then
 * the combine function, which, strict, skips NULL and takes the first
 * value as the state; a new by-reference state is copied into the states'
 * context and the old one freed, as generic_advance does.
 */
void
agg_generic_combine(TessAggState *state, GenericAgg *generic, Datum value, bool isnull)
{
	MemoryContext states = state->generic_agg->curaggcontext->ecxt_per_tuple_memory;
	MemoryContext old =
		MemoryContextSwitchTo(state->css.ss.ps.ps_ExprContext->ecxt_per_tuple_memory);
	FunctionCallInfo call;
	Datum		result;

	if (generic->has_deserial && !(isnull && generic->deserialfn.fn_strict))
	{
		call = generic->deserial_call;
		call->args[0].value = value;
		call->args[0].isnull = isnull;
		call->args[1].value = (Datum) 0;
		call->args[1].isnull = false;
		call->isnull = false;
		value = FunctionCallInvoke(call);
		isnull = call->isnull;
	}
	if (generic->combinefn.fn_strict)
	{
		if (isnull)
		{
			MemoryContextSwitchTo(old);
			return;
		}
		if (generic->state_null)
		{
			MemoryContextSwitchTo(states);
			generic->state = datumCopy(value, generic->transbyval, generic->translen);
			generic->state_null = false;
			MemoryContextSwitchTo(old);
			return;
		}
	}
	call = generic->combine_call;
	call->args[0].value = generic->state;
	call->args[0].isnull = generic->state_null;
	call->args[1].value = value;
	call->args[1].isnull = isnull;
	call->isnull = false;
	result = FunctionCallInvoke(call);
	if (!generic->transbyval &&
		DatumGetPointer(result) != DatumGetPointer(generic->state))
	{
		if (!call->isnull)
		{
			MemoryContextSwitchTo(states);
			result = datumCopy(result, generic->transbyval, generic->translen);
		}
		if (!generic->state_null)
			pfree(DatumGetPointer(generic->state));
	}
	generic->state = result;
	generic->state_null = call->isnull;
	MemoryContextSwitchTo(old);
}

void
agg_generic_accumulate(TessAggState *state, GenericAgg *generic, const TessRowMask *rows)
{
	MemoryContext states = state->generic_agg->curaggcontext->ecxt_per_tuple_memory;
	MemoryContext temporary = state->css.ss.ps.ps_ExprContext->ecxt_per_tuple_memory;
	MemoryContext old = MemoryContextSwitchTo(temporary);
	int			row = -1;

#ifdef HAVE_INT128
	TessRowMask rest;

	if (generic->fast != FAST_NONE && generic->fast_numeric)
	{
		/* Without groups, sum and avg leave the kernels a few rows at most. */
		if (generic->fast == FAST_SUM || generic->fast == FAST_AVG)
		{
			rest = fast_sum(generic, rows, states);
			rows = &rest;
		}
		fast_read(generic, rows);
		if ((generic->fast == FAST_MIN || generic->fast == FAST_MAX) &&
			fast_extreme_batch(generic, rows, states))
		{
			MemoryContextSwitchTo(old);
			return;
		}
	}
#endif
	while ((row = tess_row_mask_next(rows, row)) >= 0)
		generic_advance(generic, row, states, temporary);
	MemoryContextSwitchTo(old);
}

/* The payload of the record at ref, in the chunk's memory, which the node writes. */
uint64 *
agg_record_payload(TessAggState *state, uint32 ref)
{
	char	   *record = (char *) state->chunk_bases[tess_table_ref_chunk(ref)] +
		tess_table_ref_byte(ref);

	if (!state->payload_known)
	{
		TessTableRecord found = TESS_STRUCT_INITIALIZER(TessTableRecord);

		check(state, state->kernels->table_record(&state->table, ref, &found,
												  &state->status));
		state->payload_delta = (Size) ((const char *) found.payload - record);
		state->payload_known = true;
	}
	return (uint64 *) (record + state->payload_delta);
}

#ifdef HAVE_INT128
/*
 * The rows of a batch into the groups' states of a numeric aggregate the
 * node folds itself, its decimals read (fast_read): a decimal goes straight
 * into its group's state, made at its first one, and any other row
 * through fast_advance.
 */
static void
fast_group_decimals(TessAggState *state, int index, const TessRowMask *rows,
					MemoryContext states, MemoryContext temporary)
{
	GenericAgg *generic = state->values[index].generic;
	int			slot = state->values[index].slot;
	const TessDatumColumn *column = &generic->columns[0];
	const uint64 *side = tess_column_decimal_rows(column);
	bool		extreme = generic->fast == FAST_MIN || generic->fast == FAST_MAX;
	uint64		bit = UINT64CONST(1) << index;
	int			nwords = tess_row_mask_word_count(rows->nrows);

	for (int word = 0; word < nwords; word++)
	{
		uint64		side_bits = side != NULL ? side[word] : 0;
		uint64		read_bits = generic->decimal_bits[word];

		for (uint64 look = rows->bits[word]; look != 0; look &= look - 1)
		{
			int			row = word * 64 + pg_rightmost_one_pos64(look);
			uint64	   *payload = agg_record_payload(state, state->offsets[row]);
			FastDecimal decimal;
			FastState  *fast;

			if (!fast_row_decimal(generic, side_bits, read_bits, row, &decimal))
			{
				generic->state = (Datum) payload[slot];
				generic->state_null = (payload[0] & bit) == 0;
				generic_advance(generic, row, states, temporary);
				payload[slot] = generic->state_null ? 0 : (uint64) generic->state;
				payload[0] = generic->state_null ? payload[0] & ~bit : payload[0] | bit;
				continue;
			}
			if ((payload[0] & bit) == 0)
			{
				payload[slot] = (uint64) MemoryContextAllocZero(states, sizeof(FastState));
				payload[0] |= bit;
			}
			fast = (FastState *) payload[slot];
			if (!extreme)
			{
				fast->count++;
				fast_add(fast, decimal, states);
			}
			else if (((side_bits >> (row % 64)) & 1) != 0)
				fast_decimal_extreme(generic, fast, &decimal, states);
			else
				fast_extreme(generic, fast, column->values[row], &decimal, states);
		}
	}
}
#endif

#ifdef HAVE_INT128
/* A special value of numeric, made as the core makes it from its text. */
static Datum
sum_state_special(const char *name)
{
	return DirectFunctionCall3(numeric_in, CStringGetDatum(name),
							   ObjectIdGetDatum(InvalidOid), Int32GetDatum(-1));
}

/* A row the kernels left to a sum state, as a numeric. */
static Datum
sum_state_term(const GenericAgg *generic, const TessDatumColumn *column, int row)
{
	Datum		value = column->values[row];
	const uint64 *side = tess_column_decimal_rows(column);

	switch (generic->sum_input)
	{
		case TESS_TABLE_SUM_OF_INT4:
			return NumericGetDatum(int64_to_numeric(DatumGetInt32(value)));
		case TESS_TABLE_SUM_OF_INT8:
			return NumericGetDatum(int64_to_numeric(DatumGetInt64(value)));
		default:
			if (side != NULL && ((side[row / 64] >> (row % 64)) & 1) != 0)
				return NumericGetDatum(int64_div_fast_to_numeric(DatumGetInt64(value),
																 column->decimal_scale));
			return value;
	}
}

/*
 * A row the kernels left to a sum state, by the core's means: NaN or an
 * infinity into its bit, any other value counted and added to the group's
 * rest, a numeric in the states' context whose address is the word after
 * the kernels' state.
 */
static void
sum_state_rest(TessAggState *state, const GenericAgg *generic, int slot, int row,
			   MemoryContext states)
{
	uint64	   *words = agg_record_payload(state, state->offsets[row]) + slot;
	Numeric		number = DatumGetNumeric(sum_state_term(generic, &generic->columns[0], row));
	MemoryContext old;

	if (numeric_is_nan(number))
	{
		words[3] |= TESS_TABLE_SUM_NAN;
		return;
	}
	if (numeric_is_inf(number))
	{
		bool		positive = DatumGetInt32(DirectFunctionCall2(numeric_cmp,
																 NumericGetDatum(number),
																 NumericGetDatum(int64_to_numeric(0)))) > 0;

		words[3] |= positive ? TESS_TABLE_SUM_POSITIVE_INFINITY :
			TESS_TABLE_SUM_NEGATIVE_INFINITY;
		return;
	}
	words[2]++;
	old = MemoryContextSwitchTo(states);
	if (words[TESS_TABLE_SUM_WORDS] == 0)
		words[TESS_TABLE_SUM_WORDS] =
			(uint64) DatumGetPointer(datumCopy(NumericGetDatum(number), false, -1));
	else
	{
		Datum		sum = DirectFunctionCall2(numeric_add,
											  (Datum) words[TESS_TABLE_SUM_WORDS],
											  NumericGetDatum(number));

		pfree((void *) words[TESS_TABLE_SUM_WORDS]);
		words[TESS_TABLE_SUM_WORDS] = (uint64) DatumGetPointer(sum);
	}
	MemoryContextSwitchTo(old);
}

/*
 * A partial sum state of the node's own format (TessTableSumInput): its
 * words, and its rest, copied out to a place of its own in the current
 * context, or 0 without one.
 */
static void
sum_state_read(Datum value, uint64 *words, Datum *rest)
{
	bytea	   *bytes = DatumGetByteaPP(value);
	const char *data = VARDATA_ANY(bytes);
	Size		len = VARSIZE_ANY_EXHDR(bytes);
	uint32		tag;

	if (len >= TESS_TABLE_SUM_STATE_BYTES)
		memcpy(&tag, data, sizeof(tag));
	if (len < TESS_TABLE_SUM_STATE_BYTES || tag != TESS_TABLE_SUM_STATE_TAG)
		elog(ERROR, "TessAgg received a partial sum state of another format");
	memcpy(words, data + sizeof(tag), sizeof(uint64) * TESS_TABLE_SUM_WORDS);
	*rest = (Datum) 0;
	if (len > TESS_TABLE_SUM_STATE_BYTES)
	{
		char	   *copy = palloc(len - TESS_TABLE_SUM_STATE_BYTES);

		memcpy(copy, data + TESS_TABLE_SUM_STATE_BYTES, len - TESS_TABLE_SUM_STATE_BYTES);
		*rest = PointerGetDatum(copy);
	}
}

/*
 * A partial state the kernels left to a final grouping's sum state, merged
 * by the core's means: one with a numeric rest, or one whose sum would
 * take the group's past its bound. Its flags and its count go into the
 * words, its sum at its scale and its rest into the group's rest.
 */
static void
sum_state_merge_rest(TessAggState *state, const GenericAgg *generic, int slot, int row,
					 MemoryContext states)
{
	uint64	   *words = agg_record_payload(state, state->offsets[row]) + slot;
	Datum		value = generic->columns[0].values[row];
	uint64		theirs[TESS_TABLE_SUM_WORDS];
	Datum		rest = (Datum) 0;
	Datum		sum;
	MemoryContext old =
		MemoryContextSwitchTo(state->css.ss.ps.ps_ExprContext->ecxt_per_tuple_memory);

	if (generic->sum_pair)
	{
		ArrayType  *pair = DatumGetArrayTypeP(value);
		const int64 *items = (const int64 *) ARR_DATA_PTR(pair);

		if (ARR_NDIM(pair) != 1 || ARR_HASNULL(pair) || ARR_ELEMTYPE(pair) != INT8OID ||
			ARR_DIMS(pair)[0] != 2)
			elog(ERROR, "TessAgg received a partial average of another format");
		theirs[0] = (uint64) items[1];
		theirs[1] = items[1] < 0 ? PG_UINT64_MAX : 0;
		theirs[2] = (uint64) items[0];
		theirs[3] = 0;
	}
	else
		sum_state_read(value, theirs, &rest);
	words[3] |= theirs[3] & (TESS_TABLE_SUM_NAN | TESS_TABLE_SUM_POSITIVE_INFINITY |
							 TESS_TABLE_SUM_NEGATIVE_INFINITY);
	words[2] += theirs[2];
	sum = fast_numeric((int128) (((uint128) theirs[1] << 64) | theirs[0]),
					   (int) (theirs[3] & TESS_TABLE_SUM_SCALE_MASK));
	if (rest != (Datum) 0)
		sum = DirectFunctionCall2(numeric_add, sum, rest);
	MemoryContextSwitchTo(states);
	if (words[TESS_TABLE_SUM_WORDS] == 0)
		words[TESS_TABLE_SUM_WORDS] = (uint64) DatumGetPointer(datumCopy(sum, false, -1));
	else
	{
		Datum		total = DirectFunctionCall2(numeric_add,
												(Datum) words[TESS_TABLE_SUM_WORDS], sum);

		pfree((void *) words[TESS_TABLE_SUM_WORDS]);
		words[TESS_TABLE_SUM_WORDS] = (uint64) DatumGetPointer(total);
	}
	MemoryContextSwitchTo(old);
}

/*
 * A sum of decimals at scale `from` brought to scale `to`, not smaller: false,
 * the sum unchanged, when it would pass the bound.
 */
static bool
fast_rescale(int128 *sum, int from, int to)
{
	int128		factor = fast_powers[to - from];

	if (*sum >= FAST_BOUND / factor || *sum <= -FAST_BOUND / factor)
		return false;
	*sum *= factor;
	return true;
}

/*
 * A participant's partial state of the node's own format into a plain
 * final aggregation's state (fast_partial): its count added, its sum at the
 * larger of the two scales, or, when either sum would pass the bound at
 * it, to the rest at its own scale (the rest's display scale keeps it),
 * and its rest to the rest.
 */
void
agg_fast_merge(TessAggState *state, GenericAgg *generic, Datum value, bool isnull)
{
	MemoryContext states = state->generic_agg->curaggcontext->ecxt_per_tuple_memory;
	MemoryContext old;
	uint64		words[TESS_TABLE_SUM_WORDS];
	Datum		rest;
	FastState  *fast;
	int128		term;
	int			scale;

	if (isnull)
		return;
	old = MemoryContextSwitchTo(state->css.ss.ps.ps_ExprContext->ecxt_per_tuple_memory);
	sum_state_read(value, words, &rest);
	scale = (int) (words[3] & TESS_TABLE_SUM_SCALE_MASK);
	if ((words[3] & ~TESS_TABLE_SUM_SCALE_MASK) != 0 || scale > TESS_DECIMAL_DIGITS)
		elog(ERROR, "TessAgg received a partial sum state of another format");
	if (generic->state_null)
	{
		generic->state = PointerGetDatum(MemoryContextAllocZero(states, sizeof(FastState)));
		generic->state_null = false;
	}
	fast = (FastState *) DatumGetPointer(generic->state);
	fast->count += (int64) words[2];
	term = (int128) (((uint128) words[1] << 64) | words[0]);
	if (scale > fast->scale ? fast_rescale(&fast->sum, fast->scale, scale) :
		fast_rescale(&term, scale, fast->scale))
	{
		fast->scale = Max(fast->scale, scale);
		fast->sum += term;
		if (fast->sum >= FAST_BOUND || fast->sum <= -FAST_BOUND)
			fast_flush(fast, states);
	}
	else
		fast_rest(fast, fast_numeric(term, scale), states);
	if (rest != (Datum) 0)
		fast_rest(fast, rest, states);
	MemoryContextSwitchTo(old);
}

/*
 * The rows of a batch into the groups' sum states of the aggregates at
 * indexes, words of their records: the kernels fold what they can, the
 * record found once a row for all of them (tess_table_accumulate_sums),
 * and leave the rest here (sum_state_rest; a final grouping's partial
 * states, sum_state_merge_rest). A new group's words are zeros, the empty
 * state.
 */
void
agg_sum_states_accumulate(TessAggState *state, int nsums, const int *indexes,
					  const TessRowMask *rows)
{
	MemoryContext states = state->generic_agg->curaggcontext->ecxt_per_tuple_memory;
	int			nwords = tess_row_mask_word_count(rows->nrows);

	for (int first = 0; first < nsums; first += TESS_TABLE_MAX_SUMS)
	{
		int			count = Min(nsums - first, TESS_TABLE_MAX_SUMS);
		TessTableSumArg args[TESS_TABLE_MAX_SUMS];
		TessRowMask rests[TESS_TABLE_MAX_SUMS];

		for (int sum = 0; sum < count; sum++)
		{
			AggValue   *value = &state->values[indexes[first + sum]];

			rests[sum] = (TessRowMask) {rows->nrows, state->sum_rest_bits + sum * nwords};
			memset(rests[sum].bits, 0, sizeof(uint64) * nwords);
			args[sum] = (TessTableSumArg) {
				.kind = value->generic->sum_input,
				.column = &value->generic->columns[0],
				.value_at = sizeof(uint64) * value->slot,
				.rest = &rests[sum],
			};
		}
		check(state, state->kernels->table_accumulate_sums(&state->table, state->offsets, rows,
														   count, args, &state->status));
		for (int sum = 0; sum < count; sum++)
		{
			AggValue   *value = &state->values[indexes[first + sum]];
			int			row = -1;

			while ((row = tess_row_mask_next(&rests[sum], row)) >= 0)
			{
				if (state->finalize)
					sum_state_merge_rest(state, value->generic, value->slot, row, states);
				else
					sum_state_rest(state, value->generic, value->slot, row, states);
			}
		}
	}
}

/*
 * A group's sum or average from its sum state, as the core's numeric_sum,
 * numeric_avg, numeric_poly_sum, numeric_poly_avg and int8_avg finish
 * theirs: NULL without a value, NaN after NaN or both infinities, an
 * infinity after one, else the sum at its scale plus the rest, the
 * average that divided by the count.
 */
Datum
agg_sum_state_value(const GenericAgg *generic, const uint64 *words, bool *isnull)
{
	uint64		flags = words[3];
	int64		count = (int64) words[2];
	int128		value = (int128) (((uint128) words[1] << 64) | words[0]);
	Datum		sum;

	*isnull = false;
	if ((flags & TESS_TABLE_SUM_NAN) != 0 ||
		((flags & TESS_TABLE_SUM_POSITIVE_INFINITY) != 0 &&
		 (flags & TESS_TABLE_SUM_NEGATIVE_INFINITY) != 0))
		return sum_state_special("NaN");
	if ((flags & TESS_TABLE_SUM_POSITIVE_INFINITY) != 0)
		return sum_state_special("Infinity");
	if ((flags & TESS_TABLE_SUM_NEGATIVE_INFINITY) != 0)
		return sum_state_special("-Infinity");
	if (count == 0)
	{
		*isnull = true;
		return (Datum) 0;
	}
	sum = fast_numeric(value, (int) (flags & TESS_TABLE_SUM_SCALE_MASK));
	if (words[TESS_TABLE_SUM_WORDS] != 0)
		sum = DirectFunctionCall2(numeric_add, (Datum) words[TESS_TABLE_SUM_WORDS], sum);
	if (generic->fast == FAST_SUM)
		return sum;
	return DirectFunctionCall2(numeric_div, sum, NumericGetDatum(int64_to_numeric(count)));
}

/*
 * A group's sum state as its partial value, for the node's final grouping
 * (TessTableSumInput): the node's own bytea of the tag, the words and the
 * rest, NULL for the empty state; for avg of integer or smallint, the
 * core's int8[] of the count and the sum, whose int8 wraps as the core's
 * transition's (no rest: an integer is always a decimal the sum takes).
 */
Datum
agg_sum_state_partial(const GenericAgg *generic, const uint64 *words, bool *isnull)
{
	const struct varlena *rest = (const struct varlena *) words[TESS_TABLE_SUM_WORDS];

	*isnull = false;
	if (generic->sum_pair)
	{
		Datum		pair[2] = {Int64GetDatum((int64) words[2]), Int64GetDatum((int64) words[0])};

		return PointerGetDatum(construct_array(pair, 2, INT8OID, sizeof(int64),
											   FLOAT8PASSBYVAL, TYPALIGN_DOUBLE));
	}
	if (rest == NULL && (words[0] | words[1] | words[2] | words[3]) == 0)
	{
		*isnull = true;
		return (Datum) 0;
	}
	return sum_state_bytes(words, rest);
}
#endif

/*
 * The groups' states of a generic aggregate over the rows of a batch:
 * the groups the batch inserted start from the initial value; then, row
 * by row, as rows of one group may follow one another, the state is read
 * from the group's record, advanced and written back, with the
 * aggregate's flag bit set while it is not NULL.
 */
void
agg_generic_group_accumulate(TessAggState *state, int index, const TessRowMask *rows,
						 const TessRowMask *inserted)
{
	GenericAgg *generic = state->values[index].generic;
	int			slot = state->values[index].slot;
	MemoryContext states = state->generic_agg->curaggcontext->ecxt_per_tuple_memory;
	MemoryContext temporary = state->css.ss.ps.ps_ExprContext->ecxt_per_tuple_memory;
	MemoryContext old;
	uint64		bit = UINT64CONST(1) << index;
	int			row = -1;

#ifdef HAVE_INT128
	if (generic->sum_state)
	{
		agg_sum_states_accumulate(state, 1, &index, rows);
		return;
	}
#endif
	old = MemoryContextSwitchTo(states);

	while ((row = tess_row_mask_next(inserted, row)) >= 0)
	{
		uint64	   *payload = agg_record_payload(state, state->offsets[row]);

		payload[slot] = generic->init_null ? 0 :
			(uint64) datumCopy(generic->init, generic->transbyval, generic->translen);
		payload[0] = generic->init_null ? payload[0] & ~bit : payload[0] | bit;
	}
	MemoryContextSwitchTo(temporary);
#ifdef HAVE_INT128
	if (generic->fast != FAST_NONE && generic->fast_numeric)
	{
		fast_read(generic, rows);
		fast_group_decimals(state, index, rows, states, temporary);
		MemoryContextSwitchTo(old);
		return;
	}
#endif
	row = -1;
	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		uint64	   *payload = agg_record_payload(state, state->offsets[row]);

		generic->state = (Datum) payload[slot];
		generic->state_null = (payload[0] & bit) == 0;
		generic_advance(generic, row, states, temporary);
		payload[slot] = generic->state_null ? 0 : (uint64) generic->state;
		payload[0] = generic->state_null ? payload[0] & ~bit : payload[0] | bit;
	}
	MemoryContextSwitchTo(old);
}

/*
 * The aggregate's value: the final function over the state (with NULL
 * for the extra arguments it asks for), or, in a partial plan, the
 * state serialized, or the state itself.
 */
Datum
agg_generic_value(GenericAgg *generic, bool *isnull)
{
	FunctionCallInfo call;
	Datum		result;

#ifdef HAVE_INT128
	if (generic->fast != FAST_NONE)
		return generic->partial ? fast_partial(generic, isnull) : fast_value(generic, isnull);
#endif

	if (generic->has_serial)
	{
		if (generic->state_null)
		{
			*isnull = true;
			return (Datum) 0;
		}
		call = generic->serial_call;
		call->args[0].value = generic->state;
		call->args[0].isnull = false;
	}
	else if (generic->has_final)
	{
		if (generic->finalfn.fn_strict && generic->state_null)
		{
			*isnull = true;
			return (Datum) 0;
		}
		call = generic->final_call;
		call->args[0].value = generic->state;
		call->args[0].isnull = generic->state_null;
		for (int arg = 1; arg < generic->final_nargs; arg++)
		{
			call->args[arg].value = (Datum) 0;
			call->args[arg].isnull = true;
		}
	}
	else
	{
		*isnull = generic->state_null;
		return generic->state;
	}
	call->isnull = false;
	result = FunctionCallInvoke(call);
	*isnull = call->isnull;
	return result;
}

#include "postgres.h"

#include "access/htup_details.h"
#include "access/nbtree.h"
#include "catalog/pg_aggregate.h"
#include "commands/explain_format.h"
#include "common/hashfn.h"
#include "common/int.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/optimizer.h"
#include "parser/parse_agg.h"
#include "utils/array.h"
#include "utils/float.h"
#include "utils/fmgroids.h"
#include "utils/syscache.h"
#include "utils/typcache.h"
#include "utils/lsyscache.h"
#include "utils/datum.h"
#include "utils/builtins.h"
#include "utils/numeric.h"
#include "utils/pg_locale.h"
#include "utils/regproc.h"
#include "utils/ruleutils.h"

#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"
#include "agg.h"

/*
 * TessAgg computes the aggregates of a query over the batches of a batch
 * child, so that no row is handed up one at a time. Without GROUP BY it
 * stands in for the core's plain Aggregate and returns the one result
 * row: each aggregate is computed per batch by the registered batch
 * function of its aggregate (tessera/function.h, kind
 * TESS_FUNCTION_AGGREGATE) and the partials are combined here, with the
 * overflow check the core's transition would make. With GROUP BY it
 * stands in for the core's HashAggregate: each row finds the record of
 * its keys in a hash table (tessera/table.h), whose payload holds the
 * group's aggregate states, and the groups go out in batches when the
 * input ends. See docs/nodes.md.
 */
/* A batch with at most this many survivors is gathered for one call later. */
#define AGG_GATHER_ROWS 8
/* The table's first capacity when the planner expects fewer groups. */
#define AGG_INITIAL_GROUPS 256

/* The counters every participant of a parallel plan shares. */
enum
{
	AGG_BATCHES,
	AGG_ROWS,
	AGG_CALLS,
	AGG_COMPUTED,
	AGG_GROUPS,
	AGG_MEMORY,
	AGG_GROWS,
	/*
	 * Spilling: the most partitions of a level, the partitions sent to
	 * disk while the input was read, the chunks and bytes written, the
	 * partitions split into a level below.
	 */
	AGG_PARTITIONS,
	AGG_EVICTIONS,
	AGG_SPILLED,
	AGG_DISK,
	AGG_SPLITS,
	/* Partial mode: the times the groups went out before the input ended. */
	AGG_EARLY,
	/* Generic states past hash_mem: the rows sent to partitions on disk. */
	AGG_SPILLED_ROWS,
	AGG_NCOUNTERS
};


/*
 * Any other aggregate without GROUP BY: its transition function called for
 * each selected row of the arguments' columns of a batch, as the core's
 * Aggregate calls it per row but without a row handed up, then its final
 * function, or, in a partial plan, its serialization function for the
 * Finalize Aggregate above. A transition function that keeps its state in
 * the aggregate's memory asks for it (AggCheckCallContext): a stand-in
 * AggState gives the node's context of states.
 */
/*
 * A generic aggregate the node folds itself, its state its own in place of
 * the core's transition functions, its value the core's final function's:
 * sum and avg of numeric, bigint, integer and smallint, min and max of
 * numeric; sum, avg, min and max of float8 and float4, row by row in the
 * rows' order as the core's functions, so to the last bit. The rest go
 * through the core's functions.
 */
typedef enum FastKind
{
	FAST_NONE,
	FAST_SUM,
	FAST_AVG,
	FAST_MIN,
	FAST_MAX
} FastKind;

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

typedef struct GenericAgg
{
	FmgrInfo	transfn;
	FmgrInfo	finalfn;
	FmgrInfo	serialfn;
	bool		has_final;
	bool		has_serial;
	/*
	 * Above a gather without groups: the participants' partial values go
	 * into the state by the combine function, deserialized first by the
	 * deserialization function when the state is internal.
	 */
	FmgrInfo	combinefn;
	FmgrInfo	deserialfn;
	bool		has_deserial;
	FunctionCallInfo combine_call;
	FunctionCallInfo deserial_call;
	int			final_nargs;
	int			nargs;
	int16		translen;
	bool		transbyval;
	Datum		init;
	bool		init_null;
	Datum		state;
	bool		state_null;
	FunctionCallInfo trans_call;
	FunctionCallInfo final_call;
	FunctionCallInfo serial_call;
	/* The arguments' columns of the batch being added. */
	TessDatumColumn *columns;
	/*
	 * FAST_NONE, or the node folds the aggregate itself over a numeric
	 * argument or an integer one (an int8 word when wide), sum(int2) giving
	 * an int8.
	 */
	FastKind	fast;
	bool		fast_numeric;
	bool		fast_wide;
	bool		fast_int8_result;
	/* A float argument: FLOAT8OID or FLOAT4OID, else InvalidOid. */
	Oid			fast_float;
	/*
	 * A numeric argument's decimals of the batch being added, read by the
	 * kernels for decimal_capacity rows: the values and scales, the rows
	 * that are decimals, and the rows read (those without the column's own
	 * decimals).
	 */
	const TessKernelOps *kernels;
	Datum	   *decimal_values;
	uint8	   *decimal_scales;
	uint64	   *decimal_bits;
	uint64	   *decimal_pending;
	/* The rows the kernels' sum of a batch left to the node. */
	uint64	   *decimal_rest;
	int			decimal_capacity;
	TessStatus	decimal_status;
	/*
	 * GROUP BY, sum and avg of numeric and bigint, avg of integer and
	 * smallint: the group's state is TESS_TABLE_SUM_WORDS words of its
	 * record, which the kernels fold a batch into, reading sum_input, and
	 * then the address of the numeric sum of the rows they leave to the
	 * node, 0 without any (sum_state).
	 */
	bool		sum_state;
	TessTableSumInput sum_input;
	/*
	 * A sum state of avg of integer or smallint, whose partial value is the
	 * core's int8[] of the count and the sum (sum_state_partial).
	 */
	bool		sum_pair;
	/* A partial aggregate's: its value is its state (fast_partial). */
	bool		partial;
} GenericAgg;

/*
 * DISTINCT in an aggregate: a table of its own, without payload, keyed by
 * the group's keys and the argument, the argument alone without GROUP BY.
 * A row goes into the aggregate only when it inserted its pair: the ones
 * seen before are dropped, and so are NULL arguments, which the aggregate
 * skips anyway. The table does not spill.
 */
typedef struct DistinctSet
{
	MemoryContext context;
	int			nkeys;
	TessTableKeyKind kinds[TESS_TABLE_MAX_KEYS];
	TessTableRef table;
	void	  **bases;
	Size	   *lens;
	int			slots;
	Size		bytes;
	/* The buffers of a batch, for capacity rows. */
	int			capacity;
	uint32	   *hashes;
	uint32	   *offsets;
	uint64	   *pending_bits;
	uint64	   *inserted_bits;
	uint64	   *call_bits;
} DistinctSet;

struct AggValue
{
	AggKind		kind;
	const TessFunction *function;
	/* The argument's computed column of the projection, or -1 for count(*). */
	int			computed;
	/* FILTER (WHERE ...): its computed column, true or NULL, or -1. */
	int			filter;
	/* The argument's values of sparse batches, a column of their own. */
	Datum	   *gathered_values;
	bool	   *gathered_isnull;
	int			ngathered;
	int64		total;
	/* The extreme so far, int4 or int8 as the aggregate's transition type. */
	int64		extreme;
	bool		wide;
	bool		has_value;
	/* GROUP BY: how the table folds a row into the group's state. */
	TessTableAccumulate accumulate;
	/*
	 * GROUP BY: the payload word where the state starts, after the word of
	 * flags; a word, or a generic sum state's AGG_SUM_STATE_WORDS.
	 */
	int			slot;
	/*
	 * DISTINCT: the pairs of group and argument seen, and the argument's
	 * kind; an argument a word does not hold goes by its number in a
	 * dictionary of its values of the aggregate's own.
	 */
	struct DistinctSet *distinct;
	TessTableKeyKind argument_kind;
	struct KeyDict *distinct_dict;
	GenericAgg *generic;
};

/*
 * A key of a type a word does not hold (text, numeric, ...): its values
 * get numbers, in the order the table first meets them, through a
 * dictionary that hashes and compares them by the type's functions (the
 * key's equality and its hash function, under the key's collation), and
 * the table groups by the numbers as int8 keys; a group's key goes out as
 * the value of its number. The dictionary goes with the table it numbers:
 * made anew with every table, so such groupings spill their rows, with
 * the values, not their records.
 *
 * Equal values may differ in form (numeric 1.0 and 1.00, float8 -0 and 0,
 * text under a case-insensitive collation), and a number keeps the first
 * form of the whole input, which with several keys need not be a group's
 * first row's, the form the core's hashed grouping puts out. A group made
 * by a row of another form keeps that form, by its record, among the
 * dictionary's forms; a single key's group is its value's, its first row
 * the value's first.
 */

typedef struct KeyForm
{
	uint32		ref;
	char		status;
	Datum		value;
} KeyForm;

struct KeyDict
{
	struct keydict_hash *table;
	MemoryContext context;
	FmgrInfo	hashfn;
	FmgrInfo	eqfn;
	Oid			collation;
	int16		typlen;
	bool		typbyval;
	/*
	 * Equal values are equal bytes, and the hash function hashes them
	 * (text and varchar under a deterministic collation, bytea): a value
	 * neither compressed nor external is hashed and compared here, without
	 * a call through fmgr, which looked the collation up on every call.
	 */
	bool		bytewise;
	/*
	 * The values by their numbers, copies in the context: one after
	 * another in blocks of the dictionary's own, without a chunk's header
	 * each (a text of 14 bytes took 32 through palloc).
	 */
	Datum	   *values;
	int64		count;
	int64		slots;
	char	   *block;
	Size		block_used;
	Size		block_len;
	/* A batch's numbers and the values' hashes, for capacity rows. */
	int			capacity;
	Datum	   *batch_numbers;
	uint32	   *batch_hashes;
	/*
	 * A key of several in a grouping whose equal values may differ in form:
	 * the groups whose first row's form is not their number's, by their
	 * records, a copy of the form each; NULL while there is none.
	 */
	bool		forms;
	struct keyform_hash *form_table;
};

/* Ask memory for an address the loop reads soon, where the compiler can. */
#if defined(__GNUC__) || defined(__clang__)
#define keydict_prefetch(address) __builtin_prefetch(address)
#else
#define keydict_prefetch(address) ((void) 0)
#endif

/* A varlena whose bytes are at hand: not compressed, not external. */
static inline bool
keydict_plain(Datum value)
{
	struct varlena *pointer = (struct varlena *) DatumGetPointer(value);

	return !VARATT_IS_COMPRESSED(pointer) && !VARATT_IS_EXTERNAL(pointer);
}

/* As hashtext and hashvarlena hash a value: its bytes, whatever its header. */
static inline uint32
keydict_value_hash(KeyDict *dict, Datum value)
{
	if (dict->bytewise && keydict_plain(value))
		return hash_bytes((const unsigned char *) VARDATA_ANY(DatumGetPointer(value)),
						  VARSIZE_ANY_EXHDR(DatumGetPointer(value)));
	return DatumGetUInt32(FunctionCall1Coll(&dict->hashfn, dict->collation, value));
}

static inline bool
keydict_value_equal(KeyDict *dict, Datum a, Datum b)
{
	if (dict->bytewise && keydict_plain(a) && keydict_plain(b))
	{
		Size		len = VARSIZE_ANY_EXHDR(DatumGetPointer(a));

		return len == VARSIZE_ANY_EXHDR(DatumGetPointer(b)) &&
			memcmp(VARDATA_ANY(DatumGetPointer(a)), VARDATA_ANY(DatumGetPointer(b)),
				   len) == 0;
	}
	return DatumGetBool(FunctionCall2Coll(&dict->eqfn, dict->collation, a, b));
}

/*
 * Filled to three quarters, not simplehash's nine tenths: near that the
 * robin hood runs grow long, and a dictionary of 450 000 values made for
 * the planner's estimate of as many, 86 % full, took 17 % more of the
 * grouping (half the fill did no better).
 */
#define KEYDICT_FILLFACTOR 0.75
#define SH_FILLFACTOR (KEYDICT_FILLFACTOR)
#define SH_PREFIX keydict
#define SH_ELEMENT_TYPE KeyEntry
#define SH_KEY_TYPE Datum
#define SH_KEY value
#define SH_HASH_KEY(tb, key) keydict_value_hash((KeyDict *) (tb)->private_data, key)
#define SH_EQUAL(tb, a, b) keydict_value_equal((KeyDict *) (tb)->private_data, a, b)
#define SH_SCOPE static inline
#define SH_STORE_HASH
#define SH_GET_HASH(tb, a) a->hash
#define SH_DEFINE
#define SH_DECLARE
#include "lib/simplehash.h"

#define SH_PREFIX keyform
#define SH_ELEMENT_TYPE KeyForm
#define SH_KEY_TYPE uint32
#define SH_KEY ref
#define SH_HASH_KEY(tb, key) murmurhash32(key)
#define SH_EQUAL(tb, a, b) ((a) == (b))
#define SH_SCOPE static inline
#define SH_DEFINE
#define SH_DECLARE
#include "lib/simplehash.h"


static const CustomExecMethods agg_exec_methods;
static const TessBatchOps groups_batch_ops;
static TessRowMask distinct_rows(TessAggState *state, AggValue *value, int nrows,
								 const uint32 *group_hashes,
								 const TessRowMask *valid,
								 const TessDatumColumn *argument);
/*
 * The aggregate's functions and initial value, as the core's ExecInitAgg
 * reads them; the states live in the context the stand-in AggState gives
 * the transition functions, one for every generic aggregate of the node.
 */
/*
 * Whether the node folds an aggregate itself, and over which argument: a
 * whole one, or a partial one whose state the node writes as the final
 * aggregation above reads it: the core's own transition value, for a
 * state that is not internal, or the node's own format (own_states), for
 * the node's final aggregation (fast_partial, sum_state_partial).
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

static GenericAgg *
generic_init(TessAggState *state, Aggref *agg)
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
static void
generic_reset(TessAggState *state, GenericAgg *generic)
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
static void
generic_combine(TessAggState *state, GenericAgg *generic, Datum value, bool isnull)
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

static void
generic_accumulate(TessAggState *state, GenericAgg *generic, const TessRowMask *rows)
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

static KeyDict *key_dict_create(TessAggState *state, Oid eqop, Oid type, Oid collation);
static void key_dict_reset(KeyDict *dict, uint64 values);
static void keydict_numbers(KeyDict *dict, const TessDatumColumn *column,
							const TessRowMask *rows, bool insert, Datum *numbers,
							uint32 *hashes);

/* The payload of the record at ref, in the chunk's memory, which the node writes. */
static uint64 *
record_payload(TessAggState *state, uint32 ref)
{
	char	   *record = (char *) state->chunk_bases[ref >> TESS_TABLE_UNIT_BITS] +
		(Size) (ref & ((1u << TESS_TABLE_UNIT_BITS) - 1)) * 8;

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
			uint64	   *payload = record_payload(state, state->offsets[row]);
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
	uint64	   *words = record_payload(state, state->offsets[row]) + slot;
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
	uint64	   *words = record_payload(state, state->offsets[row]) + slot;
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
static void
fast_merge(TessAggState *state, GenericAgg *generic, Datum value, bool isnull)
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
static void
sum_states_accumulate(TessAggState *state, int nsums, const int *indexes,
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
static Datum
sum_state_value(const GenericAgg *generic, const uint64 *words, bool *isnull)
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
static Datum
sum_state_partial(const GenericAgg *generic, const uint64 *words, bool *isnull)
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
static void
generic_group_accumulate(TessAggState *state, int index, const TessRowMask *rows,
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
		sum_states_accumulate(state, 1, &index, rows);
		return;
	}
#endif
	old = MemoryContextSwitchTo(states);

	while ((row = tess_row_mask_next(inserted, row)) >= 0)
	{
		uint64	   *payload = record_payload(state, state->offsets[row]);

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
		uint64	   *payload = record_payload(state, state->offsets[row]);

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
static Datum
generic_value(GenericAgg *generic, bool *isnull)
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

static void
agg_begin(CustomScanState *css, EState *estate, int eflags)
{
	TessAggState *state = (TessAggState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
	TessBuilderConfig builder = TESS_STRUCT_INITIALIZER(TessBuilderConfig);
	TupleTableSlot *result = css->ss.ps.ps_ResultTupleSlot;
	Plan	   *child_plan = linitial(cscan->custom_plans);
	Bitmapset  *projection = NULL;
	List	   *computed = NIL;
	List	   *arguments;
	List	   *more;
	List	   *filters;
	List	   *eqops;
	List	   *keys;
	TessPlanReader *reader;
	int			groups;
	int			index = 0;

	/* The planner puts Material above a batch subtree for these. */
	if (eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK))
		elog(ERROR, "TessAgg supports neither backward scan nor mark/restore");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_agg_node || info.nchildren < 1 || info.nchildren > 2 ||
		info.child_names[0] == NULL || cscan->custom_scan_tlist == NIL)
		elog(ERROR, "TessAgg received a foreign plan");
	reader = tess_plan_reader_create((List *) info.node_data, TESS_AGG_DATA,
									 TESS_AGG_DATA_VERSION);
	arguments = tess_plan_read_list(reader, "arguments");
	more = tess_plan_read_list(reader, "more");
	filters = tess_plan_read_list(reader, "filters");
	eqops = tess_plan_read_int_list(reader, "key_eqops");
	keys = tess_plan_read_list(reader, "keys");
	groups = tess_plan_read_int(reader, "groups");
	state->groups_estimate = (uint64) Max(groups, 0);
	state->setop = tess_plan_read_int(reader, "setop");
	state->partial = tess_plan_read_int(reader, "partial") != 0;
	state->own_states = tess_plan_read_int(reader, "own_states") != 0;
	state->finalize = tess_plan_read_int(reader, "finalize") != 0;
	tess_plan_reader_finish(reader);
	if (state->finalize && (state->partial || state->setop >= 0))
		elog(ERROR, "TessAgg received a foreign plan");
	if (state->own_states && !state->partial && !(state->finalize && keys == NIL))
		elog(ERROR, "TessAgg received a foreign plan");
	if ((state->setop >= 0) != (info.nchildren == 2) ||
		(state->setop >= 0 && (list_length(arguments) != 2 || list_length(keys) == 0)))
		elog(ERROR, "TessAgg received a foreign plan");
	state->nkeys = list_length(keys);
	if (state->nkeys > TESS_TABLE_MAX_KEYS ||
		list_length(arguments) + state->nkeys != list_length(cscan->custom_scan_tlist) ||
		(state->nkeys == 0 && arguments == NIL) ||
		list_length(arguments) > AGG_MAX_GROUPED ||
		list_length(filters) != list_length(arguments) ||
		list_length(eqops) != state->nkeys)
		elog(ERROR, "TessAgg received a foreign plan");
	state->child = ExecInitNode(child_plan, estate, eflags);
	css->custom_ps = list_make1(state->child);
	state->input = tess_input_create(estate->es_query_cxt, state->child);
	state->child_layout = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	tess_plan_get_layout(child_plan, &state->child_layout);
	if (state->setop >= 0)
	{
		Plan	   *right = lsecond(cscan->custom_plans);

		state->sides[0] = state->child;
		state->side_inputs[0] = state->input;
		state->side_layouts[0] = state->child_layout;
		state->sides[1] = ExecInitNode(right, estate, eflags);
		css->custom_ps = lappend(css->custom_ps, state->sides[1]);
		state->side_inputs[1] = tess_input_create(estate->es_query_cxt, state->sides[1]);
		state->side_layouts[1] = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
		tess_plan_get_layout(right, &state->side_layouts[1]);
		state->setop_copies = -1;
	}
	state->nvalues = list_length(arguments);
	state->values = palloc0_array(AggValue, Max(state->nvalues, 1));
	state->status = (TessStatus) TESS_STRUCT_INITIALIZER(TessStatus);
	/* The keys are the first computed columns, the table's key kinds. */
	foreach_ptr(Node, key, keys)
	{
		int			position = foreach_current_index(key);

		if (list_nth_int(eqops, position) != 0)
		{
			state->kinds[position] = TESS_TABLE_KEY_INT8;
			state->dicts[position] = key_dict_create(state, (Oid) list_nth_int(eqops, position),
													exprType(key), exprCollation(key));
			state->has_dicts = true;
			state->has_forms |= state->dicts[position]->forms;
			state->row_spill = true;
		}
		else if (!tess_word_key_kind(exprType(key), &state->kinds[position]))
			elog(ERROR, "TessAgg received a key of type %u", exprType(key));
		computed = lappend(computed,
						   makeTargetEntry((Expr *) key, position + 1, NULL, false));
		foreach_ptr(Var, var, pull_var_clause(key, 0))
		{
			int			column = var->varno == INDEX_VAR ?
				tess_layout_column(&state->child_layout, var->varattno - 1) : -1;

			if (column < 0)
				elog(ERROR, "TessAgg grouping expression names no column of its child");
			projection = bms_add_member(projection, column);
		}
	}
	foreach_ptr(TargetEntry, entry, cscan->custom_scan_tlist)
	{
		Aggref	   *agg;
		AggValue   *value;

		if (foreach_current_index(entry) < state->nkeys)
			continue;
		agg = castNode(Aggref, entry->expr);
		value = &state->values[index++];

		/* The partial values are the whole ones' types: nothing to convert. */
		if (DO_AGGSPLIT_SKIPFINAL(agg->aggsplit) != state->partial)
			elog(ERROR, "TessAgg received a foreign plan");
		value->kind = aggregate_kind(agg->aggfnoid);
		value->wide = agg->aggtranstype == INT8OID;
		value->function = tess_runtime_api()->functions->find(agg->aggfnoid);
		value->computed = -1;
		value->filter = -1;
		if (!batch_aggregate(agg))
		{
			if (!generic_supported(agg) ||
				(state->nkeys > 0 && DO_AGGSPLIT_SKIPFINAL(agg->aggsplit) &&
				 !(state->own_states && sum_state_aggregate(agg))))
				elog(ERROR, "TessAgg has no implementation of %s",
					 format_procedure(agg->aggfnoid));
			value->kind = AGG_GENERIC;
			value->generic = generic_init(state, agg);
			if (state->nkeys > 0 && state->generic_output == NULL)
			{
				state->has_generic = true;
				state->row_spill = true;
				state->generic_output = AllocSetContextCreate(estate->es_query_cxt,
															  "TessAgg generic values",
															  ALLOCSET_DEFAULT_SIZES);
			}
		}
		/*
		 * A partial value is the transition type's: an extreme's, int8 for
		 * the rest; with groups a sum state's the node's own
		 * (TessTableSumInput), without them a generic one's the core's,
		 * which its combine function takes.
		 */
		if (state->finalize && ((value->kind == AGG_GENERIC && state->nkeys > 0 &&
								 !value->generic->sum_state) ||
								agg->aggdistinct != NIL))
			elog(ERROR, "TessAgg received a foreign plan");
		if (state->partial && value->kind == AGG_GENERIC && state->nkeys > 0 &&
			!value->generic->sum_state)
			elog(ERROR, "TessAgg received a foreign plan");
		switch (value->kind)
		{
			case AGG_COUNT:
				value->accumulate = state->finalize ? TESS_TABLE_SUM_INT8 :
					agg->args == NIL ? TESS_TABLE_COUNT_ROWS : TESS_TABLE_COUNT;
				break;
			case AGG_SUM:
				value->accumulate = state->finalize ? TESS_TABLE_SUM_INT8 :
					TESS_TABLE_SUM_INT4;
				break;
			case AGG_MIN:
				value->accumulate = value->wide ? TESS_TABLE_MIN_INT8 :
					TESS_TABLE_MIN_INT4;
				break;
			case AGG_MAX:
				value->accumulate = value->wide ? TESS_TABLE_MAX_INT8 :
					TESS_TABLE_MAX_INT4;
				break;
			case AGG_GENERIC:
				break;
		}
		if (agg->args != NIL || state->finalize)
		{
			Node	   *argument = list_nth(arguments, index - 1);
			List	   *vars = pull_var_clause(argument, 0);

			value->computed = list_length(computed);
			computed = lappend(computed,
							   makeTargetEntry((Expr *) argument,
											   value->computed + 1, NULL, false));
			/* The other arguments of a generic aggregate follow the first. */
			foreach_ptr(Node, other, (List *) list_nth(more, index - 1))
			{
				computed = lappend(computed,
								   makeTargetEntry((Expr *) other, list_length(computed) + 1,
												   NULL, false));
				vars = list_concat(vars, pull_var_clause(other, 0));
			}
			foreach_ptr(Var, var, vars)
			{
				int			column = var->varno == INDEX_VAR ?
					tess_layout_column(&state->child_layout, var->varattno - 1) : -1;

				if (column < 0)
					elog(ERROR, "TessAgg argument names no column of its child");
				projection = bms_add_member(projection, column);
			}
			value->gathered_values = palloc_array(Datum, 64);
			value->gathered_isnull = palloc_array(bool, 64);
			if (agg->aggdistinct != NIL)
			{
				SortGroupClause *clause = linitial_node(SortGroupClause, agg->aggdistinct);

				if (!tess_word_key_kind(exprType(argument), &value->argument_kind))
				{
					value->argument_kind = TESS_TABLE_KEY_INT8;
					value->distinct_dict = key_dict_create(state, clause->eqop,
														   exprType(argument),
														   exprCollation(argument));
					/* Only the count of the values matters, not a form to put out. */
					value->distinct_dict->forms = false;
				}
				value->distinct = palloc0(sizeof(struct DistinctSet));
				state->has_distinct = true;
			}
		}
	}
	/* The FILTER conditions, computed columns after every argument. */
	foreach_ptr(Node, filter, filters)
	{
		AggValue   *value = &state->values[foreach_current_index(filter)];

		if (filter == NULL)
			continue;
		value->filter = list_length(computed);
		computed = lappend(computed,
						   makeTargetEntry((Expr *) filter, value->filter + 1, NULL, false));
		foreach_ptr(Var, var, pull_var_clause(filter, 0))
		{
			int			column = var->varno == INDEX_VAR ?
				tess_layout_column(&state->child_layout, var->varattno - 1) : -1;

			if (column < 0)
				elog(ERROR, "TessAgg filter names no column of its child");
			projection = bms_add_member(projection, column);
		}
	}
	state->ncomputed = list_length(computed);
	state->computed_lens = palloc_array(int16, Max(state->ncomputed, 1));
	state->computed_byvals = palloc_array(bool, Max(state->ncomputed, 1));
	state->computed_columns = palloc0_array(TessDatumColumn, Max(state->ncomputed, 1));
	foreach_node(TargetEntry, entry, computed)
	{
		int			column = foreach_current_index(entry);

		get_typlenbyval(exprType((Node *) entry->expr), &state->computed_lens[column],
						&state->computed_byvals[column]);
	}
	if (computed != NIL)
	{
		/* The arguments are computed columns over the child's target list. */
		TessProjectionConfig config = TESS_STRUCT_INITIALIZER(TessProjectionConfig);

		config.parent_context = estate->es_query_cxt;
		config.parent = &css->ss.ps;
		config.econtext = css->ss.ps.ps_ExprContext;
		config.scan_slot = ExecInitExtraTupleSlot(estate,
												  ExecTypeFromTL(child_plan->targetlist),
												  &TTSOpsVirtual);
		config.scan_tuple = &state->child_layout;
		config.base_columns = state->child_layout.ncolumns;
		config.computed = computed;
		state->projection = tess_projection_create(&config);
		/*
		 * The right side: the same keys, its columns by position, and its
		 * side a constant 1 where the left side's is 0.
		 */
		if (state->setop >= 0)
		{
			List	   *right = copyObject(computed);
			TargetEntry *side = list_nth_node(TargetEntry, right, state->values[1].computed);
			Plan	   *plan = lsecond(cscan->custom_plans);
			Bitmapset  *columns = NULL;
			TessRequest side_request = TESS_STRUCT_INITIALIZER(TessRequest);

			if (!IsA(side->expr, Const))
				elog(ERROR, "TessAgg received a foreign plan");
			side->expr = (Expr *) makeConst(INT4OID, -1, InvalidOid, sizeof(int32),
											Int32GetDatum(1), false, true);
			state->side_projections[0] = state->projection;
			config.scan_slot = ExecInitExtraTupleSlot(estate,
													  ExecTypeFromTL(plan->targetlist),
													  &TTSOpsVirtual);
			config.scan_tuple = &state->side_layouts[1];
			config.base_columns = state->side_layouts[1].ncolumns;
			config.computed = right;
			state->side_projections[1] = tess_projection_create(&config);
			foreach_node(TargetEntry, entry, right)
				foreach_node(Var, var, pull_var_clause((Node *) entry->expr, 0))
				{
					int			column = var->varno == INDEX_VAR ?
						tess_layout_column(&state->side_layouts[1], var->varattno - 1) : -1;

					if (column < 0)
						elog(ERROR, "TessAgg key names no column of its right side");
					columns = bms_add_member(columns, column);
				}
			side_request.projection_columns = columns;
			side_request.output_mode = TESS_OUTPUT_BATCH;
			tess_input_set_request(state->side_inputs[1], &side_request);
		}
	}
	/* Whole batches; the arguments' columns only for the surviving rows. */
	request.projection_columns = projection;
	/*
	 * Read in order only when the keys and the arguments, as computed, would
	 * first ask for a column before one they asked for already: else the
	 * provider walks each row once anyway, and the calls are wasted.
	 */
	{
		int			last = -1;
		bool		ascending = true;

		foreach_node(TargetEntry, entry, computed)
			foreach_node(Var, var, pull_var_clause((Node *) entry->expr, 0))
			{
				int			column = var->varno == INDEX_VAR ?
					tess_layout_column(&state->child_layout, var->varattno - 1) : -1;

				if (column >= 0 && column < last)
					ascending = false;
				last = Max(last, column);
			}
		state->read_columns = palloc_array(int, Max(bms_num_members(projection), 1));
		/* Two sides of two layouts: their columns come as they are asked for. */
		for (int column = -1;
			 !ascending && state->setop < 0 &&
			 (column = bms_next_member(projection, column)) >= 0;)
			state->read_columns[state->nread_columns++] = column;
	}
	/* The states' places in a record: a word each, a sum state's more. */
	{
		int			slot = 1;

		for (int index = 0; index < state->nvalues; index++)
		{
			AggValue   *value = &state->values[index];

			value->slot = slot;
			slot += value->generic != NULL && value->generic->sum_state ?
				AGG_SUM_STATE_WORDS : 1;
		}
		state->payload_size = sizeof(uint64) * slot;
		state->sum_indexes = palloc_array(int, Max(state->nvalues, 1));
	}
	request.output_mode = TESS_OUTPUT_BATCH;
	tess_input_set_request(state->input, &request);
	builder.parent_context = estate->es_query_cxt;
	builder.tuple_desc = result->tts_tupleDescriptor;
	builder.ncolumns = result->tts_tupleDescriptor->natts;
	builder.capacity = state->nkeys > 0 ? AGG_GROUP_ROWS : 1;
	if (state->nkeys > 0)
	{
		state->kernels = tess_runtime_kernels();
		if (state->kernels == NULL ||
			!TESS_ABI_HAS_FIELD(state->kernels, TessKernelOps, table_combine))
			elog(ERROR, "TessAgg needs the kernels module for GROUP BY");
		state->table_context = AllocSetContextCreate(estate->es_query_cxt,
													 "TessAgg groups",
													 ALLOCSET_DEFAULT_SIZES);
		/* For spilling: how the states merge, and the layout's fingerprint. */
		state->combines = palloc_array(TessTableCombine, Max(state->nvalues, 1));
		for (int value = 0; value < state->nvalues; value++)
			state->combines[value] =
				state->values[value].kind == AGG_COUNT ? TESS_TABLE_COMBINE_COUNT :
				state->values[value].kind == AGG_SUM ? TESS_TABLE_COMBINE_SUM :
				state->values[value].kind == AGG_MIN ? TESS_TABLE_COMBINE_MIN :
				TESS_TABLE_COMBINE_MAX;
		if (state->kernels->table_size(state->nkeys, state->kinds,
									   state->payload_size,
									   AGG_INITIAL_GROUPS, &state->layout_len,
									   &state->status) != TESS_OK)
			tess_status_report(&state->status);
		state->layout_index = palloc0(state->layout_len);
		if (state->kernels->table_create(state->layout_index, state->layout_len,
										 state->nkeys, state->kinds,
										 state->payload_size,
										 AGG_INITIAL_GROUPS, &state->status) != TESS_OK)
			tess_status_report(&state->status);
		state->state_words = palloc0_array(uint64,
										   Max(state->nvalues, 1) * AGG_GROUP_ROWS);
		if (state->projection == NULL)
			elog(ERROR, "TessAgg groups by computed columns");
	}
	/* DISTINCT in an aggregate keeps its pairs in tables, with or without groups. */
	if (state->has_distinct && state->kernels == NULL)
	{
		state->kernels = tess_runtime_kernels();
		if (state->kernels == NULL ||
			!TESS_ABI_HAS_FIELD(state->kernels, TessKernelOps, table_combine))
			elog(ERROR, "TessAgg needs the kernels module for DISTINCT");
	}
	state->builder = tess_builder_create(&builder);
	if (state->nkeys > 0 && state->setop < 0 && !state->has_generic &&
		css->ss.ps.qual == NULL && css->ss.ps.ps_ProjInfo == NULL)
	{
		state->direct = true;
		state->agg_values = palloc0_array(Datum, Max(state->nvalues, 1) * AGG_GROUP_ROWS);
		state->agg_isnull = palloc0_array(bool, Max(state->nvalues, 1) * AGG_GROUP_ROWS);
		state->groups_batch.abi_version = TESS_BATCH_ABI_VERSION;
		state->groups_batch.struct_size = sizeof(TessBatch);
		state->groups_batch.table_oid = InvalidOid;
		state->groups_batch.ops = &groups_batch_ops;
		state->groups_batch.private_data = state;
	}
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   result, &info.layout);
}

/*
 * A partial value into the running value, a batch's or, above a gather, a
 * participant's: counts and sums added as int8, 22003 past the range,
 * extremes compared.
 */
static void
join_partial(AggValue *value, Datum partial)
{
	switch (value->kind)
	{
		case AGG_COUNT:
		case AGG_SUM:
			if (pg_add_s64_overflow(value->total, DatumGetInt64(partial),
									&value->total))
				ereport(ERROR,
						(errcode(ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE),
						 errmsg("bigint out of range")));
			break;
		case AGG_MIN:
		case AGG_MAX:
			{
				int64		found = value->wide ? DatumGetInt64(partial) :
					(int64) DatumGetInt32(partial);

				if (!value->has_value ||
					(value->kind == AGG_MIN ? found < value->extreme :
					 found > value->extreme))
					value->extreme = found;
				break;
			}
		case AGG_GENERIC:
			break;
	}
	value->has_value = true;
}

/*
 * One call of the aggregate's batch function over a column, or over rows
 * alone for count(*); the partial joins the running value. No readiness
 * mask: the column's rows are all initialized memory (tessera/batch.h),
 * and a mask would keep the kernel off its vector path for every word the
 * selection does not fill.
 */
static void
evaluate(TessAggState *state, AggValue *value, const TessDatumColumn *column,
		 TessRowMask *rows)
{
	TessFunctionCall call = TESS_STRUCT_INITIALIZER(TessFunctionCall);
	TessFunctionArg arg = TESS_STRUCT_INITIALIZER(TessFunctionArg);
	uint64		word = 0;
	TessRowMask present = {1, &word};
	Datum		partial = (Datum) 0;

	call.function = value->function;
	if (column != NULL)
	{
		arg.column = column;
		call.nargs = 1;
		call.args = &arg;
	}
	call.rows = rows;
	call.values = &partial;
	call.non_nulls = &present;
	call.context = CurrentMemoryContext;
	call.status = &state->status;
	state->calls++;
	if (value->function->evaluate(&call) != TESS_OK)
		tess_status_report(&state->status);
	if ((word & 1) == 0)
		return;
	join_partial(value, partial);
}

/* The gathered values as a column with every row selected, in one call. */
static void
flush_gathered(TessAggState *state, AggValue *value)
{
	TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	uint64		word;
	TessRowMask rows = {value->ngathered, &word};

	if (value->ngathered == 0)
		return;
	column.values = value->gathered_values;
	column.isnull = value->gathered_isnull;
	column.nrows = value->ngathered;
	word = value->ngathered == 64 ? UINT64_MAX :
		(UINT64CONST(1) << value->ngathered) - 1;
	evaluate(state, value, &column, &rows);
	value->ngathered = 0;
}

/*
 * The rows of those given that an aggregate's FILTER keeps: its computed
 * column is true there and NULL elsewhere (filter_value), asked for those
 * rows only; in the node's buffer, which the aggregate uses before the
 * next one asks.
 */
static TessRowMask
filtered_rows(TessAggState *state, TessBatch *batch, int filter, const TessRowMask *rows)
{
	TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	int			nwords = tess_row_mask_word_count(rows->nrows);
	int			row = -1;

	if (state->filter_words < nwords)
	{
		state->filter_bits = MemoryContextAlloc(state->css.ss.ps.state->es_query_cxt,
												sizeof(uint64) * nwords);
		state->filter_words = nwords;
	}
	batch->ops->get_datum_column(batch, state->child_layout.ncolumns + filter, rows,
								 TESS_COLUMN_FOR_FILTER, &column);
	if (column.values == NULL || column.isnull == NULL || column.nrows != rows->nrows)
		elog(ERROR, "Tessera projection returned an invalid column");
	for (int word = 0; word < nwords; word++)
	{
		uint64		selected = rows->bits[word];
		uint64		kept = 0;
		int			base = word * 64;

		/* A whole word without a branch a row; a partial one row by row. */
		if (selected == UINT64_MAX)
		{
			for (int bit = 0; bit < 64; bit++)
				kept |= (uint64) (!column.isnull[base + bit] &
								  (DatumGetBool(column.values[base + bit]) ? 1 : 0)) << bit;
		}
		else
		{
			while (selected != 0)
			{
				int			bit = pg_rightmost_one_pos64(selected);

				row = base + bit;
				if (!column.isnull[row] && DatumGetBool(column.values[row]))
					kept |= UINT64CONST(1) << bit;
				selected &= selected - 1;
			}
		}
		state->filter_bits[word] = kept;
	}
	return (TessRowMask) {rows->nrows, state->filter_bits};
}

/*
 * Whether an aggregate reads its argument's decimals (TessDatumColumn): a
 * numeric one the node folds itself, not DISTINCT, whose pairs hash the
 * argument's Datums.
 */
static inline bool
fast_decimals(const AggValue *value)
{
	return value->generic != NULL && value->generic->fast != FAST_NONE &&
		value->generic->fast_numeric && value->distinct == NULL;
}

/*
 * Add one batch to the aggregate: its partial through the batch function,
 * or, for a batch with few survivors, their values gathered into a column
 * of the aggregate's own, since a call costs more than the rows it would
 * sum; the column is evaluated when it fills or the input ends. The batch
 * is the projection's wrapper, which computes the argument's column.
 */
static void
accumulate(TessAggState *state, AggValue *value, TessBatch *batch, int nrows)
{
	TessDatumColumn computed = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	const TessDatumColumn *column = &computed;
	TessRowMask rows = batch->rows;
	int			row = -1;

	if (value->filter >= 0)
	{
		rows = filtered_rows(state, batch, value->filter, &batch->rows);
		nrows = tess_row_mask_count(&rows);
	}
	if (value->computed < 0)
	{
		evaluate(state, value, NULL, &rows);
		return;
	}
	/* A numeric aggregate of its own reads its argument's decimals. */
	computed.accept_decimals = fast_decimals(value);
	batch->ops->get_datum_column(batch,
								 state->child_layout.ncolumns + value->computed,
								 &rows, TESS_COLUMN_FOR_PROJECTION, &computed);
	if (computed.values == NULL || computed.isnull == NULL ||
		computed.nrows != batch->rows.nrows)
		elog(ERROR, "Tessera projection returned an invalid column");
	/*
	 * Above a gather: a row a participant's partial value, NULL skipped (a
	 * participant without rows gives a count of 0 and NULL otherwise), not
	 * the rows a batch function would count.
	 */
	if (state->finalize)
	{
		while ((row = tess_row_mask_next(&rows, row)) >= 0)
		{
#ifdef HAVE_INT128
			if (value->kind == AGG_GENERIC && value->generic->fast != FAST_NONE)
				fast_merge(state, value->generic, computed.values[row],
						   computed.isnull[row]);
			else
#endif
			if (value->kind == AGG_GENERIC)
				generic_combine(state, value->generic, computed.values[row],
								computed.isnull[row]);
			else if (!computed.isnull[row])
				join_partial(value, computed.values[row]);
		}
		return;
	}
	if (value->generic != NULL)
	{
		value->generic->columns[0] = computed;
		for (int arg = 1; arg < value->generic->nargs; arg++)
		{
			TessDatumColumn *other = &value->generic->columns[arg];

			*other = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
			batch->ops->get_datum_column(batch,
										 state->child_layout.ncolumns + value->computed + arg,
										 &rows, TESS_COLUMN_FOR_PROJECTION, other);
			if (other->values == NULL || other->isnull == NULL ||
				other->nrows != batch->rows.nrows)
				elog(ERROR, "Tessera projection returned an invalid column");
		}
		if (value->distinct != NULL)
		{
			TessRowMask pairs = distinct_rows(state, value, rows.nrows, NULL, &rows, column);

			generic_accumulate(state, value->generic, &pairs);
		}
		else
			generic_accumulate(state, value->generic, &rows);
		return;
	}
	if (value->distinct != NULL)
	{
		TessRowMask pairs = distinct_rows(state, value, rows.nrows, NULL, &rows, column);

		evaluate(state, value, column, &pairs);
		return;
	}
	if (nrows > AGG_GATHER_ROWS)
	{
		evaluate(state, value, column, &rows);
		return;
	}
	while ((row = tess_row_mask_next(&rows, row)) >= 0)
	{
		if (value->ngathered == 64)
			flush_gathered(state, value);
		value->gathered_values[value->ngathered] = column->values[row];
		value->gathered_isnull[value->ngathered] = column->isnull[row];
		value->ngathered++;
	}
}

static void read_in_order(TessAggState *state, TessBatch *batch);
static void distinct_reset(TessAggState *state, AggValue *value);

/* Empty every distinct set, before the input is read. */
static void
reset_distinct(TessAggState *state)
{
	for (int index = 0; index < state->nvalues; index++)
		if (state->values[index].distinct != NULL)
			distinct_reset(state, &state->values[index]);
}

/* Read every batch of the child into the running values. */
static void
drain(TessAggState *state)
{
	reset_distinct(state);
	/* The states of a previous scan go, with the callbacks they registered. */
	if (state->generic_agg != NULL)
		ReScanExprContext(state->generic_agg->curaggcontext);
	for (int index = 0; index < state->nvalues; index++)
		if (state->values[index].generic != NULL)
			generic_reset(state, state->values[index].generic);
	for (;;)
	{
		TessBatch  *batch = tess_input_next(state->input);
		int			rows;

		if (batch == NULL)
			break;
		rows = tess_row_mask_count(&batch->rows);
		state->batches++;
		state->rows += rows;
		if (rows > 0)
		{
			TessBatch  *input = batch;

			ResetExprContext(state->css.ss.ps.ps_ExprContext);
			if (state->projection != NULL)
				input = tess_projection_wrap(state->projection, batch);
			read_in_order(state, input);
			for (int index = 0; index < state->nvalues; index++)
				accumulate(state, &state->values[index], input, rows);
			if (state->projection != NULL)
				input->ops->release(input);
		}
		tess_input_finish(state->input);
	}
	for (int index = 0; index < state->nvalues; index++)
		flush_gathered(state, &state->values[index]);
}

/* The one result row: the aggregates in the scan slot. */
static TupleTableSlot *
result_row(TessAggState *state)
{
	TupleTableSlot *scan = state->css.ss.ss_ScanTupleSlot;

	ExecClearTuple(scan);
	for (int index = 0; index < state->nvalues; index++)
	{
		AggValue   *value = &state->values[index];

		scan->tts_isnull[index] = !value->has_value;
		switch (value->kind)
		{
			case AGG_COUNT:
				scan->tts_values[index] = Int64GetDatum(value->total);
				scan->tts_isnull[index] = false;
				break;
			case AGG_SUM:
				scan->tts_values[index] = Int64GetDatum(value->total);
				break;
			case AGG_MIN:
			case AGG_MAX:
				scan->tts_values[index] = value->wide ?
					Int64GetDatum(value->extreme) :
					Int32GetDatum((int32) value->extreme);
				break;
			case AGG_GENERIC:
				scan->tts_values[index] = generic_value(value->generic,
														&scan->tts_isnull[index]);
				break;
		}
	}
	return ExecStoreVirtualTuple(scan);
}

static Size distinct_bytes(TessAggState *state);

/*
 * The bytes of the table now, and the most so far; once it spills, the
 * index and every chunk of every level, which live in the levels' memory.
 */
void
note_memory(TessAggState *state)
{
	Size		memory = state->spill == NULL ? state->table_bytes :
		agg_spill_memory(state);

	if (state->has_distinct)
		memory += distinct_bytes(state);
	state->peak_memory = Max(state->peak_memory, memory);
}

/*
 * The groups' records lie in chunks: the first of AGG_FIRST_CHUNK bytes,
 * so that a few groups take little, the others of the most a chunk may
 * have. Records never move; when they reach half the buckets, only the
 * index is made anew, larger.
 */
#define AGG_FIRST_CHUNK (64 * 1024)

/*
 * A first index for capacity groups at most, and at most a quarter of
 * hash_mem, about 8 bytes of buckets per group: an estimate too large
 * would take the memory the groups need; the index grows as they come.
 */
uint64
first_capacity(uint64 capacity)
{
	uint64		most = get_hash_memory_limit() / 32;

	return Max(Min(capacity, most), AGG_INITIAL_GROUPS);
}

/* An index for capacity groups in the table's memory. */
void *
new_index(TessAggState *state, uint64 capacity, Size *size)
{
	Size		payload_size = state->payload_size;

	check(state, state->kernels->table_size(state->nkeys, state->kinds,
											payload_size, capacity, size,
											&state->status));
	return MemoryContextAllocExtended(state->table_context, *size, MCXT_ALLOC_HUGE);
}

/* An empty table of groups, its index sized for the planner's estimate. */
static void
create_table(TessAggState *state)
{
	Size		payload_size = state->payload_size;
	uint64		capacity = first_capacity(state->groups_estimate);
	Size		size;

	MemoryContextReset(state->table_context);
	state->capacity = 0;
	state->chunk_slots = 16;
	state->chunk_bases = MemoryContextAlloc(state->table_context,
											sizeof(void *) * state->chunk_slots);
	state->chunk_lens = MemoryContextAlloc(state->table_context,
										   sizeof(Size) * state->chunk_slots);
	state->table.index = new_index(state, capacity, &size);
	state->table.index_len = size;
	state->table.chunks = state->chunk_bases;
	state->table.chunk_lens = state->chunk_lens;
	state->table.nchunks = 0;
	state->table_bytes = size;
	check(state, state->kernels->table_create(state->table.index, size, state->nkeys,
											  state->kinds, payload_size,
											  capacity, &state->status));
	note_memory(state);
}

/* Another chunk of records, the last one being full. */
static void
add_chunk(TessAggState *state)
{
	int			chunk = state->table.nchunks;
	/* Past the first, an eighth of hash_mem, so that a small one spills late. */
	Size		len = chunk == 0 ? AGG_FIRST_CHUNK :
		Max(AGG_FIRST_CHUNK, Min(TESS_TABLE_MAX_CHUNK_LEN,
								 TYPEALIGN_DOWN(8, get_hash_memory_limit() / 8)));
	void	   *base;

	if (chunk == TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessAgg group table cannot hold more than %d chunks",
						TESS_TABLE_MAX_CHUNKS)));
	if (chunk == state->chunk_slots)
	{
		state->chunk_slots *= 2;
		state->chunk_bases = repalloc(state->chunk_bases,
									  sizeof(void *) * state->chunk_slots);
		state->chunk_lens = repalloc(state->chunk_lens,
									 sizeof(Size) * state->chunk_slots);
		state->table.chunks = state->chunk_bases;
		state->table.chunk_lens = state->chunk_lens;
	}
	base = MemoryContextAlloc(state->table_context, len);
	check(state, state->kernels->table_chunk_init(base, len, &state->status));
	state->chunk_bases[chunk] = base;
	state->chunk_lens[chunk] = len;
	state->table.nchunks++;
	state->table_bytes += len;
	note_memory(state);
}

/*
 * An index for twice the groups: the buckets are filled anew from the
 * records, which stay where they are, and the old index is freed.
 */
void
regrow_table(TessAggState *state, uint64 groups)
{
	void	   *old = state->table.index;
	Size		size;
	void	   *index;

	/* find_or_insert stopped at half the buckets, over existing chunks. */
	Assert(groups > 0 && state->table.nchunks > 0);
	index = new_index(state, groups * 2, &size);

	check(state, state->kernels->table_regrow(&state->table, index, size,
											  groups * 2, &state->status));
	state->table_bytes = state->table_bytes - state->table.index_len + size;
	state->peak_memory = Max(state->peak_memory, state->table_bytes +
							 state->table.index_len);
	state->table.index = index;
	state->table.index_len = size;
	pfree(old);
	state->grows++;
}

/* The buffers of a batch of nrows rows, in the table's memory. */
static void
reserve_rows(TessAggState *state, int nrows)
{
	int			nwords = tess_row_mask_word_count(nrows);

	if (state->capacity >= nrows)
		return;
	state->hashes = MemoryContextAlloc(state->table_context, sizeof(uint32) * nrows);
	state->offsets = MemoryContextAlloc(state->table_context, sizeof(uint32) * nrows);
	state->valid_bits = MemoryContextAlloc(state->table_context, sizeof(uint64) * nwords);
	state->pending_bits = MemoryContextAlloc(state->table_context, sizeof(uint64) * nwords);
	state->inserted_bits = MemoryContextAlloc(state->table_context, sizeof(uint64) * nwords);
	state->call_bits = MemoryContextAlloc(state->table_context, sizeof(uint64) * nwords);
	state->sum_rest_bits = MemoryContextAlloc(state->table_context,
											  sizeof(uint64) * nwords * TESS_TABLE_MAX_SUMS);
	state->capacity = nrows;
}

/* An index of the pairs for capacity of them in the set's memory. */
static void *
distinct_index(TessAggState *state, DistinctSet *set, uint64 capacity, Size *size)
{
	check(state, state->kernels->table_size(set->nkeys, set->kinds, 0, capacity,
											size, &state->status));
	return MemoryContextAllocExtended(set->context, *size, MCXT_ALLOC_HUGE);
}

/* An empty set: the groups' keys, then the argument. */
static void
distinct_reset(TessAggState *state, AggValue *value)
{
	DistinctSet *set = value->distinct;
	uint64		capacity = AGG_INITIAL_GROUPS;
	Size		size;

	if (set->context == NULL)
		set->context = AllocSetContextCreate(state->css.ss.ps.state->es_query_cxt,
											 "TessAgg distinct",
											 ALLOCSET_DEFAULT_SIZES);
	MemoryContextReset(set->context);
	set->nkeys = state->nkeys + 1;
	for (int key = 0; key < state->nkeys; key++)
		set->kinds[key] = state->kinds[key];
	set->kinds[state->nkeys] = value->argument_kind;
	set->capacity = 0;
	set->slots = 16;
	set->bases = MemoryContextAlloc(set->context, sizeof(void *) * set->slots);
	set->lens = MemoryContextAlloc(set->context, sizeof(Size) * set->slots);
	set->table.index = distinct_index(state, set, capacity, &size);
	set->table.index_len = size;
	set->table.chunks = set->bases;
	set->table.chunk_lens = set->lens;
	set->table.nchunks = 0;
	set->bytes = size;
	check(state, state->kernels->table_create(set->table.index, size, set->nkeys,
											  set->kinds, 0, capacity,
											  &state->status));
	if (value->distinct_dict != NULL)
		key_dict_reset(value->distinct_dict, 256);
}

static void
distinct_add_chunk(TessAggState *state, DistinctSet *set)
{
	int			chunk = set->table.nchunks;
	Size		len = chunk == 0 ? AGG_FIRST_CHUNK : TESS_TABLE_MAX_CHUNK_LEN;
	void	   *base;

	if (chunk == TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessAgg distinct table cannot hold more than %d chunks",
						TESS_TABLE_MAX_CHUNKS)));
	if (chunk == set->slots)
	{
		set->slots *= 2;
		set->bases = repalloc(set->bases, sizeof(void *) * set->slots);
		set->lens = repalloc(set->lens, sizeof(Size) * set->slots);
		set->table.chunks = set->bases;
		set->table.chunk_lens = set->lens;
	}
	base = MemoryContextAlloc(set->context, len);
	check(state, state->kernels->table_chunk_init(base, len, &state->status));
	set->bases[chunk] = base;
	set->lens[chunk] = len;
	set->table.nchunks++;
	set->bytes += len;
}

static void
distinct_regrow(TessAggState *state, DistinctSet *set, uint64 records)
{
	void	   *old = set->table.index;
	Size		size;
	void	   *index = distinct_index(state, set, records * 2, &size);

	check(state, state->kernels->table_regrow(&set->table, index, size,
											  records * 2, &state->status));
	set->bytes = set->bytes - set->table.index_len + size;
	set->table.index = index;
	set->table.index_len = size;
	pfree(old);
}

/*
 * The rows of valid, of a batch of nrows rows, that insert their pair into
 * the aggregate's set: a mask in the set's buffers. group_hashes are the
 * rows' hashes of the groups' keys, state->table_keys their keys, or NULL
 * without GROUP BY.
 */
static TessRowMask
distinct_rows(TessAggState *state, AggValue *value, int nrows,
			  const uint32 *group_hashes, const TessRowMask *valid,
			  const TessDatumColumn *argument)
{
	DistinctSet *set = value->distinct;
	int			nwords = tess_row_mask_word_count(nrows);
	TessTableKey keys[TESS_TABLE_MAX_KEYS];
	TessRowMask pending;
	TessRowMask inserted;
	TessDatumColumn numbers;
	bool		int8 = value->argument_kind == TESS_TABLE_KEY_INT8;

	/* A value a word does not hold: the pairs take its number in the dictionary. */
	if (value->distinct_dict != NULL)
	{
		KeyDict    *dict = value->distinct_dict;

		if (dict->capacity < nrows)
		{
			MemoryContext query = state->css.ss.ps.state->es_query_cxt;

			dict->capacity = nrows;
			dict->batch_numbers = MemoryContextAlloc(query, sizeof(Datum) * nrows);
			dict->batch_hashes = MemoryContextAlloc(query, sizeof(uint32) * nrows);
		}
		keydict_numbers(dict, argument, valid, true, dict->batch_numbers, dict->batch_hashes);
		numbers = *argument;
		numbers.values = dict->batch_numbers;
		argument = &numbers;
	}
	if (set->capacity < nrows)
	{
		set->hashes = MemoryContextAlloc(set->context, sizeof(uint32) * nrows);
		set->offsets = MemoryContextAlloc(set->context, sizeof(uint32) * nrows);
		set->pending_bits = MemoryContextAlloc(set->context, sizeof(uint64) * nwords);
		set->inserted_bits = MemoryContextAlloc(set->context, sizeof(uint64) * nwords);
		set->call_bits = MemoryContextAlloc(set->context, sizeof(uint64) * nwords);
		set->capacity = nrows;
	}
	pending = (TessRowMask) {nrows, set->pending_bits};
	inserted = (TessRowMask) {nrows, set->inserted_bits};
	memset(set->inserted_bits, 0, sizeof(uint64) * nwords);
	/* The groups' hashes folded with the argument's; a NULL one drops out. */
	if (group_hashes != NULL)
	{
		memcpy(set->hashes, group_hashes, sizeof(uint32) * nrows);
		memcpy(set->pending_bits, valid->bits, sizeof(uint64) * nwords);
		check(state, (int8 ? state->kernels->int8_hash_next :
					  state->kernels->int4_hash_next) (argument, NULL,
													   TESS_NULL_KEYS_REJECT,
													   set->hashes, &pending,
													   &state->status));
	}
	else
	{
		memset(set->pending_bits, 0, sizeof(uint64) * nwords);
		check(state, (int8 ? state->kernels->int8_hash :
					  state->kernels->int4_hash) (argument, NULL, valid,
												  TESS_NULL_KEYS_REJECT,
												  set->hashes, &pending,
												  &state->status));
	}
	for (int key = 0; key < state->nkeys; key++)
		keys[key] = state->table_keys[key];
	keys[state->nkeys].kind = value->argument_kind;
	keys[state->nkeys].column = argument;
	keys[state->nkeys].prepared = NULL;
	if (set->table.nchunks == 0)
		distinct_add_chunk(state, set);
	for (;;)
	{
		TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
		TessRowMask call = {nrows, set->call_bits};

		/*
		 * Each call fills its mask of new pairs whole: they add up. A mask
		 * has no bits past its rows on entry, which a longer batch left.
		 */
		memset(set->call_bits, 0, sizeof(uint64) * nwords);
		check(state, state->kernels->table_find_or_insert(&set->table,
														  set->table.nchunks - 1,
														  set->hashes, set->nkeys,
														  keys, &pending,
														  set->offsets, &call,
														  &state->status));
		for (int word = 0; word < nwords; word++)
			set->inserted_bits[word] |= set->call_bits[word];
		if (tess_row_mask_count(&pending) == 0)
			break;
		check(state, state->kernels->table_stats(&set->table, &stats,
												 &state->status));
		if (stats.records * 2 >= stats.buckets)
			distinct_regrow(state, set, stats.records);
		else
			distinct_add_chunk(state, set);
	}
	return inserted;
}

/* The bytes of every distinct set. */
static Size
distinct_bytes(TessAggState *state)
{
	Size		bytes = 0;

	for (int index = 0; index < state->nvalues; index++)
	{
		AggValue   *value = &state->values[index];

		if (value->distinct != NULL)
			bytes += value->distinct->bytes;
		if (value->distinct_dict != NULL)
			bytes += MemoryContextMemAllocated(value->distinct_dict->context, true);
	}
	return bytes;
}

/* ------------------------------------------------------ keys through dictionaries */

/*
 * Whether equal values of the key's type are equal bytes: compared by
 * them (text under a deterministic collation, bytea), or so under any
 * collation by the type's default B-tree family (btequalimage: oid, uuid,
 * ...). Not for numeric, float, text under a nondeterministic collation
 * (text_pattern_ops says so of its bytewise order only), bpchar, whose
 * equality ignores trailing blanks, arrays, ranges or a family that does
 * not say.
 */
static bool
key_dict_images_equal(KeyDict *dict, Oid type)
{
	TypeCacheEntry *entry;

	if (dict->bytewise)
		return true;
	entry = lookup_type_cache(type, TYPECACHE_BTREE_OPFAMILY);
	return OidIsValid(entry->btree_opf) &&
		get_opfamily_proc(entry->btree_opf, entry->btree_opintype, entry->btree_opintype,
						  BTEQUALIMAGE_PROC) == F_BTEQUALIMAGE;
}

static KeyDict *
key_dict_create(TessAggState *state, Oid eqop, Oid type, Oid collation)
{
	EState	   *estate = state->css.ss.ps.state;
	KeyDict    *dict = MemoryContextAllocZero(estate->es_query_cxt, sizeof(KeyDict));
	RegProcedure hashproc;

	if (!get_op_hash_functions(eqop, &hashproc, NULL))
		elog(ERROR, "TessAgg found no hash function of operator %u", eqop);
	fmgr_info_cxt(get_opcode(eqop), &dict->eqfn, estate->es_query_cxt);
	fmgr_info_cxt(hashproc, &dict->hashfn, estate->es_query_cxt);
	dict->collation = collation;
	dict->bytewise =
		(get_opcode(eqop) == F_BYTEAEQ && hashproc == F_HASHVARLENA) ||
		(get_opcode(eqop) == F_TEXTEQ && hashproc == F_HASHTEXT &&
		 OidIsValid(collation) && pg_newlocale_from_collation(collation)->deterministic);
	get_typlenbyval(type, &dict->typlen, &dict->typbyval);
	dict->forms = state->nkeys > 1 && !key_dict_images_equal(dict, type);
	dict->context = AllocSetContextCreate(estate->es_query_cxt, "TessAgg key values",
										  ALLOCSET_DEFAULT_SIZES);
	return dict;
}

/* Forget every value: the table they numbered is made anew. */
static void
key_dict_reset(KeyDict *dict, uint64 values)
{
	MemoryContextReset(dict->context);
	/*
	 * Room for the values expected, where the planner's estimate of the
	 * groups is known and within a quarter of hash_mem: a dictionary grown
	 * from 256 to half a million values took 5 % of an INTERSECT.
	 */
	values = Min(values, get_hash_memory_limit() / 4 /
				 (sizeof(KeyEntry) * 2 / KEYDICT_FILLFACTOR + sizeof(Datum)));
	values = Max(values, 256);
	dict->table = keydict_create(dict->context, values, dict);
	dict->slots = values;
	dict->values = MemoryContextAlloc(dict->context, sizeof(Datum) * dict->slots);
	dict->count = 0;
	dict->block = NULL;
	dict->block_used = 0;
	dict->block_len = 0;
	dict->form_table = NULL;
}

#define KEYDICT_BLOCK_LEN (64 * 1024)
#define KEYDICT_PREFETCH_BYTES (1024 * 1024)

/*
 * A copy of a by-reference value in the dictionary's blocks, one after
 * another, each at a MAXALIGN'd place; a value past a quarter of a block,
 * or an expanded object to flatten, is a copy of its own.
 */
static Datum
keydict_copy(KeyDict *dict, Datum value)
{
	Size		size;
	char	   *copy;

	if (dict->typbyval)
		return value;
	if (dict->typlen == -1 &&
		VARATT_IS_EXTERNAL_EXPANDED(DatumGetPointer(value)))
		return datumCopy(value, false, -1);
	size = datumGetSize(value, false, dict->typlen);
	if (size > KEYDICT_BLOCK_LEN / 4)
		return datumCopy(value, false, dict->typlen);
	if (dict->block == NULL || dict->block_used + size > dict->block_len)
	{
		dict->block = MemoryContextAlloc(dict->context, KEYDICT_BLOCK_LEN);
		dict->block_len = KEYDICT_BLOCK_LEN;
		dict->block_used = 0;
	}
	copy = dict->block + dict->block_used;
	memcpy(copy, DatumGetPointer(value), size);
	dict->block_used += MAXALIGN(size);
	return PointerGetDatum(copy);
}

/*
 * A dictionary of values past the caches: each row's bucket asked of
 * memory before the lookups, so that the misses of a batch's rows overlap
 * rather than each lookup waiting for its own (half a million values of
 * text: 8 to 11 % of the query). A pass of its own, the hashes computed
 * again by the lookups, and only by the values held, not the buckets: a
 * table made for the planner's estimate of 427 000 groups held 99 values,
 * which the pass cost 8 %.
 */
static pg_noinline void
keydict_prefetch_rows(KeyDict *dict, const TessDatumColumn *column, const TessRowMask *rows)
{
	int			row = -1;

	while ((row = tess_row_mask_next(rows, row)) >= 0)
		if (!column->isnull[row])
			keydict_prefetch(&dict->table->data[keydict_value_hash(dict, column->values[row]) &
												 dict->table->sizemask]);
}

/*
 * The numbers of the values of the rows of rows: a value the dictionary
 * lacks gets the next number, or, with insert false, -1, which no record
 * has; NULL stays NULL for the table's NULL key. hashes[row] receives the
 * value's hash, 0 for NULL.
 */
static void
keydict_numbers(KeyDict *dict, const TessDatumColumn *column, const TessRowMask *rows,
				bool insert, Datum *numbers, uint32 *hashes)
{
	int			row = -1;

	if (dict->table->members * sizeof(KeyEntry) > KEYDICT_PREFETCH_BYTES)
		keydict_prefetch_rows(dict, column, rows);
	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		Datum		value = column->values[row];
		uint32		hash;

		if (column->isnull[row])
		{
			numbers[row] = (Datum) 0;
			hashes[row] = 0;
			continue;
		}
		hash = keydict_value_hash(dict, value);
		hashes[row] = hash;
		if (insert)
		{
			bool		found;
			KeyEntry   *entry = keydict_insert_hash(dict->table, value, hash, &found);

			if (!found)
			{
				MemoryContext old = MemoryContextSwitchTo(dict->context);

				if (dict->count == dict->slots)
				{
					if (dict->count == PG_UINT32_MAX)
						elog(ERROR, "TessAgg numbers at most %u values of a key", PG_UINT32_MAX);
					dict->slots *= 2;
					dict->values = repalloc_huge(dict->values, sizeof(Datum) * dict->slots);
				}
				entry->value = keydict_copy(dict, value);
				entry->number = (uint32) dict->count;
				dict->values[dict->count++] = entry->value;
				MemoryContextSwitchTo(old);
			}
			numbers[row] = Int64GetDatum(entry->number);
		}
		else
		{
			KeyEntry   *entry = keydict_lookup_hash(dict->table, value, hash);

			numbers[row] = Int64GetDatum(entry != NULL ? (int64) entry->number : -1);
		}
	}
}

/* As datum_image_eq, without its calls for a value whose bytes are at hand. */
static inline bool
keydict_same_image(KeyDict *dict, Datum a, Datum b)
{
	if (dict->typbyval)
		return a == b;
	if (dict->typlen == -1 && keydict_plain(a) && keydict_plain(b))
	{
		Size		len = VARSIZE_ANY_EXHDR(DatumGetPointer(a));

		return len == VARSIZE_ANY_EXHDR(DatumGetPointer(b)) &&
			memcmp(VARDATA_ANY(DatumGetPointer(a)), VARDATA_ANY(DatumGetPointer(b)),
				   len) == 0;
	}
	return datum_image_eq(a, b, dict->typbyval, dict->typlen);
}

/*
 * The groups the rows of inserted made, by keys of several forms: a group
 * whose row came in another form than its number's first keeps a copy of
 * it by its record. Once per group, so a copy per group at most.
 */
static void
key_forms(TessAggState *state, const TessRowMask *inserted)
{
	for (int key = 0; key < state->nkeys; key++)
	{
		KeyDict    *dict = state->dicts[key];
		const TessDatumColumn *column = &state->key_columns[key];
		int			row = -1;

		if (dict == NULL || !dict->forms)
			continue;
		while ((row = tess_row_mask_next(inserted, row)) >= 0)
		{
			Datum		value = column->values[row];
			MemoryContext old;
			KeyForm    *form;
			bool		found;

			if (column->isnull[row] ||
				keydict_same_image(dict, value,
								   dict->values[DatumGetInt64(dict->batch_numbers[row])]))
				continue;
			old = MemoryContextSwitchTo(dict->context);
			if (dict->form_table == NULL)
				dict->form_table = keyform_create(dict->context, 64, NULL);
			form = keyform_insert(dict->form_table, state->offsets[row], &found);
			form->value = keydict_copy(dict, value);
			MemoryContextSwitchTo(old);
		}
	}
}

/* The bytes the groups take: the table and, with generic aggregates, their states. */
static Size
groups_memory(TessAggState *state)
{
	Size		bytes = state->table_bytes;

	if (state->has_generic)
		bytes += MemoryContextMemAllocated(state->generic_agg->curaggcontext->ecxt_per_tuple_memory,
										   true);
	for (int key = 0; key < state->nkeys; key++)
		if (state->dicts[key] != NULL)
			bytes += MemoryContextMemAllocated(state->dicts[key]->context, true);
	return bytes;
}

/* The child's columns the node reads, in their order, before it computes anything. */
static void
read_in_order(TessAggState *state, TessBatch *batch)
{
	if (state->nread_columns < 2)
		return;
	for (int index = 0; index < state->nread_columns; index++)
	{
		TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);

		batch->ops->get_datum_column(batch, state->read_columns[index], &batch->rows,
									 TESS_COLUMN_FOR_PROJECTION, &column);
	}
}

static void
computed_column(TessAggState *state, TessBatch *batch, int computed,
				TessColumnPurpose purpose, bool decimals, TessDatumColumn *result)
{
	*result = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
	result->accept_decimals = decimals;
	batch->ops->get_datum_column(batch, state->child_layout.ncolumns + computed,
								 &batch->rows, purpose, result);
	if (result->values == NULL || result->isnull == NULL ||
		result->nrows != batch->rows.nrows)
		elog(ERROR, "Tessera projection returned an invalid column");
}

/*
 * One batch into the groups: its keys, hashed in key order with NULL as a
 * key of its own, give each row the record of its group, created where
 * none exists (in another chunk or a larger index when the table has no
 * room), and each aggregate folds the
 * rows into the records' states. The batch is the projection's wrapper,
 * which computes the keys and the arguments.
 */
static void
group_batch(TessAggState *state, TessBatch *batch)
{
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask valid;
	TessRowMask pending;
	TessRowMask inserted;
	int			nsums;

	/*
	 * The right side of INTERSECT or EXCEPT while every group of the left
	 * side is in the table: its rows only count into the groups they find,
	 * and a row of no group, which cannot change what goes out, is dropped,
	 * as the core's SetOp does; its values get no numbers either.
	 */
	bool		probe = state->setop >= 0 && state->side == 1 && !state->replaying &&
		!state->frozen && state->spill == NULL;

	reserve_rows(state, nrows);
	if (!state->replaying)
		read_in_order(state, batch);
	memset(state->valid_bits, 0, sizeof(uint64) * nwords);
	memset(state->inserted_bits, 0, sizeof(uint64) * nwords);
	valid = (TessRowMask) {nrows, state->valid_bits};
	pending = (TessRowMask) {nrows, state->pending_bits};
	inserted = (TessRowMask) {nrows, state->inserted_bits};
	for (int key = 0; key < state->nkeys; key++)
	{
		TessDatumColumn *column = &state->key_columns[key];
		bool		int8 = state->kinds[key] == TESS_TABLE_KEY_INT8;
		KeyDict    *dict = state->dicts[key];

		computed_column(state, batch, key, TESS_COLUMN_FOR_FILTER, false, column);
		/* A key through a dictionary: the table groups by its values' numbers. */
		if (dict != NULL)
		{
			if (dict->capacity < nrows)
			{
				MemoryContext query = state->css.ss.ps.state->es_query_cxt;

				dict->capacity = nrows;
				dict->batch_numbers = MemoryContextAlloc(query, sizeof(Datum) * nrows);
				dict->batch_hashes = MemoryContextAlloc(query, sizeof(uint32) * nrows);
			}
			keydict_numbers(dict, column, &batch->rows, !state->frozen && !probe,
							dict->batch_numbers, dict->batch_hashes);
			state->number_columns[key] = *column;
			state->number_columns[key].values = dict->batch_numbers;
			column = &state->number_columns[key];
		}
		if (key == 0)
			check(state, (int8 ? state->kernels->int8_hash :
						  state->kernels->int4_hash) (column, NULL, &batch->rows,
													  TESS_NULL_KEYS_GROUP,
													  state->hashes, &valid,
													  &state->status));
		else
			check(state, (int8 ? state->kernels->int8_hash_next :
						  state->kernels->int4_hash_next) (column, NULL,
														   TESS_NULL_KEYS_GROUP,
														   state->hashes, &valid,
														   &state->status));
		state->table_keys[key].kind = state->kinds[key];
		state->table_keys[key].column = column;
		state->table_keys[key].prepared = NULL;
	}
	/* A frozen table's rows spill by their values' hashes. */
	if (state->has_dicts && state->frozen)
	{
		int			row = -1;

		if (state->value_hash_rows < nrows)
		{
			state->value_hash_rows = nrows;
			state->value_hashes = MemoryContextAlloc(state->css.ss.ps.state->es_query_cxt,
													 sizeof(uint32) * nrows);
		}
		while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
		{
			uint32		hash = 0;

			for (int key = 0; key < state->nkeys; key++)
			{
				const TessDatumColumn *column = &state->key_columns[key];
				uint32		part = state->dicts[key] != NULL ?
					state->dicts[key]->batch_hashes[row] :
					column->isnull[row] ? 0 :
					(uint32) murmurhash64((uint64) column->values[row]);

				hash = hash_combine(hash, part);
			}
			state->value_hashes[row] = hash;
		}
	}
	memcpy(state->pending_bits, state->valid_bits, sizeof(uint64) * nwords);
	/*
	 * A frozen table takes no new group: the rows of the groups it has go
	 * on into them, the others, their computed values, to disk.
	 */
	if (state->frozen)
	{
		uint64	   *missing_bits;
		TessRowMask missing;

		if (state->missing_words < nwords)
		{
			state->missing_words = nwords;
			state->missing_bits = state->missing_bits == NULL ?
				MemoryContextAlloc(state->css.ss.ps.state->es_query_cxt,
								   sizeof(uint64) * nwords) :
				repalloc(state->missing_bits, sizeof(uint64) * nwords);
		}
		missing_bits = state->missing_bits;
		missing = (TessRowMask) {nrows, missing_bits};

		check(state, state->kernels->table_probe(&state->table, state->hashes, state->nkeys,
												 state->table_keys, &valid, state->offsets,
												 &pending, &state->status));
		for (int word = 0; word < nwords; word++)
		{
			missing_bits[word] = state->valid_bits[word] & ~state->pending_bits[word];
			state->valid_bits[word] = state->pending_bits[word];
		}
		for (int column = 0; column < state->ncomputed; column++)
		{
			if (column < state->nkeys)
				state->computed_columns[column] = state->key_columns[column];
			else
				computed_column(state, batch, column, TESS_COLUMN_FOR_PROJECTION, false,
								&state->computed_columns[column]);
		}
		rows_write(state, state->rows_spill, &missing);
	}
	else if (probe)
	{
		/* No chunk yet: an empty left side, no group to find. */
		if (state->table.nchunks == 0)
			memset(state->valid_bits, 0, sizeof(uint64) * nwords);
		else
		{
			check(state, state->kernels->table_probe(&state->table, state->hashes,
													 state->nkeys, state->table_keys,
													 &valid, state->offsets, &pending,
													 &state->status));
			memcpy(state->valid_bits, state->pending_bits, sizeof(uint64) * nwords);
		}
	}
	else if (state->spill != NULL)
		agg_find_partitioned(state, &pending, &inserted);
	else if (state->table.nchunks == 0)
		add_chunk(state);
	for (; !probe && state->spill == NULL && !state->frozen;)
	{
		TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
		TessRowMask call = {nrows, state->call_bits};

		/*
		 * Each call fills its mask of new groups whole: they add up, or the
		 * groups made before a chunk ran out would miss their initial
		 * states. A mask has no bits past its rows on entry, which a longer
		 * batch left.
		 */
		memset(state->call_bits, 0, sizeof(uint64) * nwords);
		check(state, state->kernels->table_find_or_insert(&state->table,
														  state->table.nchunks - 1,
														  state->hashes,
														  state->nkeys,
														  state->table_keys,
														  &pending,
														  state->offsets,
														  &call,
														  &state->status));
		for (int word = 0; word < nwords; word++)
			state->inserted_bits[word] |= state->call_bits[word];
		if (tess_row_mask_count(&pending) == 0)
			break;
		/*
		 * The rows left pending find room in a larger index, when the
		 * groups reached half the buckets, or else in another chunk.
		 */
		check(state, state->kernels->table_stats(&state->table, &stats,
												 &state->status));
		if (stats.records * 2 >= stats.buckets)
			regrow_table(state, stats.records);
		else
			add_chunk(state);
	}
	if (state->has_forms)
		key_forms(state, &inserted);
	nsums = 0;
	for (int index = 0; index < state->nvalues; index++)
	{
		AggValue   *value = &state->values[index];
		TessDatumColumn column;

		/* FILTER: the rows it keeps; the groups those rows made count still. */
		TessRowMask rows = value->filter >= 0 ?
			filtered_rows(state, batch, value->filter, &valid) : valid;

		if (value->computed >= 0)
			computed_column(state, batch, value->computed,
							TESS_COLUMN_FOR_PROJECTION, fast_decimals(value), &column);
		if (value->generic != NULL)
		{
			value->generic->columns[0] = column;
			for (int arg = 1; arg < value->generic->nargs; arg++)
				computed_column(state, batch, value->computed + arg,
								TESS_COLUMN_FOR_PROJECTION, false,
								&value->generic->columns[arg]);
			/* The sum states over every valid row go in one call, below. */
			if (value->generic->sum_state && value->filter < 0 && value->distinct == NULL)
			{
				state->sum_indexes[nsums++] = index;
				continue;
			}
			if (value->distinct != NULL)
				rows = distinct_rows(state, value, nrows, state->hashes, &rows, &column);
			generic_group_accumulate(state, index, &rows, &inserted);
			continue;
		}
		if (value->distinct != NULL)
			rows = distinct_rows(state, value, nrows, state->hashes, &rows,
								 &column);
		state->calls++;
		check(state, state->kernels->table_accumulate(&state->table,
													  state->offsets, &rows,
													  value->accumulate,
													  value->computed >= 0 ? &column : NULL,
													  NULL,
													  sizeof(uint64) * value->slot,
													  0, (uint32) index,
													  &state->status));
	}
#ifdef HAVE_INT128
	if (nsums > 0)
		sum_states_accumulate(state, nsums, state->sum_indexes, &valid);
#endif
	/*
	 * Past seven eighths of hash_mem, the rest left for a batch's chunk and
	 * index: the groups go into partitions, and the largest to disk; in
	 * partial mode they go out instead (group_drain).
	 */
	if (state->spill == NULL && (!state->partial || state->partial_spill) &&
		!state->has_distinct && !state->row_spill &&
		state->table_bytes > get_hash_memory_limit() / 8 * 7)
		agg_start_spill(state);
	/*
	 * Generic states past hash_mem: the table freezes, new groups' rows go
	 * to disk; in partial mode the groups go out instead (group_drain).
	 */
	if (state->row_spill && !state->partial && !state->has_distinct && !state->frozen &&
		state->rows_level < ROWS_MAX_LEVELS &&
		groups_memory(state) > get_hash_memory_limit() / 8 * 7)
	{
		TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);

		/*
		 * A table of fewer groups than a batch's is its own overhead past a
		 * tiny hash_mem, not groups too many: freezing it would split every
		 * partition again, level after level.
		 */
		check(state, state->kernels->table_stats(&state->table, &stats, &state->status));
		if (stats.records >= AGG_GROUP_ROWS)
		{
			state->frozen = true;
			state->rows_spill = rows_spill_create(state, state->rows_level);
		}
	}
	if (state->spill != NULL)
		agg_make_room(state);
}

/* Read every batch of the child into the table of groups. */
/* Read side `side` of INTERSECT or EXCEPT from now on. */
static void
setop_side(TessAggState *state, int side)
{
	state->side = side;
	state->child = state->sides[side];
	state->input = state->side_inputs[side];
	state->projection = state->side_projections[side];
	state->child_layout = state->side_layouts[side];
}

static void
group_drain(TessAggState *state)
{
	agg_spill_free(state);
	rows_spill_free(state);
	state->rows_level = 0;
	for (int key = 0; key < state->nkeys; key++)
		if (state->dicts[key] != NULL)
			key_dict_reset(state->dicts[key], state->groups_estimate);
	/* The groups of a previous table and their states go together. */
	if (state->generic_agg != NULL)
		ReScanExprContext(state->generic_agg->curaggcontext);
	create_table(state);
	reset_distinct(state);
	for (;;)
	{
		TessBatch  *batch;
		int			rows;

		/*
		 * Partial mode: a table near hash_mem, the states' memory counted,
		 * goes out now, as partials the Finalize Aggregate merges, and the
		 * input goes on after it.
		 */
		if (state->partial && !state->partial_spill &&
			groups_memory(state) > get_hash_memory_limit() / 8 * 7)
		{
			TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);

			check(state, state->kernels->table_stats(&state->table, &stats,
													 &state->status));
			/*
			 * More groups than half the rows read since the table started:
			 * sending it up would fold nothing. The groups go into
			 * partitions and to disk from now on, and out once the input is
			 * done, still as partials. Sum states spill no state (their
			 * records merge a word an aggregate): they go up still.
			 */
			if (!state->row_spill && stats.records * 2 > state->rows - state->emit_rows)
			{
				state->partial_spill = true;
				agg_start_spill(state);
				continue;
			}
			state->emit_rows = state->rows;
			state->early_emits++;
			state->drained = true;
			state->cursor = 0;
			return;
		}
		batch = tess_input_next(state->input);
		/* INTERSECT or EXCEPT: the right side after the left. */
		if (batch == NULL && state->setop >= 0 && state->side == 0)
		{
			setop_side(state, 1);
			continue;
		}
		if (batch == NULL)
			break;
		rows = tess_row_mask_count(&batch->rows);
		state->batches++;
		state->rows += rows;
		if (rows > 0)
		{
			TessBatch  *input = tess_projection_wrap(state->projection, batch);

			ResetExprContext(state->css.ss.ps.ps_ExprContext);
			group_batch(state, input);
			input->ops->release(input);
		}
		tess_input_finish(state->input);
	}
	state->drained = true;
	state->input_done = true;
	state->cursor = 0;
	if (state->spill != NULL)
		agg_finish_input(state);
}

/*
 * Aggregate index of a group into the scan slot: a count as it is, a sum
 * or an extreme NULL without the flag of a value, an int4 extreme as an
 * int4 Datum.
 */
static void
group_value_into(TessAggState *state, int index, int group, Datum *datum, bool *isnull)
{
	AggValue   *value = &state->values[index];
	uint64		word = state->state_words[index * AGG_GROUP_ROWS + group];
	bool		seen = ((state->flag_words[group] >> index) & 1) != 0;

	*isnull = value->kind != AGG_COUNT && !seen;
	switch (value->kind)
	{
		case AGG_COUNT:
		case AGG_SUM:
			*datum = Int64GetDatum((int64) word);
			break;
		case AGG_MIN:
		case AGG_MAX:
			*datum = value->wide ?
				Int64GetDatum((int64) word) : Int32GetDatum((int32) (int64) word);
			break;
		case AGG_GENERIC:
			{
				MemoryContext old = MemoryContextSwitchTo(state->generic_output);

#ifdef HAVE_INT128
				if (value->generic->sum_state)
				{
					const uint64 *words = record_payload(state, state->walked[group]) +
						value->slot;

					*datum = state->partial ?
						sum_state_partial(value->generic, words, isnull) :
						sum_state_value(value->generic, words, isnull);
					MemoryContextSwitchTo(old);
					break;
				}
#endif
				value->generic->state = (Datum) word;
				value->generic->state_null = !seen;
				*datum = generic_value(value->generic, isnull);
				MemoryContextSwitchTo(old);
				break;
			}
	}
}

static void
group_value(TessAggState *state, int index, int group, TupleTableSlot *scan)
{
	int			attribute = state->nkeys + index;

	group_value_into(state, index, group, &scan->tts_values[attribute],
					 &scan->tts_isnull[attribute]);
}

/*
 * A column of the groups' own batch: a key's values as the walk gathered
 * them (a number already its value, which lives in the dictionary until
 * the next partition's rows are read, after the batch is done with), or an
 * aggregate's values.
 */
static void
groups_get_column(TessBatch *batch, int column, const TessRowMask *rows,
				  TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessAggState *state = (TessAggState *) batch->private_data;

	if (column < 0 || column >= state->nkeys + state->nvalues)
		elog(ERROR, "TessAgg has no column %d", column);
	if (column < state->nkeys)
	{
		result->values = state->key_values[column];
		result->isnull = state->key_isnull[column];
	}
	else
	{
		result->values = &state->agg_values[(column - state->nkeys) * AGG_GROUP_ROWS];
		result->isnull = &state->agg_isnull[(column - state->nkeys) * AGG_GROUP_ROWS];
	}
	result->nrows = batch->rows.nrows;
}

static const TessBatchOps groups_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = groups_get_column,
};

/*
 * The next groups of the walk, up to a batch of them, as result rows:
 * the keys and the aggregates in the scan slot, HAVING over them and the
 * plan's projection, as for the one row without GROUP BY. NULL when the
 * walk is over; a batch HAVING left empty is not returned.
 */
/*
 * The rows of the next partition into a table of their own, their groups
 * from the initial states; false when no partition is left.
 */
static bool
rows_drain(TessAggState *state)
{
	TessBatch  *batch;

	rows_spill_close(state);
	if (state->reader == NULL)
		rows_reader_init(state);
	if (!rows_next_partition(state))
		return false;
	if (state->generic_agg != NULL)
		ReScanExprContext(state->generic_agg->curaggcontext);
	for (int key = 0; key < state->nkeys; key++)
		if (state->dicts[key] != NULL)
			key_dict_reset(state->dicts[key], 256);
	create_table(state);
	state->frozen = false;
	state->replaying = true;
	while ((batch = reader_next(state)) != NULL)
	{
		ResetExprContext(state->css.ss.ps.ps_ExprContext);
		group_batch(state, batch);
	}
	state->replaying = false;
	state->cursor = 0;
	return true;
}

/*
 * The walk's next groups, up to a batch of them: their keys (a number as
 * its value), flags and states gathered into the node's arrays; 0 when
 * the walk is over, partitions and partial tables included.
 */
static int
next_chunk(TessAggState *state)
{
	for (;;)
	{
		uint64		all;
		TessRowMask groups;
		int			count;

		/*
		 * The groups go out without a return to the executor while HAVING
		 * rejects them, and partitions merge on the way: a chunk at a time.
		 */
		CHECK_FOR_INTERRUPTS();

		/* A table that spilled has no index once every partition is out. */
		if (state->table.index == NULL)
			return 0;
		check(state, state->kernels->table_scan(&state->table,
												&state->cursor, state->walked,
												AGG_GROUP_ROWS, &count,
												&state->status));
		if (count == 0)
		{
			/* Partial mode: the rest of the input into a table anew. */
			if (state->spill == NULL && !state->input_done)
			{
				group_drain(state);
				continue;
			}
			/* The next partition of rows of groups a frozen table lacked. */
			if ((state->rows_spill != NULL || state->rows_pending != NIL) &&
				rows_drain(state))
				continue;
			/* The next partition of a table that spilled. */
			if (state->spill == NULL || !agg_advance(state))
				return 0;
			state->cursor = 0;
			continue;
		}
		state->groups += count;
		all = count == 64 ? UINT64_MAX : (UINT64CONST(1) << count) - 1;
		groups = (TessRowMask) {count, &all};
		for (int key = 0; key < state->nkeys; key++)
		{
			check(state, state->kernels->table_gather_key(&state->table,
														  state->walked, &groups,
														  key, state->key_values[key],
														  state->key_isnull[key],
														  &state->status));
			/* A number goes out as its value, or as the group's own form. */
			if (state->dicts[key] != NULL)
			{
				KeyDict    *dict = state->dicts[key];

				for (int group = 0; group < count; group++)
					if (!state->key_isnull[key][group])
						state->key_values[key][group] =
							dict->values[DatumGetInt64(state->key_values[key][group])];
				for (int group = 0; dict->form_table != NULL && group < count; group++)
				{
					KeyForm    *form = keyform_lookup(dict->form_table, state->walked[group]);

					if (form != NULL)
						state->key_values[key][group] = form->value;
				}
			}
		}
		check(state, state->kernels->table_gather(&state->table,
												  state->walked, &groups, 0,
												  (Datum *) state->flag_words,
												  &state->status));
		for (int index = 0; index < state->nvalues; index++)
			check(state, state->kernels->table_gather(&state->table,
													  state->walked, &groups,
													  sizeof(uint64) * state->values[index].slot,
													  (Datum *) &state->state_words[index * AGG_GROUP_ROWS],
													  &state->status));
		return count;
	}
}

/*
 * The copies of a group INTERSECT or EXCEPT puts out, from its rows and
 * its right side's: EXCEPT a group of the left side alone, INTERSECT one
 * of both, and with ALL as many as the left side's rows exceed the
 * right's, or the fewer of the two.
 */
static int64
setop_copies(TessAggState *state, int group)
{
	int64		rows = (int64) state->state_words[0 * AGG_GROUP_ROWS + group];
	int64		right = (int64) state->state_words[1 * AGG_GROUP_ROWS + group];
	int64		left = rows - right;

	switch ((SetOpCmd) state->setop)
	{
		case SETOPCMD_EXCEPT:
			return left > 0 && right == 0 ? 1 : 0;
		case SETOPCMD_EXCEPT_ALL:
			return Max(left - right, 0);
		case SETOPCMD_INTERSECT:
			return left > 0 && right > 0 ? 1 : 0;
		case SETOPCMD_INTERSECT_ALL:
			return Min(left, right);
	}
	return 0;
}

/*
 * The next rows of INTERSECT or EXCEPT, up to a batch: each group of the
 * walk its copies, a group's copies going on into the next batch when
 * they do not fit; NULL at the end.
 */
static TessBatch *
setop_groups(TessAggState *state)
{
	TupleTableSlot *scan = state->css.ss.ss_ScanTupleSlot;
	int			emitted = 0;

	tess_builder_reset(state->builder);
	for (;;)
	{
		int			group;
		TupleTableSlot *row;

		if (state->setop_group >= state->setop_count)
		{
			/*
			 * The rows so far go first: the next groups may come from a
			 * partition whose reading frees the values they point into.
			 */
			if (emitted > 0)
				return tess_builder_finish(state->builder, InvalidOid);
			state->setop_count = next_chunk(state);
			state->setop_group = 0;
			state->setop_copies = -1;
			if (state->setop_count == 0)
				return tess_builder_finish(state->builder, InvalidOid);
		}
		group = state->setop_group;
		if (state->setop_copies < 0)
			state->setop_copies = setop_copies(state, group);
		if (state->setop_copies > 0)
		{
			ExecClearTuple(scan);
			for (int key = 0; key < state->nkeys; key++)
			{
				scan->tts_values[key] = state->key_values[key][group];
				scan->tts_isnull[key] = state->key_isnull[key][group];
			}
			for (int index = 0; index < state->nvalues; index++)
				group_value(state, index, group, scan);
			ExecStoreVirtualTuple(scan);
			ResetExprContext(state->css.ss.ps.ps_ExprContext);
			state->css.ss.ps.ps_ExprContext->ecxt_scantuple = scan;
			row = state->css.ss.ps.ps_ProjInfo != NULL ?
				ExecProject(state->css.ss.ps.ps_ProjInfo) :
				ExecCopySlot(state->css.ss.ps.ps_ResultTupleSlot, scan);
			while (state->setop_copies > 0 && emitted < AGG_GROUP_ROWS)
			{
				tess_builder_append_slot(state->builder, row);
				state->setop_copies--;
				emitted++;
			}
		}
		if (state->setop_copies == 0)
		{
			state->setop_group++;
			state->setop_copies = -1;
		}
		if (emitted == AGG_GROUP_ROWS)
			return tess_builder_finish(state->builder, InvalidOid);
	}
}

static TessBatch *
next_groups(TessAggState *state)
{
	ExprContext *econtext = state->css.ss.ps.ps_ExprContext;
	TupleTableSlot *scan = state->css.ss.ss_ScanTupleSlot;

	if (state->setop >= 0)
		return setop_groups(state);
	/* The walk's arrays are the batch's columns. */
	if (state->direct)
	{
		int			count = next_chunk(state);

		if (count == 0)
			return NULL;
		for (int index = 0; index < state->nvalues; index++)
			for (int group = 0; group < count; group++)
				group_value_into(state, index, group,
								 &state->agg_values[index * AGG_GROUP_ROWS + group],
								 &state->agg_isnull[index * AGG_GROUP_ROWS + group]);
		state->groups_bits[0] = count == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << count) - 1;
		state->groups_batch.rows.nrows = count;
		state->groups_batch.rows.bits = state->groups_bits;
		return &state->groups_batch;
	}
	for (;;)
	{
		int			count = next_chunk(state);
		TessBatch  *batch;

		if (count == 0)
			return NULL;
		tess_builder_reset(state->builder);
		for (int group = 0; group < count; group++)
		{
			TupleTableSlot *row;

			ExecClearTuple(scan);
			if (state->generic_output != NULL)
				MemoryContextReset(state->generic_output);
			for (int key = 0; key < state->nkeys; key++)
			{
				scan->tts_values[key] = state->key_values[key][group];
				scan->tts_isnull[key] = state->key_isnull[key][group];
			}
			for (int index = 0; index < state->nvalues; index++)
				group_value(state, index, group, scan);
			ExecStoreVirtualTuple(scan);
			ResetExprContext(econtext);
			econtext->ecxt_scantuple = scan;
			if (state->css.ss.ps.qual != NULL &&
				!ExecQual(state->css.ss.ps.qual, econtext))
				continue;
			row = state->css.ss.ps.ps_ProjInfo != NULL ?
				ExecProject(state->css.ss.ps.ps_ProjInfo) :
				ExecCopySlot(state->css.ss.ps.ps_ResultTupleSlot, scan);
			tess_builder_append_slot(state->builder, row);
		}
		batch = tess_builder_finish(state->builder, InvalidOid);
		if (batch != NULL)
			return batch;
	}
}

/*
 * The result row, once: the aggregates in the scan slot, HAVING over
 * them, and the plan's projection when the targets are not the bare
 * aggregates, as the executor set it up for the scan tuple.
 */
/*
 * GROUP BY: after the input, a batch of groups per call to a batch-aware
 * parent, or their rows one by one to a row-wise parent.
 */
static TupleTableSlot *
group_exec(TessAggState *state)
{
	bool		rows = tess_output_request(state->output)->output_mode ==
		TESS_OUTPUT_ROWS;

	if (!state->drained)
		group_drain(state);
	if (rows && state->published != NULL)
	{
		state->next_row = tess_row_mask_next(&state->published->rows,
											 state->next_row);
		if (state->next_row >= 0)
			return tess_output_select(state->output, state->next_row);
		tess_output_finish(state->output);
	}
	tess_output_release(state->output);
	state->published = next_groups(state);
	if (state->published == NULL)
	{
		state->done = true;
		return NULL;
	}
	state->next_row = tess_row_mask_next(&state->published->rows, -1);
	return tess_output_publish(state->output, state->published);
}

static TupleTableSlot *
agg_exec(CustomScanState *css)
{
	TessAggState *state = (TessAggState *) css;
	ExprContext *econtext = css->ss.ps.ps_ExprContext;
	TupleTableSlot *row;
	TessBatch  *batch;

	if (state->nkeys > 0)
		return state->done ? NULL : group_exec(state);
	if (state->done)
	{
		/* Served to a row-wise parent, or read by a batch-aware one. */
		tess_output_finish(state->output);
		tess_output_release(state->output);
		return NULL;
	}
	drain(state);
	state->done = true;
	ResetExprContext(econtext);
	econtext->ecxt_scantuple = result_row(state);
	if (css->ss.ps.qual != NULL && !ExecQual(css->ss.ps.qual, econtext))
		return NULL;
	row = css->ss.ps.ps_ProjInfo != NULL ? ExecProject(css->ss.ps.ps_ProjInfo) :
		ExecCopySlot(css->ss.ps.ps_ResultTupleSlot, econtext->ecxt_scantuple);
	tess_builder_reset(state->builder);
	tess_builder_append_slot(state->builder, row);
	batch = tess_builder_finish(state->builder, InvalidOid);
	return tess_output_publish(state->output, batch);
}

static void
agg_end(CustomScanState *css)
{
	TessAggState *state = (TessAggState *) css;

	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	tess_output_end(state->output);
	if (state->setop >= 0)
	{
		ExecEndNode(state->sides[0]);
		ExecEndNode(state->sides[1]);
	}
	else
		ExecEndNode(state->child);
	agg_spill_free(state);
	rows_spill_free(state);
	if (state->table_context != NULL)
		MemoryContextDelete(state->table_context);
}

static void
agg_rescan(CustomScanState *css)
{
	TessAggState *state = (TessAggState *) css;

	tess_output_clear(state->output);
	/* INTERSECT or EXCEPT: both sides again, from the left. */
	if (state->setop >= 0)
	{
		for (int side = 0; side < 2; side++)
		{
			tess_projection_reset(state->side_projections[side]);
			if (css->ss.ps.chgParam != NULL)
				UpdateChangedParamSet(state->sides[side], css->ss.ps.chgParam);
			ExecReScan(state->sides[side]);
			tess_input_rescan(state->side_inputs[side]);
		}
		setop_side(state, 0);
		state->setop_count = 0;
		state->setop_group = 0;
		state->setop_copies = -1;
	}
	else
	{
		if (state->projection != NULL)
			tess_projection_reset(state->projection);
		/* The core passes changed parameters to outer and inner plans only. */
		if (css->ss.ps.chgParam != NULL)
			UpdateChangedParamSet(state->child, css->ss.ps.chgParam);
		ExecReScan(state->child);
		tess_input_rescan(state->input);
	}
	for (int index = 0; index < state->nvalues; index++)
	{
		state->values[index].total = 0;
		state->values[index].has_value = false;
		state->values[index].ngathered = 0;
	}
	state->done = false;
	state->batches = 0;
	state->rows = 0;
	state->calls = 0;
	/* GROUP BY: the table is built again from the rescanned child. */
	agg_spill_free(state);
	rows_spill_free(state);
	state->drained = false;
	state->input_done = false;
	state->published = NULL;
	state->groups = 0;
	state->emit_rows = 0;
	state->partial_spill = false;
}

/* This participant's counters. */
static void
agg_counters(TessAggState *state, uint64 *values)
{
	memset(values, 0, AGG_NCOUNTERS * sizeof(uint64));
	values[AGG_BATCHES] = state->batches;
	values[AGG_ROWS] = state->rows;
	values[AGG_CALLS] = state->calls;
	if (state->setop >= 0)
		for (int side = 0; side < 2; side++)
		{
			const TessProjectionStats *computed =
				tess_projection_stats(state->side_projections[side]);

			values[AGG_COMPUTED] += computed->chain_datums + computed->row_datums;
		}
	else if (state->projection != NULL)
	{
		const TessProjectionStats *computed = tess_projection_stats(state->projection);

		values[AGG_COMPUTED] = computed->chain_datums + computed->row_datums;
	}
	values[AGG_GROUPS] = state->groups;
	values[AGG_MEMORY] = state->peak_memory;
	values[AGG_GROWS] = state->grows;
	values[AGG_PARTITIONS] = state->partitions;
	values[AGG_EVICTIONS] = state->evictions;
	values[AGG_SPILLED] = state->spilled;
	values[AGG_DISK] = state->disk_bytes;
	values[AGG_SPLITS] = state->splits;
	values[AGG_EARLY] = state->early_emits;
	values[AGG_SPILLED_ROWS] = state->spilled_rows;
}

/* The totals of every participant in a parallel plan, else the node's own. */
static void
agg_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	TessAggState *state = (TessAggState *) css;
	const uint64 *totals;
	uint64		own[AGG_NCOUNTERS];

	if (state->nkeys > 0)
	{
		CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
		List	   *context = set_deparse_context_plan(es->deparse_cxt,
													   css->ss.ps.plan, ancestors);
		bool		useprefix = es->rtable_size > 1 || es->verbose;
		List	   *keys = NIL;

		foreach_node(TargetEntry, entry, cscan->custom_scan_tlist)
		{
			if (foreach_current_index(entry) < state->nkeys)
				keys = lappend(keys, deparse_expression((Node *) entry->expr,
														context, useprefix,
														false));
		}
		ExplainPropertyList("Group Key", keys, es);
		if (state->setop >= 0)
			ExplainPropertyText("Set Operation",
								state->setop == SETOPCMD_INTERSECT ? "Intersect" :
								state->setop == SETOPCMD_INTERSECT_ALL ? "Intersect All" :
								state->setop == SETOPCMD_EXCEPT ? "Except" : "Except All", es);
	}
	if (state->partial || state->finalize)
		ExplainPropertyText("Partial Mode", state->partial ? "Partial" : "Finalize", es);
	if (!es->analyze)
		return;
	agg_counters(state, own);
	totals = tess_shared_stats_totals_or(state->stats, own);
	ExplainPropertyInteger("Input Batches", NULL, totals[AGG_BATCHES], es);
	ExplainPropertyInteger("Input Rows", NULL, totals[AGG_ROWS], es);
	ExplainPropertyInteger("Kernel Calls", NULL, totals[AGG_CALLS], es);
	if (state->projection != NULL)
		ExplainPropertyInteger("Computed Datums", NULL, totals[AGG_COMPUTED], es);
	if (state->nkeys > 0)
	{
		ExplainPropertyInteger("Groups", NULL, totals[AGG_GROUPS], es);
		ExplainPropertyInteger("Table Grows", NULL, totals[AGG_GROWS], es);
		ExplainPropertyInteger("Memory Usage", "kB",
							   (totals[AGG_MEMORY] + 1023) / 1024, es);
		if (totals[AGG_EARLY] > 0)
			ExplainPropertyInteger("Early Emits", NULL, totals[AGG_EARLY], es);
		if (totals[AGG_PARTITIONS] > 0)
		{
			ExplainPropertyInteger("Batches", NULL, totals[AGG_PARTITIONS], es);
			ExplainPropertyInteger("Evictions", NULL, totals[AGG_EVICTIONS], es);
			ExplainPropertyInteger("Spilled Chunks", NULL, totals[AGG_SPILLED], es);
			ExplainPropertyInteger("Disk Usage", "kB", (totals[AGG_DISK] + 1023) / 1024, es);
			if (totals[AGG_SPILLED_ROWS] > 0)
				ExplainPropertyInteger("Spilled Rows", NULL, totals[AGG_SPILLED_ROWS], es);
			if (totals[AGG_SPLITS] > 0)
				ExplainPropertyInteger("Split Partitions", NULL, totals[AGG_SPLITS], es);
		}
	}
}

/*
 * A parallel plan: the node shares only its counters, in the rows of its
 * chunk; the child divides the work and the Finalize Aggregate above the
 * Gather combines the participants' values.
 */
static Size
agg_estimate_dsm(CustomScanState *css, ParallelContext *pcxt)
{
	return tess_shared_stats_estimate(AGG_NCOUNTERS, pcxt->nworkers);
}

static void
agg_initialize_dsm(CustomScanState *css, ParallelContext *pcxt,
				   void *coordinate)
{
	TessAggState *state = (TessAggState *) css;

	state->stats = tess_shared_stats_setup(state->stats,
										   css->ss.ps.state->es_query_cxt,
										   coordinate, AGG_NCOUNTERS,
										   pcxt->nworkers, pcxt->seg);
}

static void
agg_reinitialize_dsm(CustomScanState *css, ParallelContext *pcxt,
					 void *coordinate)
{
	TessAggState *state = (TessAggState *) css;

	tess_shared_stats_reset(state->stats);
}

static void
agg_initialize_worker(CustomScanState *css, shm_toc *toc, void *coordinate)
{
	TessAggState *state = (TessAggState *) css;

	state->stats = tess_shared_stats_attach(css->ss.ps.state->es_query_cxt,
											coordinate, ParallelWorkerNumber + 1);
}

static void
agg_shutdown(CustomScanState *css)
{
	TessAggState *state = (TessAggState *) css;
	uint64		values[AGG_NCOUNTERS];

	if (state->stats == NULL)
		return;
	agg_counters(state, values);
	tess_shared_stats_store(state->stats, values);
}

static const CustomExecMethods agg_exec_methods = {
	.CustomName = "TessAgg",
	.BeginCustomScan = agg_begin,
	.ExecCustomScan = agg_exec,
	.EndCustomScan = agg_end,
	.ReScanCustomScan = agg_rescan,
	.ExplainCustomScan = agg_explain,
	.EstimateDSMCustomScan = agg_estimate_dsm,
	.InitializeDSMCustomScan = agg_initialize_dsm,
	.ReInitializeDSMCustomScan = agg_reinitialize_dsm,
	.InitializeWorkerCustomScan = agg_initialize_worker,
	.ShutdownCustomScan = agg_shutdown,
};

static Node *
agg_create_state(CustomScan *cscan)
{
	TessAggState *state = (TessAggState *)
		newNode(sizeof(TessAggState), T_CustomScanState);

	state->css.methods = &agg_exec_methods;
	return (Node *) state;
}

const CustomScanMethods tess_agg_scan_methods = {
	.CustomName = "TessAgg",
	.CreateCustomScanState = agg_create_state,
};

const TessNode tess_agg_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_AGG_NODE_NAME,
};

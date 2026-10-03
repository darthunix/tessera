/*
 * What the executor files of TessAgg share: the node (agg.c), its table
 * of groups (agg_group.c), its generic aggregates (agg_generic.c) and its
 * key dictionaries (agg_keydict.c). The planner and the spill see agg.h.
 */
#ifndef TESSERA_NODES_AGG_NODE_H
#define TESSERA_NODES_AGG_NODE_H

#include "postgres.h"

#include "common/hashfn.h"

#include "agg.h"

/* The table's first capacity when the planner expects fewer groups. */
#define AGG_INITIAL_GROUPS 256

/*
 * Any other aggregate without GROUP BY: its transition function called for
 * each selected row of the arguments' columns of a batch, as the core's
 * Aggregate calls it per row but without a row handed up, then its final
 * function, or, in a partial plan, its serialization function for the
 * Finalize Aggregate above. A transition function that keeps its state in
 * the aggregate's memory asks for it (AggCheckCallContext): a stand-in
 * AggState gives the node's context of states, and its temporary one
 * (AggGetTempMemoryContext). It has no Aggref (AggGetAggref): an
 * aggregate with a C function of a loadable library, which may ask for
 * one, stays the core's (generic_supported).
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
	/* The rows the kernels' sum or extreme of a batch left to the node. */
	uint64	   *decimal_rest;
	int			decimal_capacity;
	TessStatus	decimal_status;
	/*
	 * GROUP BY, sum and avg of numeric and bigint, avg of integer and
	 * smallint, sum of smallint: the group's state is TESS_TABLE_SUM_WORDS
	 * words of its record, which the kernels fold a batch into, reading
	 * sum_input, and then the address of the numeric sum of the rows they
	 * leave to the node, 0 without any (sum_state).
	 */
	bool		sum_state;
	TessTableSumInput sum_input;
	/*
	 * A sum state of avg of integer or smallint, whose partial value is the
	 * core's int8[] of the count and the sum (agg_sum_state_partial).
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
 * The groups' records lie in chunks: the first of AGG_FIRST_CHUNK bytes,
 * so that a few groups take little, the others of the most a chunk may
 * have. Records never move; when they reach half the buckets, only the
 * index is made anew, larger.
 */
#define AGG_FIRST_CHUNK (64 * 1024)

/* The node (agg.c). */
extern TessRowMask agg_filtered_rows(TessAggState *state, TessBatch *batch,
									 int filter, const TessRowMask *rows);
extern void agg_reset_distinct(TessAggState *state);

/* The table of groups and the walk over it (agg_group.c). */
extern void agg_read_in_order(TessAggState *state, TessBatch *batch);
extern void agg_setop_side(TessAggState *state, int side);
extern void agg_group_drain(TessAggState *state);
extern TessBatch *agg_next_groups(TessAggState *state);

/* Generic aggregates and their fast path (agg_generic.c). */
extern GenericAgg *agg_generic_init(TessAggState *state, Aggref *agg);
extern void agg_generic_reset(TessAggState *state, GenericAgg *generic);
extern void agg_generic_combine(TessAggState *state, GenericAgg *generic,
								Datum value, bool isnull);
extern void agg_generic_accumulate(TessAggState *state, GenericAgg *generic,
								   const TessRowMask *rows);
extern uint64 *agg_record_payload(TessAggState *state, uint32 ref);
#ifdef HAVE_INT128
extern void agg_fast_merge(TessAggState *state, GenericAgg *generic,
						   Datum value, bool isnull);
extern void agg_sum_states_accumulate(TessAggState *state, int nsums,
									  const int *indexes,
									  const TessRowMask *rows);
extern Datum agg_sum_state_value(const GenericAgg *generic,
								 const uint64 *words, bool *isnull);
extern Datum agg_sum_state_partial(const GenericAgg *generic,
								   const uint64 *words, bool *isnull);
#endif
extern void agg_generic_group_accumulate(TessAggState *state, int index,
										 const TessRowMask *rows,
										 const TessRowMask *inserted);
extern Datum agg_generic_value(GenericAgg *generic, bool *isnull);

/* Key dictionaries and DISTINCT sets (agg_keydict.c). */
extern void agg_distinct_reset(TessAggState *state, AggValue *value);
extern TessRowMask agg_distinct_rows(TessAggState *state, AggValue *value,
									 int nrows, const uint32 *group_hashes,
									 const TessRowMask *valid,
									 const TessDatumColumn *argument);
extern Size agg_distinct_bytes(TessAggState *state);
extern KeyDict *agg_key_dict_create(TessAggState *state, Oid eqop, Oid type,
									Oid collation);
extern void agg_key_dict_reset(KeyDict *dict, uint64 values);
extern void agg_keydict_numbers(KeyDict *dict, const TessDatumColumn *column,
								const TessRowMask *rows, bool insert,
								Datum *numbers, uint32 *hashes);
extern void agg_key_forms(TessAggState *state, const TessRowMask *inserted);

#endif							/* TESSERA_NODES_AGG_NODE_H */

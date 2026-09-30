#include "postgres.h"

#include "access/htup_details.h"
#include "access/nbtree.h"
#include "access/parallel.h"
#include "catalog/pg_aggregate.h"
#include "catalog/pg_type_d.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "common/hashfn.h"
#include "common/int.h"
#include "executor/executor.h"
#include "lib/hyperloglog.h"
#include "miscadmin.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/clauses.h"
#include "optimizer/planner.h"
#include "optimizer/prep.h"
#include "optimizer/tlist.h"
#include "parser/parse_agg.h"
#include "port/pg_bitutils.h"
#include "storage/shm_toc.h"
#include "utils/array.h"
#include "utils/float.h"
#include "utils/fmgroids.h"
#include "utils/syscache.h"
#include "utils/typcache.h"
#include "utils/lsyscache.h"
#include "utils/datum.h"
#include "utils/builtins.h"
#include "utils/memutils.h"
#include "utils/numeric.h"
#include "utils/pg_locale.h"
#include "utils/regproc.h"
#include "utils/selfuncs.h"
#include "utils/ruleutils.h"

#include "tessera/decimal.h"
#include "tessera/expr.h"
#include "tessera/function.h"
#include "tessera/kernel_ops.h"
#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

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
/* Groups per output batch. */
#define AGG_GROUP_ROWS 64
/* A generic sum state's payload words: the kernels' state, then the rest's address. */
#define AGG_SUM_STATE_WORDS (TESS_TABLE_SUM_WORDS + 1)
/* The table's first capacity when the planner expects fewer groups. */
#define AGG_INITIAL_GROUPS 256
/* A group's aggregate states have one flag bit each in a payload word. */
#define AGG_MAX_GROUPED 64
/* Raise the error a kernel or table call stored, if it failed. */
#define check(state, code) tess_status_check((code), &(state)->status)

/*
 * A path's flags in its private data: the node is the query's grouping,
 * whose plan applies HAVING (a DISTINCT or a set operation above one must
 * not); the node groups for a Gather, its table emptied early; the node
 * groups above one, merging the participants' partial values; the partial
 * values of its sum states are the node's own format, which only its final
 * grouping reads (TessTableSumInput).
 */
#define AGG_PATH_HAVING 0x01
#define AGG_PATH_PARTIAL 0x02
#define AGG_PATH_FINALIZE 0x04
#define AGG_PATH_OWN_STATES 0x08

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

/* How the partials of an aggregate combine, and what an empty input gives. */
typedef enum AggKind
{
	AGG_COUNT,					/* int8 sum of the partials, 0 without any */
	AGG_SUM,					/* int8 sum of the partials, NULL without any */
	AGG_MIN,					/* the least partial, NULL without any */
	AGG_MAX,					/* the greatest partial, NULL without any */
	AGG_GENERIC					/* the core's functions, over the batch's rows */
} AggKind;

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

typedef struct AggValue
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
} AggValue;

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
typedef struct KeyEntry
{
	Datum		value;
	uint32		hash;
	uint32		number;
	char		status;
} KeyEntry;

typedef struct KeyForm
{
	uint32		ref;
	char		status;
	Datum		value;
} KeyForm;

typedef struct KeyDict
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
} KeyDict;

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

typedef struct TessAggState
{
	CustomScanState css;
	PlanState  *child;
	TessInput  *input;
	/*
	 * INTERSECT or EXCEPT (the SetOpCmd, -1 for none): the left side is read
	 * first, then the right, each through its own input, projection and
	 * layout, which child, input, projection and child_layout name while
	 * it is read (side); the groups count their rows and their right
	 * side's. The group being put out, of the walk's current ones, and the
	 * copies of it still to go (-1 before they are counted).
	 */
	int			setop;
	int			side;
	PlanState  *sides[2];
	TessInput  *side_inputs[2];
	TessProjection *side_projections[2];
	TessLayout	side_layouts[2];
	int			setop_count;
	int			setop_group;
	int64		setop_copies;
	TessOutput *output;
	TessBuilder *builder;
	/* The arguments as computed columns after the child's; NULL without any. */
	TessProjection *projection;
	TessLayout	child_layout;
	AggValue   *values;
	int			nvalues;
	/* Written by a batch function on failure only. */
	TessStatus	status;
	/* The row was returned; the next call ends the scan. */
	bool		done;
	/* Under a Gather: the values as they are, for the Finalize Aggregate. */
	bool		partial;
	/*
	 * The partial values of sum states are the node's own format, for the
	 * node's final grouping above (AGG_PATH_OWN_STATES).
	 */
	bool		own_states;
	/*
	 * Above a gather: each aggregate's argument is the participants'
	 * partial values, which merge (counts and sums add as int8).
	 */
	bool		finalize;
	/*
	 * An aggregate has DISTINCT: its pairs of group and argument live in a
	 * table of their own, which does not spill, so neither do the groups.
	 */
	bool		has_distinct;
	uint64		batches;
	uint64		rows;
	uint64		calls;
	/* The counters of every participant, in a parallel plan. */
	TessSharedStats *stats;
	/*
	 * The child's columns the keys and the arguments read, ascending: read
	 * first, in that order, so that a provider that deforms a row column
	 * after column walks it once (a key of a later column read first would
	 * make it walk the row again for each earlier argument).
	 */
	int		   *read_columns;
	int			nread_columns;
	/* Generic aggregates: their states' context, and the AggState they see. */
	AggState   *generic_agg;
	/*
	 * GROUP BY with a generic aggregate: the groups' states are words of
	 * their records, a by-reference one the address of its copy, so the
	 * groups do not spill; a group's final values live until the next.
	 */
	bool		has_generic;
	MemoryContext generic_output;
	/* The bytes from a record's start to its payload, once known. */
	Size		payload_delta;
	bool		payload_known;
	/*
	 * Such groups past hash_mem go the core's way: the table takes no new
	 * group (frozen), and the rows of the groups it lacks go, their keys'
	 * and arguments' values, to partitions on disk by bits of their hash,
	 * each read back later into a table of its own; a partition too large
	 * again splits by the next bits. The sets of partitions still to read
	 * wait in a stack, the one being written on top.
	 */
	bool		frozen;
	/* The level a spill made now writes: 0 for the input, one below a partition read. */
	int			rows_level;
	struct RowSpill *rows_spill;
	List	   *rows_pending;
	struct RowReader *reader;
	bool		replaying;
	int			ncomputed;
	int16	   *computed_lens;
	bool	   *computed_byvals;
	TessDatumColumn *computed_columns;
	uint64	   *missing_bits;
	int			missing_words;
	uint64		spilled_rows;
	/*
	 * Keys through dictionaries: one per such key, NULL for a word key; the
	 * rows' hashes of their values, which choose their partitions when they
	 * spill (the numbers are the table's alone); and whether the groups
	 * spill their rows rather than their records.
	 */
	KeyDict    *dicts[TESS_TABLE_MAX_KEYS];
	bool		has_dicts;
	bool		has_forms;
	bool		row_spill;
	uint32	   *value_hashes;
	int			value_hash_rows;
	TessDatumColumn number_columns[TESS_TABLE_MAX_KEYS];

	/*
	 * GROUP BY: the keys, computed columns before the arguments, and the
	 * table of groups: a record per group, its payload a word of flags
	 * (bit i: aggregate i has a value) and each aggregate's state from its
	 * slot, payload_size bytes in all; the rows a batch's sum states left
	 * to the node.
	 */
	int			nkeys;
	Size		payload_size;
	uint64	   *sum_rest_bits;
	int		   *sum_indexes;
	TessTableKeyKind kinds[TESS_TABLE_MAX_KEYS];
	const TessKernelOps *kernels;
	MemoryContext table_context;
	/*
	 * The table: its index and chunks, the chunks' bases and lengths with
	 * room for chunk_slots of them, and the bytes they take together.
	 */
	TessTableRef table;
	void	  **chunk_bases;
	Size	   *chunk_lens;
	int			chunk_slots;
	Size		table_bytes;
	Size		peak_memory;
	uint64		groups_estimate;
	uint64		grows;
	uint64		groups;
	/* Per batch: the hashes, the record of each row and the masks. */
	int			capacity;
	uint32	   *hashes;
	uint32	   *offsets;
	uint64	   *valid_bits;
	uint64	   *pending_bits;
	uint64	   *inserted_bits;
	uint64	   *call_bits;
	/* The rows an aggregate's FILTER keeps, for the aggregate at hand. */
	uint64	   *filter_bits;
	int			filter_words;
	TessDatumColumn key_columns[TESS_TABLE_MAX_KEYS];
	TessTableKey table_keys[TESS_TABLE_MAX_KEYS];
	/* The output: the walk over the groups, the batch a row parent reads. */
	bool		drained;
	uint64		cursor;
	uint32		walked[AGG_GROUP_ROWS];
	Datum		key_values[TESS_TABLE_MAX_KEYS][AGG_GROUP_ROWS];
	bool		key_isnull[TESS_TABLE_MAX_KEYS][AGG_GROUP_ROWS];
	uint64		flag_words[AGG_GROUP_ROWS];
	/* The states of a batch of groups, AGG_GROUP_ROWS per aggregate. */
	uint64	   *state_words;
	TessBatch  *published;
	int			next_row;
	/*
	 * The groups go out as the node's own batch, its columns the keys and
	 * the aggregates' values of the walk's arrays, when the result is the
	 * scan tuple itself: no HAVING, no projection, no generic aggregate,
	 * whose values are made one group at a time. Else as rows of the
	 * builder, each value copied twice (into the result slot, then into
	 * the builder), which took 15 % of a grouping of 450 000 texts.
	 */
	bool		direct;
	TessBatch	groups_batch;
	uint64		groups_bits[1];
	Datum	   *agg_values;
	bool	   *agg_isnull;

	/*
	 * Spilling: the level of partitions being read or given out, NULL
	 * while the groups fit; an index of the table's layout alone, for the
	 * files' fingerprint; how each aggregate's states merge; the counters.
	 */
	struct AggSpill *spill;
	void	   *layout_index;
	Size		layout_len;
	TessTableCombine *combines;
	uint64		partitions;
	uint64		evictions;
	uint64		spilled;
	uint64		disk_bytes;
	uint64		splits;
	/*
	 * Partial mode: the groups go out and the table starts anew whenever it
	 * would outgrow hash_mem, the Finalize Aggregate above merging a
	 * group's partials; the input may go on after a walk.
	 */
	bool		input_done;
	uint64		early_emits;
	/*
	 * The rows read when the table last started anew; and whether the
	 * groups go to disk instead, as a serial node's do, since a table sent
	 * up held nearly a group per row read: spread groups fold little
	 * before the table fills, and the Finalize Aggregate would get them
	 * all.
	 */
	uint64		emit_rows;
	bool		partial_spill;
} TessAggState;

static const CustomExecMethods agg_exec_methods;
static TessRowMask distinct_rows(TessAggState *state, AggValue *value, int nrows,
								 const uint32 *group_hashes,
								 const TessRowMask *valid,
								 const TessDatumColumn *argument);
static create_upper_paths_hook_type previous_create_upper_paths_hook = NULL;

static Plan *agg_plan(PlannerInfo *root, RelOptInfo *rel,
					  CustomPath *best_path, List *tlist, List *clauses,
					  List *custom_plans);

static const CustomPathMethods agg_path_methods = {
	.CustomName = "TessAgg",
	.PlanCustomPath = agg_plan,
};

/* The kind of a supported aggregate, or -1: the node knows how to combine these. */
static int
aggregate_kind(Oid aggfnoid)
{
	switch (aggfnoid)
	{
		case F_COUNT_:
		case F_COUNT_ANY:
			return AGG_COUNT;
		case F_SUM_INT4:
			return AGG_SUM;
		case F_MIN_INT4:
		case F_MIN_INT8:
			return AGG_MIN;
		case F_MAX_INT4:
		case F_MAX_INT8:
			return AGG_MAX;
		default:
			return -1;
	}
}

static Node *aggregate_argument(const Aggref *agg);

/*
 * An aggregate the node computes through the core's functions: a whole
 * one or the partial one of a parallel plan, of arguments without a
 * subplan (DISTINCT: distinct_supported).
 */
static bool
generic_supported(const Aggref *agg)
{
	if (agg->args == NIL)
		return false;
	foreach_node(TargetEntry, entry, agg->args)
	{
		if (contain_subplans((Node *) entry->expr))
			return false;
	}
	return true;
}

/*
 * DISTINCT in an aggregate of one argument: a table of the pairs of group
 * and argument seen. count over any type whose equality hashes, a value a
 * word does not hold by its number in a dictionary; sum, avg, min and max
 * over integers, whose value neither the order of the values nor which of
 * equal ones comes first changes, unlike string_agg's order, a float's
 * sum or the scale of a numeric one.
 */
static bool
distinct_supported(const Aggref *agg)
{
	SortGroupClause *clause;
	TessTableKeyKind kind;
	RegProcedure hashproc;

	if (list_length(agg->args) != 1 || list_length(agg->aggdistinct) != 1)
		return false;
	clause = linitial_node(SortGroupClause, agg->aggdistinct);
	switch (agg->aggfnoid)
	{
		case F_COUNT_ANY:
			return tess_word_key_kind(exprType(aggregate_argument(agg)), &kind) ||
				(OidIsValid(clause->eqop) && get_op_hash_functions(clause->eqop, &hashproc, NULL));
		case F_SUM_INT2:
		case F_SUM_INT4:
		case F_SUM_INT8:
		case F_AVG_INT2:
		case F_AVG_INT4:
		case F_AVG_INT8:
		case F_MIN_INT4:
		case F_MIN_INT8:
		case F_MAX_INT4:
		case F_MAX_INT8:
			return true;
		default:
			return false;
	}
}

/* Whether a batch function computes the aggregate: else the core's do. */
static bool
batch_aggregate(const Aggref *agg)
{
	const TessFunction *function = tess_runtime_api()->functions->find(agg->aggfnoid);
	Oid			type;

	if (aggregate_kind(agg->aggfnoid) < 0 || function == NULL ||
		function->kind != TESS_FUNCTION_AGGREGATE)
		return false;
	if (agg->aggfnoid == F_COUNT_ || agg->aggfnoid == F_COUNT_ANY)
		return true;
	type = exprType(aggregate_argument(agg));
	return type == INT4OID || type == INT8OID;
}

/*
 * Whether a grouping keeps the aggregate as a sum state, words of the
 * group's record that the kernels fold (generic_init): sum and avg of
 * numeric, with the kernels module, and of bigint, avg of integer and
 * smallint. The planner costs such an aggregate as the node's own.
 */
static bool
sum_state_aggregate(const Aggref *agg)
{
#ifdef HAVE_INT128
	if (list_length(agg->args) != 1)
		return false;
	switch (agg->aggfnoid)
	{
		case F_SUM_NUMERIC:
		case F_AVG_NUMERIC:
			return tess_runtime_kernels() != NULL;
		case F_SUM_INT8:
		case F_AVG_INT8:
		case F_AVG_INT4:
		case F_AVG_INT2:
			return true;
		default:
			break;
	}
#endif
	return false;
}

/*
 * Whether a plain partial aggregate the node folds itself goes up in the
 * node's own format (fast_partial): sum and avg of numeric and bigint, whose
 * state in the core is internal, which only the core's functions write.
 * The node writes the others' states as the core's own transition values.
 */
static bool
own_partial_aggregate(const Aggref *agg)
{
	return agg->aggtranstype == INTERNALOID && sum_state_aggregate(agg);
}

/* The aggregates of the target list whose partial values are the node's own format. */
static int
own_partials(List *tlist)
{
	int			count = 0;

	foreach_node(TargetEntry, entry, tlist)
		if (IsA(entry->expr, Aggref) && own_partial_aggregate((Aggref *) entry->expr))
			count++;
	return count;
}

/*
 * Whether the groups of a grouping with generic aggregates fit hash_mem,
 * as the planner estimates them, since their states, words of the records
 * or addresses of copies, keep the groups from spilling: a record, a sum
 * state's four more words, and the states a word does not hold, each by
 * its type's average width or, for an internal state, the aggregate's
 * declared space or 1 kB, as the core estimates its hashed groups.
 */
static bool
generic_fits(PlannerInfo *root, RelOptInfo *output_rel, int nkeys, List *tlist)
{
	double		groups = 0;
	double		bytes;

	foreach_ptr(Path, path, output_rel->pathlist)
		if (IsA(path, AggPath) && ((AggPath *) path)->aggstrategy == AGG_HASHED)
			groups = Max(groups, path->rows);
	if (groups <= 0)
		return false;
	bytes = 16.0 + 8.0 * nkeys + 8.0;
	foreach_node(TargetEntry, entry, tlist)
	{
		Aggref	   *agg = (Aggref *) entry->expr;
		int16		len;
		bool		byval;

		if (!IsA(agg, Aggref))
			continue;
		bytes += 8.0;
		if (batch_aggregate(agg))
			continue;
		if (sum_state_aggregate(agg))
		{
			bytes += 8.0 * (AGG_SUM_STATE_WORDS - 1);
			continue;
		}
		get_typlenbyval(agg->aggtranstype, &len, &byval);
		if (byval)
			continue;
		if (agg->aggtranstype == INTERNALOID)
		{
			HeapTuple	tuple = SearchSysCache1(AGGFNOID, ObjectIdGetDatum(agg->aggfnoid));
			int32		space = 0;

			if (HeapTupleIsValid(tuple))
			{
				space = ((Form_pg_aggregate) GETSTRUCT(tuple))->aggtransspace;
				ReleaseSysCache(tuple);
			}
			bytes += space > 0 ? space : ALLOCSET_SMALL_INITSIZE;
		}
		else
			bytes += get_typavgwidth(agg->aggtranstype, -1);
	}
	return groups * bytes <= (double) get_hash_memory_limit();
}

/* Whether an aggregate of the target list goes through the core's functions. */
static bool
has_generic(List *tlist)
{
	foreach_node(TargetEntry, entry, tlist)
	{
		if (IsA(entry->expr, Aggref) && !batch_aggregate((Aggref *) entry->expr))
			return true;
	}
	return false;
}

/*
 * The sum states a grouping of the target list keeps, or -1 when another
 * aggregate of it calls the core's transition function row by row.
 */
static int
sum_states(List *tlist)
{
	int			count = 0;

	foreach_node(TargetEntry, entry, tlist)
	{
		Aggref	   *agg = (Aggref *) entry->expr;

		if (!IsA(agg, Aggref) || batch_aggregate(agg))
			continue;
		if (!sum_state_aggregate(agg))
			return -1;
		count++;
	}
	return count;
}

/* The aggregated argument, or NULL for count(*). */
static Node *
aggregate_argument(const Aggref *agg)
{
	return agg->args == NIL ? NULL :
		(Node *) ((TargetEntry *) linitial(agg->args))->expr;
}

/*
 * Whether the node computes this aggregate: a plain call, whole or the
 * partial one of a parallel plan, of an aggregate the node combines and
 * the registry implements over batches, with no argument for count(*),
 * one expression of any type for count (the count reads NULL flags
 * alone) or one int4 or int8 expression for the others, which the
 * projection provider computes by a chain or row by row, and a FILTER
 * condition, which it computes too; a subplan in them would need fixing
 * against the scan tuple, which the arguments do not go through.
 */
static bool
aggregate_supported(const Aggref *agg)
{
	Node	   *argument;

	if (agg->agglevelsup != 0 || agg->aggkind != AGGKIND_NORMAL ||
		(agg->aggsplit != AGGSPLIT_SIMPLE &&
		 agg->aggsplit != AGGSPLIT_INITIAL_SERIAL) || agg->aggorder != NIL ||
		contain_subplans((Node *) agg->aggfilter) ||
		agg->aggdirectargs != NIL || agg->aggvariadic ||
		(agg->aggdistinct != NIL && !distinct_supported(agg)))
		return false;
	if (!batch_aggregate(agg))
		return generic_supported(agg);
	if (agg->aggfnoid == F_COUNT_)
		return agg->aggstar && agg->args == NIL;
	argument = aggregate_argument(agg);
	if (list_length(agg->args) != 1 || contain_subplans(argument))
		return false;
	return true;
}

/*
 * Whether an expression reads something the child's target lacks: a
 * subexpression the target holds whole, as the planner puts a grouping
 * expression there, or a column of it are available; a placeholder
 * would stay one in the private data, so none is accepted.
 */
static bool
unavailable(Node *node, List *exprs)
{
	if (node == NULL)
		return false;
	if (list_member(exprs, node))
		return false;
	if (IsA(node, Var) || IsA(node, PlaceHolderVar))
		return true;
	return expression_tree_walker(node, unavailable, exprs);
}

/*
 * Above a gather: whether the child's target gives the keys and each
 * aggregate's partial value, which the node merges.
 */
static bool
partials_available(const List *tlist, const Path *child)
{
	foreach_ptr(TargetEntry, entry, tlist)
	{
		Node	   *expr = (Node *) entry->expr;

		if (IsA(expr, Aggref))
		{
			Aggref	   *partial = copyObject((Aggref *) expr);

			mark_partial_aggref(partial, AGGSPLIT_INITIAL_SERIAL);
			expr = (Node *) partial;
		}
		if (!list_member(child->pathtarget->exprs, expr))
			return false;
	}
	return true;
}

/* Whether the child's target gives what the keys and the arguments read. */
static bool
arguments_available(const List *tlist, const Path *child)
{
	foreach_ptr(TargetEntry, entry, tlist)
	{
		Node	   *argument = IsA(entry->expr, Aggref) ?
			(Node *) list_make2(((Aggref *) entry->expr)->args,
								((Aggref *) entry->expr)->aggfilter) :
			(Node *) entry->expr;

		if (unavailable(argument, child->pathtarget->exprs))
			return false;
	}
	return true;
}

/*
 * Whether an expression above the aggregation is made of what the scan
 * tuple holds: grouping expressions, aggregates and constants, as the
 * planner will rewrite it; a column outside them, such as one the
 * primary key makes functionally dependent, is not.
 */
static bool
not_from_groups(Node *node, List *keys)
{
	if (node == NULL || IsA(node, Aggref))
		return false;
	foreach_ptr(Node, key, keys)
	{
		if (equal(node, key))
			return false;
	}
	if (IsA(node, Var) || IsA(node, PlaceHolderVar))
		return true;
	return expression_tree_walker(node, not_from_groups, keys);
}

/*
 * Append the distinct aggregates of the expressions to a flat target
 * list, the scan tuple of the node after the grouping expressions; false
 * when one is not supported, or an expression reads a column neither a
 * grouping expression nor an aggregate gives. Expressions above the
 * aggregates are left to the plan's projection and qual over that tuple.
 */
static bool
collect_aggregates(Node *expressions, List *keys, List **tlist)
{
	List	   *found;

	if (keys != NIL && not_from_groups(expressions, keys))
		return false;
	found = pull_var_clause(expressions, PVC_INCLUDE_AGGREGATES |
							PVC_RECURSE_PLACEHOLDERS);
	foreach_ptr(Node, node, found)
	{
		if (IsA(node, Aggref))
		{
			if (!aggregate_supported((Aggref *) node))
				return false;
			*tlist = add_to_flat_tlist(*tlist, list_make1(node));
		}
		else if (keys == NIL)
			return false;
	}
	return true;
}

/*
 * The queries the node handles: plain aggregation of a single result row,
 * or grouping by plain grouping expressions.
 */
static bool
query_supported(PlannerInfo *root, RelOptInfo *input_rel, RelOptInfo *output_rel)
{
	Query	   *parse = root->parse;

	return (parse->hasAggs || parse->groupClause != NIL) &&
		parse->groupingSets == NIL && !parse->hasWindowFuncs &&
		output_rel->reloptkind == RELOPT_UPPER_REL && !IS_DUMMY_REL(input_rel);
}

/*
 * The expressions of grouping or distinct clauses when the node can group
 * by them: 1 to 16 values of a type the table keeps in a word
 * (tess_word_key_kind) or of any type its equality hashes, a bare column, a chain the expression compiler
 * takes such as c % 10, or any other expression, computed row by row. NIL
 * otherwise, also when the planner dropped every grouping clause, as for
 * a constant one.
 */
static List *
clause_keys(PlannerInfo *root, List *clauses)
{
	List	   *keys = NIL;

	if (clauses == NIL || list_length(clauses) > TESS_TABLE_MAX_KEYS)
		return NIL;
	foreach_node(SortGroupClause, clause, clauses)
	{
		Node	   *expr = (Node *) get_sortgroupclause_expr(clause,
															root->processed_tlist);
		TessTableKeyKind kind;

		/*
		 * A key the compiler does not take is computed row by row; one of a
		 * type a word does not hold goes through a dictionary of its
		 * values by its hash and equality functions (KeyDict).
		 */
		if ((!tess_word_key_kind(exprType(expr), &kind) && !clause->hashable) ||
			contain_subplans(expr) || contain_volatile_functions(expr))
			return NIL;
		keys = lappend(keys, expr);
	}
	return keys;
}

static List *
group_keys(PlannerInfo *root)
{
	return clause_keys(root, root->processed_groupClause);
}

/* The core's aggregate paths of the list with this strategy and split. */
static List *
aggregate_templates(const List *pathlist, AggStrategy strategy, AggSplit aggsplit)
{
	List	   *templates = NIL;

	foreach_ptr(Path, path, pathlist)
	{
		if (IsA(path, AggPath) &&
			((AggPath *) path)->aggstrategy == strategy &&
			((AggPath *) path)->aggsplit == aggsplit)
			templates = lappend(templates, path);
	}
	return templates;
}

/*
 * The node's own cost of grouping: its batch kernels hash the keys and
 * look the groups up for a fraction of the core's cpu_operator_cost a key
 * and a row, fold its own aggregates and sum states for such a fraction
 * too, and call a generic aggregate's transition function as the core
 * does. A sum state is five words of the group's record, not the core's
 * estimate of its transition state (128 bytes for numeric's). Past seven
 * eighths of hash_mem the rows of the groups that do not fit go to 32
 * partitions and are read back once per level, their columns written in
 * blocks sequentially, without the core's penalty for random writes. The
 * shares, tessera.agg_key_share and tessera.agg_kernel_share (0.25 each),
 * are measured: grouping alone took 0.30 of the core's time, with generic
 * aggregates 0.41 to 0.53, spilling 0.40 to 0.63 (plan 5.13).
 */
#define AGG_SPILL_PARTS 32.0

static void
group_cost(PlannerInfo *root, const Path *child, double groups, int nkeys,
		   List *tlist, AggSplit split, Path *result)
{
	AggClauseCosts costs;
	double		rows = child->rows;
	double		width = 0;
	double		entry = 16.0 + 8.0 * nkeys + 8.0;
	double		limit = (double) get_hash_memory_limit() / 8 * 7;
	int			naggs = 0;
	int			ncolumns = 0;
	int			nsums = sum_states(tlist);
	Cost		startup;
	Cost		run;

	MemSet(&costs, 0, sizeof(costs));
	if (root->parse->hasAggs)
		get_agg_clause_costs(root, split, &costs);
	foreach_node(TargetEntry, entry_node, tlist)
	{
		List	   *exprs = IsA(entry_node->expr, Aggref) ?
			list_copy((List *) ((Aggref *) entry_node->expr)->args) :
			list_make1(makeTargetEntry(entry_node->expr, 1, NULL, false));

		if (IsA(entry_node->expr, Aggref))
			naggs++;
		foreach_node(TargetEntry, arg, exprs)
		{
			Oid			type = exprType((Node *) arg->expr);
			int16		len;
			bool		byval;

			ncolumns++;
			get_typlenbyval(type, &len, &byval);
			width += 8.0 + (byval ? 0 : get_typavgwidth(type, exprTypmod((Node *) arg->expr)));
		}
	}
	entry += 8.0 * naggs + (nsums < 0 ? costs.transitionSpace :
							8.0 * (AGG_SUM_STATE_WORDS - 1) * nsums);
	startup = child->total_cost;
	startup += cpu_operator_cost * tess_agg_key_share * nkeys * rows;
	/*
	 * A key a word does not hold: its type's hash and the dictionary's
	 * lookup a row, tessera.agg_dictionary_share (0.65) past the key's
	 * share, 0.9 of the core's cpu_operator_cost with it, as the set
	 * operations' dictionary measured (plan 5.13, step 5): SELECT DISTINCT
	 * through the dictionary took 0.45 to 0.84 of the core's hashed time
	 * over the same scan with text, varchar and char keys, the same with
	 * numeric (plan 4.21 а).
	 */
	foreach_node(TargetEntry, key, tlist)
	{
		TessTableKeyKind kind;

		if (foreach_current_index(key) >= nkeys)
			break;
		if (!tess_word_key_kind(exprType((Node *) key->expr), &kind))
			startup += cpu_operator_cost * tess_agg_dictionary_share * rows;
	}
	startup += costs.transCost.startup +
		costs.transCost.per_tuple * (nsums < 0 ? 1.0 : tess_agg_kernel_share) * rows;
	/* Groups past hash_mem: their rows to disk and back, once per level. */
	if (groups * entry > limit && ncolumns > 0)
	{
		double		share = 1.0 - limit / (groups * entry);
		double		depth = ceil(log(groups * entry / limit) / log(AGG_SPILL_PARTS));
		double		spilled = rows * share * Max(depth, 1.0);
		double		pages = spilled * width / BLCKSZ;

		startup += pages * seq_page_cost + spilled * cpu_tuple_cost;
		run = pages * seq_page_cost + spilled * cpu_tuple_cost;
	}
	else
		run = 0;
	startup += costs.finalCost.startup;
	run += costs.finalCost.per_tuple * groups + cpu_tuple_cost * groups;
	result->startup_cost = startup;
	result->total_cost = startup + run;
}

static const TessBatchOps groups_batch_ops;
static bool key_eqop(Node *key, List *clauses, int *eqop);
static void create_nonunion_paths(PlannerInfo *root, RelOptInfo *output_rel);

/*
 * The node's path in place of the core's aggregate path: the same planner
 * properties and rows, a lower cost, the batch child over the core path's
 * input, the grouping expressions and the aggregates it computes; the
 * groups spill past hash_mem as the core's do. NULL when the input cannot
 * be read in batches or lacks a column. The private data: the keys, the
 * groups expected, the set operation's command (-1), the path's flags
 * (AGG_PATH_*) and each key's equality.
 */
static CustomPath *
make_agg_path(PlannerInfo *root, const AggPath *agg, List *tlist, int nkeys, int flags)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	Path	   *child;
	Path		template;

	child = agg->subpath;
	/* A sort the core put below for its sorted grouping: hashing needs none. */
	while (IsA(child, SortPath) || IsA(child, IncrementalSortPath))
		child = ((SortPath *) child)->subpath;
	child = tess_batch_input_path(root, child);
	if (child == NULL ||
		!((flags & AGG_PATH_FINALIZE) ? partials_available(tlist, child) :
		  arguments_available(tlist, child)))
		return NULL;
	template = agg->path;
	/*
	 * Grouping costs the node's own; a plain aggregate a share of the
	 * core's, tessera.agg_cost_factor (0.9), but a final one the core's:
	 * its work is a row a participant, and the share would take a tenth
	 * off the partial stack below it.
	 */
	if (nkeys > 0)
		group_cost(root, child, agg->path.rows, nkeys, tlist, agg->aggsplit, &template);
	else if ((flags & AGG_PATH_FINALIZE) == 0)
		template.total_cost *= tess_agg_cost_factor;
	/* The groups come in no order, whatever order the core's had. */
	template.pathkeys = NIL;
	config.template_path = &template;
	config.methods = &agg_path_methods;
	config.node = &tess_agg_node;
	config.children = list_make1(child);
	config.expressions = tlist;
	config.node_data = (Node *) list_make4_int(nkeys,
											   (int) Min(agg->path.rows,
														 (double) PG_INT32_MAX),
											   -1, flags);
	/*
	 * Each key's equality, for a key a word does not hold (0 for the
	 * others): its type's default one, which one of the grouping clauses
	 * must use, since the clauses may come in another order than the keys.
	 */
	foreach_node(TargetEntry, entry, tlist)
	{
		int			eqop;

		if (foreach_current_index(entry) >= nkeys)
			break;
		if (!key_eqop((Node *) entry->expr, agg->groupClause, &eqop))
			return NULL;
		config.node_data = (Node *) lappend_int((List *) config.node_data, eqop);
	}
	return tess_path_create(&config);
}

/*
 * The partial relation (UPPERREL_PARTIAL_GROUP_AGG or
 * UPPERREL_PARTIAL_DISTINCT) of an upper one, when the core built partial
 * paths for it; PostgreSQL passes the partially grouped one to no hook.
 */
static RelOptInfo *
partial_upper_rel(PlannerInfo *root, UpperRelationKind kind, RelOptInfo *upper_rel)
{
	foreach_ptr(RelOptInfo, rel, root->upper_rels[kind])
	{
		if (bms_equal(rel->relids, upper_rel->relids))
			return rel->partial_pathlist != NIL ? rel : NULL;
	}
	return NULL;
}

/*
 * Whether a partial grouping can key its table by these: the node's
 * partial table empties early, and the dictionaries of keys a word does
 * not hold would go with it.
 */
static bool
partial_keys(List *keys)
{
	foreach_ptr(Node, key, keys)
	{
		TessTableKeyKind kind;

		if (!tess_word_key_kind(exprType(key), &kind))
			return false;
	}
	return true;
}

/*
 * Grouping without aggregates, by GROUP BY, DISTINCT or UNION, in every
 * participant under a gather: over partial, a partial hashed path of the
 * core's, the node's partial path over its batch child, which groups for
 * the gather and empties its table early, TessGather over it, and above
 * that the node's grouping of the participants' groups, where the core
 * would merge them row by row; the final grouping has target, or the
 * partial one's without it, the clauses and the path flags given.
 */
static void
add_key_stack_path(PlannerInfo *root, RelOptInfo *partial_rel, RelOptInfo *output_rel,
				   AggPath *partial, List *keys, PathTarget *target, List *clauses,
				   int flags, double groups)
{
	List	   *tlist = add_to_flat_tlist(NIL, keys);
	CustomPath *below;
	CustomPath *path;
	AggPath    *final;
	Path	   *gather;

	if (not_from_groups((Node *) partial->path.pathtarget->exprs, keys))
		return;
	below = make_agg_path(root, partial, tlist, list_length(keys), AGG_PATH_PARTIAL);
	if (below == NULL || !below->path.parallel_safe || below->path.parallel_workers <= 0)
		return;
	below->path.parallel_aware = true;
	gather = tess_gather_path(root, partial_rel, &below->path);
	if (gather == NULL)
		return;
	final = create_agg_path(root, output_rel, gather,
							target != NULL ? target : partial->path.pathtarget,
							AGG_HASHED, AGGSPLIT_SIMPLE, clauses, NIL, NULL, groups);
	path = make_agg_path(root, final, tlist, list_length(keys), flags);
	if (path != NULL)
		add_path(output_rel, &path->path);
}

/* The stack over each of the core's partial hashed paths of partial_rel. */
static void
create_key_stack_paths(PlannerInfo *root, RelOptInfo *partial_rel, RelOptInfo *output_rel,
					   AggSplit split, List *keys, PathTarget *target, List *clauses,
					   int flags, double groups)
{
	if (partial_rel == NULL || keys == NIL || groups <= 0 || !partial_keys(keys))
		return;
	foreach_ptr(AggPath, agg, aggregate_templates(partial_rel->partial_pathlist,
												  AGG_HASHED, split))
		add_key_stack_path(root, partial_rel, output_rel, agg, keys, target, clauses,
						   flags, groups);
}

/*
 * The node in place of the core's partial aggregate under a Gather.
 * PostgreSQL offers extensions no hook for the partially grouped relation
 * and builds the Gather and the Finalize Aggregate before it calls this
 * one, so the node builds the whole stack for each of the core's partial
 * aggregate paths: its own partial path over the batch child, with the
 * partial aggregates as its targets, TessGather over it and the node's
 * final aggregation of final_tlist (the serial path's keys and
 * aggregates) above, whose arguments are the aggregates' partial values
 * and which applies HAVING (plan 4.23, item 4b: over the node's partial
 * aggregate, always the node's final one). Without GROUP BY it merges one
 * row a participant, a generic aggregate's by its combine function; with
 * GROUP BY each participant keeps a table of its own groups, which it
 * merges in batches, the aggregates the node's own and sum states, whose
 * partial values are the node's own format, which the core's Finalize
 * does not read. Only without TessGather (tessera.batch_gather off) do
 * the core's Gather and Finalize Aggregate merge them, and then not sum
 * states. The path is parallel-aware for the counters the node shares;
 * the child divides the work. Without aggregates the node groups above
 * the gather alone (create_key_stack_paths).
 */
static void
create_partial_paths(PlannerInfo *root, RelOptInfo *grouped_rel,
					 GroupPathExtraData *extra, List *keys, List *final_tlist,
					 double groups)
{
	RelOptInfo *partial_rel = partial_upper_rel(root, UPPERREL_PARTIAL_GROUP_AGG, grouped_rel);
	List	   *tlist = keys != NIL ? add_to_flat_tlist(NIL, keys) : NIL;
	AggStrategy strategy = keys != NIL ? AGG_HASHED : AGG_PLAIN;
	int			nsums = 0;
	int			nown = 0;

	if (partial_rel == NULL || extra == NULL || !extra->partial_costs_set ||
		(keys != NIL && groups <= 0))
		return;
	if (!collect_aggregates((Node *) partial_rel->reltarget->exprs, keys, &tlist))
		return;
	if (list_length(tlist) == list_length(keys))
	{
		create_key_stack_paths(root, partial_rel, grouped_rel, AGGSPLIT_INITIAL_SERIAL, keys,
							   grouped_rel->reltarget, root->processed_groupClause,
							   AGG_PATH_HAVING, groups);
		return;
	}
	/*
	 * Generic states would go with a table emptied early too: with GROUP BY
	 * the aggregates are the node's own and sum states. Without it, the
	 * numeric and bigint sums go up in the node's own format too.
	 */
	if (keys != NIL)
		nsums = sum_states(tlist);
	else
		nown = own_partials(tlist);
	if (nsums < 0 || !partial_keys(keys))
		return;
	foreach_ptr(AggPath, agg, aggregate_templates(partial_rel->partial_pathlist,
												  strategy,
												  AGGSPLIT_INITIAL_SERIAL))
	{
		CustomPath *partial = make_agg_path(root, agg, tlist, list_length(keys),
											AGG_PATH_PARTIAL |
											(nsums > 0 || nown > 0 ?
											 AGG_PATH_OWN_STATES : 0));
		Path	   *gathered;
		CustomPath *path;
		GatherPath *gather;
		AggPath    *final;
		double		rows;

		if (partial == NULL || !partial->path.parallel_safe ||
			partial->path.parallel_workers <= 0)
			continue;
		partial->path.parallel_aware = true;
		/* The node merges the participants' values itself, above TessGather. */
		gathered = tess_gather_path(root, partial_rel, &partial->path);
		if (gathered != NULL)
		{
			final = create_agg_path(root, grouped_rel, gathered, grouped_rel->reltarget,
									strategy, AGGSPLIT_FINAL_DESERIAL,
									keys != NIL ? root->processed_groupClause : NIL,
									(List *) extra->havingQual,
									&extra->agg_final_costs, keys != NIL ? groups : 1.0);
			path = make_agg_path(root, final, final_tlist, list_length(keys),
								 AGG_PATH_HAVING | AGG_PATH_FINALIZE |
								 (nown > 0 ? AGG_PATH_OWN_STATES : 0));
			if (path != NULL)
			{
				add_path(grouped_rel, &path->path);
				continue;
			}
		}
		/* The core's Finalize reads no sum state of the node's format. */
		if (nsums > 0)
			continue;
		if (nown > 0)
		{
			partial = make_agg_path(root, agg, tlist, 0, AGG_PATH_PARTIAL);
			if (partial == NULL)
				continue;
			partial->path.parallel_aware = true;
		}
		rows = compute_gather_rows(&partial->path);
		gather = create_gather_path(root, partial_rel, &partial->path,
									partial->path.pathtarget, NULL, &rows);
		final = create_agg_path(root, grouped_rel, &gather->path,
								grouped_rel->reltarget, strategy,
								AGGSPLIT_FINAL_DESERIAL,
								keys != NIL ? root->processed_groupClause : NIL,
								(List *) extra->havingQual,
								&extra->agg_final_costs,
								keys != NIL ? groups : 1.0);
		add_path(grouped_rel, &final->path);
	}
}

/* Whether an aggregate of the target list has DISTINCT. */
static bool
has_distinct_aggregate(List *tlist)
{
	foreach_node(TargetEntry, entry, tlist)
		if (IsA(entry->expr, Aggref) && ((Aggref *) entry->expr)->aggdistinct != NIL)
			return true;
	return false;
}

/*
 * Whether the pairs of group and argument of the DISTINCT aggregates fit
 * hash_mem, as the planner estimates them: their tables do not spill. A
 * group key fewer than a table's most leaves room for the argument.
 */
static bool
distinct_fits(PlannerInfo *root, RelOptInfo *input_rel, List *keys, List *tlist)
{
	double		bytes = 0;

	foreach_node(TargetEntry, entry, tlist)
	{
		Aggref	   *agg;
		double		pairs;

		if (!IsA(entry->expr, Aggref) || ((Aggref *) entry->expr)->aggdistinct == NIL)
			continue;
		agg = (Aggref *) entry->expr;
		if (list_length(keys) >= TESS_TABLE_MAX_KEYS)
			return false;
		pairs = estimate_num_groups(root,
									lappend(list_copy(keys), aggregate_argument(agg)),
									input_rel->rows, NULL, NULL);
		bytes += pairs * (16.0 + 8.0 * (list_length(keys) + 1));
		/* A dictionary of the values a word does not hold: an entry and a copy each. */
		{
			Node	   *argument = aggregate_argument(agg);
			TessTableKeyKind kind;

			if (!tess_word_key_kind(exprType(argument), &kind))
				bytes += estimate_num_groups(root, list_make1(argument), input_rel->rows,
											 NULL, NULL) *
					(sizeof(KeyEntry) * 2 + sizeof(Datum) +
					 get_typavgwidth(exprType(argument), exprTypmod(argument)));
		}
	}
	return bytes <= (double) get_hash_memory_limit();
}

/*
 * SELECT DISTINCT is grouping without aggregates: the node's path next to
 * each of the core's hashed distinct paths, over the same input, its keys
 * the distinct expressions, and the parallel stack over each of its
 * partial ones. DISTINCT ON, which keeps other columns of a row of each
 * group, needs the order and stays with the core.
 */
static void
create_distinct_paths(PlannerInfo *root, RelOptInfo *input_rel,
					  RelOptInfo *output_rel)
{
	List	   *keys;
	List	   *tlist;
	double		groups = 0;

	if (root->parse->hasDistinctOn || IS_DUMMY_REL(input_rel))
		return;
	keys = clause_keys(root, root->processed_distinctClause);
	if (keys == NIL)
		return;
	tlist = add_to_flat_tlist(NIL, keys);
	/* add_path changes the list: the candidates are taken first. */
	foreach_ptr(AggPath, agg, aggregate_templates(output_rel->pathlist,
												  AGG_HASHED, AGGSPLIT_SIMPLE))
	{
		CustomPath *path;

		if (agg->groupClause == NIL ||
			not_from_groups((Node *) agg->path.pathtarget->exprs, keys))
			continue;
		path = make_agg_path(root, agg, tlist, list_length(keys), 0);
		if (path != NULL)
			add_path(output_rel, &path->path);
	}
	foreach_ptr(Path, path, output_rel->pathlist)
		groups = Max(groups, path->rows);
	create_key_stack_paths(root, partial_upper_rel(root, UPPERREL_PARTIAL_DISTINCT, output_rel),
						   output_rel, AGGSPLIT_SIMPLE, keys, NULL,
						   root->processed_distinctClause, 0, groups);
}

/* The relations of a set operation's leaves, the tree's whole in the top one. */
static Relids
setop_leaves(Node *node, Relids leaves)
{
	if (IsA(node, RangeTblRef))
		return bms_add_member(leaves, ((RangeTblRef *) node)->rtindex);
	leaves = setop_leaves(((SetOperationStmt *) node)->larg, leaves);
	return setop_leaves(((SetOperationStmt *) node)->rarg, leaves);
}

/*
 * The set operation of the tree whose leaves are relids, the uppermost of
 * those the core folds into one (UNION of UNIONs); NULL for none.
 */
static SetOperationStmt *
setop_of(Node *node, Relids relids)
{
	SetOperationStmt *op;
	SetOperationStmt *found;

	if (!IsA(node, SetOperationStmt))
		return NULL;
	op = (SetOperationStmt *) node;
	if (bms_equal(setop_leaves(node, NULL), relids))
		return op;
	found = setop_of(op->larg, relids);
	return found != NULL ? found : setop_of(op->rarg, relids);
}

/*
 * Whether the core projected the paths of a set operation within another to
 * the other's column types: it does so before the hook, so a path added
 * here would lack the projection, and one over it could not find the set
 * operation's columns in the node's plan, which shows its first branch's.
 */
static bool
setop_projected(RelOptInfo *rel)
{
	foreach_ptr(Path, path, rel->pathlist)
		if (!equal(path->pathtarget->exprs, rel->reltarget->exprs))
			return true;
	return false;
}

/*
 * The partial Append of a UNION's branches, as the core builds it for its
 * Gather (generate_union_paths), which keeps it nowhere else: the first
 * partial path of each branch of append; NULL when a branch has none or
 * may not run in parallel.
 */
static Path *
union_partial_append(PlannerInfo *root, RelOptInfo *rel, AppendPath *append)
{
	AppendPathInput input = {0};
	int			workers = 0;

	if (!rel->consider_parallel || max_parallel_workers_per_gather <= 0)
		return NULL;
	foreach_ptr(Path, subpath, append->subpaths)
	{
		RelOptInfo *branch = subpath->parent;
		Path	   *partial;

		if (!branch->consider_parallel || branch->partial_pathlist == NIL)
			return NULL;
		partial = linitial(branch->partial_pathlist);
		workers = Max(workers, partial->parallel_workers);
		input.partial_subpaths = lappend(input.partial_subpaths, partial);
	}
	if (enable_parallel_append)
	{
		workers = Max(workers, pg_leftmost_one_pos32(list_length(input.partial_subpaths)) + 1);
		workers = Min(workers, max_parallel_workers_per_gather);
	}
	if (workers <= 0)
		return NULL;
	return (Path *) create_append_path(root, rel, input, NIL, NULL, workers,
									   enable_parallel_append, -1);
}

/*
 * UNION without ALL is grouping of the branches' rows by every column: the
 * node's path next to each of the core's hashed aggregate paths over the
 * Append of the branches, the node's Append below it. Above a set
 * operation the core puts a sort and a limit, or the Append, SetOp or
 * aggregate of another, which read its columns by position, where the
 * node's plan shows its first branch's targets in place of the set
 * operation's columns (tess_plan_setop_columns); one the core projected to
 * another's column types stays the core's (setop_projected), and so do the
 * operations of a recursive union, whose worktable rescans them. Once, the
 * parallel stack over the branches' partial Append: the node's partial
 * grouping over its parallel Append in every participant, TessGather, and
 * its grouping of their groups above.
 */
static void
create_setop_paths(PlannerInfo *root, RelOptInfo *output_rel)
{
	SetOperationStmt *top;
	List	   *keys;
	List	   *tlist;
	bool		stacked = false;

	if (root->parse->setOperations == NULL || root->hasRecursion ||
		IS_DUMMY_REL(output_rel))
		return;
	top = setop_of(root->parse->setOperations, output_rel->relids);
	if (top == NULL || setop_projected(output_rel))
		return;
	if (top->op != SETOP_UNION)
	{
		create_nonunion_paths(root, output_rel);
		return;
	}
	/* add_path changes the list: the candidates are taken first. */
	foreach_ptr(AggPath, agg, aggregate_templates(output_rel->pathlist,
												  AGG_HASHED, AGGSPLIT_SIMPLE))
	{
		CustomPath *path;
		Path	   *branches = agg->subpath;

		/* The partial Append under the core's Gather. */
		if (IsA(branches, GatherPath))
			branches = ((GatherPath *) branches)->subpath;
		keys = agg->path.pathtarget->exprs;
		if (!IsA(branches, AppendPath) || keys == NIL ||
			list_length(keys) > TESS_TABLE_MAX_KEYS ||
			list_length(agg->groupClause) != list_length(keys))
			continue;
		foreach_ptr(Node, key, keys)
		{
			TessTableKeyKind kind;
			SortGroupClause *clause = list_nth_node(SortGroupClause, agg->groupClause,
													foreach_current_index(key));

			if (!tess_word_key_kind(exprType(key), &kind) && !clause->hashable)
			{
				keys = NIL;
				break;
			}
		}
		if (keys == NIL)
			continue;
		if (!stacked && partial_keys(keys))
		{
			/* Under the core's Gather, or as the core would build it. */
			Path	   *partial_append = branches != agg->subpath ? branches :
				union_partial_append(root, output_rel, (AppendPath *) branches);

			stacked = true;
			if (partial_append != NULL)
			{
				/* A participant's groups: no more than its rows. */
				AggPath    *partial = create_agg_path(root, output_rel, partial_append,
													  agg->path.pathtarget, AGG_HASHED,
													  AGGSPLIT_SIMPLE, agg->groupClause,
													  NIL, NULL,
													  Min(agg->path.rows,
														  partial_append->rows));

				add_key_stack_path(root, output_rel, output_rel, partial, keys, NULL,
								   agg->groupClause, 0, agg->path.rows);
			}
		}
		if (branches != agg->subpath)
			continue;
		tlist = add_to_flat_tlist(NIL, keys);
		path = make_agg_path(root, agg, tlist, list_length(keys), 0);
		if (path != NULL)
			add_path(output_rel, &path->path);
	}
}

/*
 * A key's equality for the private data: 0 for a word key, else its type's
 * default equality, which one of the clauses must use; false when the type
 * has none that hashes.
 */
static bool
key_eqop(Node *key, List *clauses, int *eqop)
{
	TessTableKeyKind kind;
	TypeCacheEntry *type;
	bool		used = false;

	*eqop = 0;
	if (tess_word_key_kind(exprType(key), &kind))
		return true;
	type = lookup_type_cache(exprType(key), TYPECACHE_EQ_OPR | TYPECACHE_HASH_PROC);
	foreach_node(SortGroupClause, clause, clauses)
		used |= clause->eqop == type->eq_opr && clause->hashable;
	if (!OidIsValid(type->eq_opr) || !OidIsValid(type->hash_proc) || !used)
		return false;
	*eqop = (int) type->eq_opr;
	return true;
}

/*
 * INTERSECT and EXCEPT, with ALL or not, are grouping of both sides' rows
 * by every column, the left side's first, counting each group's rows and
 * its right side's; each group then goes out as many times as the
 * operation says. The node's path next to each of the core's SetOp paths
 * of a set operation the node takes (as for UNION), its two batch children
 * the sides' paths below any sort; the groups spill past hash_mem, where
 * the core's hashed SetOp would not be chosen. The private data is the
 * grouping one with the command in its place.
 */

static void
create_nonunion_paths(PlannerInfo *root, RelOptInfo *output_rel)
{
	foreach_ptr(Path, candidate, list_copy(output_rel->pathlist))
	{
		SetOpPath  *setop;
		TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
		List	   *keys;
		List	   *data;
		Path	   *left;
		Path	   *right;
		Path		template;
		Cost		own;
		bool		dictionary = false;

		if (!IsA(candidate, SetOpPath))
			continue;
		setop = (SetOpPath *) candidate;
		keys = setop->path.pathtarget->exprs;
		if (keys == NIL || list_length(keys) > TESS_TABLE_MAX_KEYS ||
			list_length(setop->groupList) != list_length(keys))
			continue;
		data = list_make4_int(list_length(keys),
							  (int) Min(setop->numGroups, (double) PG_INT32_MAX),
							  (int) setop->cmd, 0);
		foreach_ptr(Node, key, keys)
		{
			int			eqop;

			if (!key_eqop(key, setop->groupList, &eqop))
			{
				data = NIL;
				break;
			}
			dictionary |= eqop != 0;
			data = lappend_int(data, eqop);
		}
		if (data == NIL)
			continue;
		left = setop->leftpath;
		right = setop->rightpath;
		/* Sorts the core put below for its sorted SetOp: hashing needs none. */
		while (IsA(left, SortPath) || IsA(left, IncrementalSortPath))
			left = ((SortPath *) left)->subpath;
		while (IsA(right, SortPath) || IsA(right, IncrementalSortPath))
			right = ((SortPath *) right)->subpath;
		left = tess_batch_input_path(root, left);
		right = left == NULL ? NULL : tess_batch_input_path(root, right);
		if (right == NULL)
			continue;
		/*
		 * The sides' batch paths and a share of what the core's SetOp costs
		 * over its own, tessera.setop_word_share (0.5) and
		 * tessera.setop_dictionary_share (0.9): the node's took 0.55 of the
		 * core's time with keys of words, 0.9 with a key through a dictionary
		 * (plan 5.13, step 5).
		 * All of it before the first row, as the groups are made first.
		 */
		own = setop->path.total_cost - setop->leftpath->total_cost -
			setop->rightpath->total_cost;
		template = setop->path;
		template.total_cost = left->total_cost + right->total_cost +
			Max(own, 0) * (dictionary ? tess_setop_dictionary_share : tess_setop_word_share);
		template.startup_cost = template.total_cost;
		template.pathkeys = NIL;
		config.template_path = &template;
		config.methods = &agg_path_methods;
		config.node = &tess_agg_node;
		config.children = list_make2(left, right);
		config.expressions = add_to_flat_tlist(NIL, keys);
		config.node_data = (Node *) data;
		add_path(output_rel, &tess_path_create(&config)->path);
	}
}

/*
 * The node's path in place of each of the core's plain aggregate paths
 * whose input can be read in batches, and the parallel stack in place of
 * each partial one.
 */
static void
create_upper_paths(PlannerInfo *root, UpperRelationKind stage,
				   RelOptInfo *input_rel, RelOptInfo *output_rel, void *extra)
{
	List	   *keys = NIL;
	List	   *tlist = NIL;
	List	   *templates;
	AggStrategy strategy = AGG_PLAIN;
	double		groups = 0;

	if (previous_create_upper_paths_hook != NULL)
		previous_create_upper_paths_hook(root, stage, input_rel, output_rel,
										 extra);
	if (!*tess_runtime_api()->settings->enable)
		return;
	if (stage == UPPERREL_DISTINCT)
	{
		create_distinct_paths(root, input_rel, output_rel);
		return;
	}
	if (stage == UPPERREL_SETOP)
	{
		create_setop_paths(root, output_rel);
		return;
	}
	if (stage != UPPERREL_GROUP_AGG ||
		!query_supported(root, input_rel, output_rel))
		return;
	if (root->parse->groupClause != NIL)
	{
		/* The grouping expressions lead the scan tuple, the table's keys. */
		keys = group_keys(root);
		if (keys == NIL)
			return;
		tlist = add_to_flat_tlist(NIL, keys);
		strategy = AGG_HASHED;
	}
	if (!collect_aggregates((Node *) list_make2(output_rel->reltarget->exprs,
												root->parse->havingQual),
							keys, &tlist) ||
		tlist == NIL ||
		(keys != NIL && list_length(tlist) - list_length(keys) > AGG_MAX_GROUPED))
		return;
	if (!distinct_fits(root, input_rel, keys, tlist))
		return;
	/*
	 * Groups of generic aggregates spill their rows, not their states, but
	 * not alongside a DISTINCT aggregate's table: then their states must fit.
	 */
	if (keys != NIL && has_generic(tlist) && has_distinct_aggregate(tlist) &&
		!generic_fits(root, output_rel, list_length(keys), tlist))
		return;
	/* add_path changes the list: the candidates are taken first. */
	templates = aggregate_templates(output_rel->pathlist, strategy, AGGSPLIT_SIMPLE);
	/*
	 * The core's sorted grouping may have beaten its hashed one out of the
	 * list, as its spill costs more (and with DISTINCT in an aggregate it
	 * groups only sorted): the node hashes in its place at its own cost,
	 * reading the input below the sort, unless hashing is disabled.
	 */
	if (templates == NIL && strategy == AGG_HASHED && enable_hashagg)
		templates = aggregate_templates(output_rel->pathlist, AGG_SORTED,
										AGGSPLIT_SIMPLE);
	foreach_ptr(AggPath, agg, templates)
	{
		CustomPath *path = make_agg_path(root, agg, tlist, list_length(keys),
										 AGG_PATH_HAVING);

		if (path != NULL)
			add_path(output_rel, &path->path);
	}
	/*
	 * The planner's estimate of the groups, for a Finalize HashAggregate:
	 * the rows of the core's grouped paths, the relation's own being set
	 * only after this hook.
	 */
	foreach_ptr(Path, path, output_rel->pathlist)
		groups = Max(groups, path->rows);
	create_partial_paths(root, output_rel, (GroupPathExtraData *) extra, keys, tlist,
						 groups);
}

/*
 * An argument's column as a Var of INDEX_VAR naming the child's target,
 * which the projection maps to its batch column through the child's
 * layout: the arguments cannot go through custom_exprs, which the planner
 * would fix against the scan tuple of aggregates, so they travel in the
 * private data with their targets resolved here.
 */
static Node *
resolve_argument(Node *node, TessPlanChild *child)
{
	TargetEntry *found;

	if (node == NULL)
		return NULL;
	/* A column of the child, or an expression it computes, as a grouping one. */
	found = IsA(node, Const) ? NULL :
		tlist_member((Expr *) node, child->plan->targetlist);
	if (found != NULL)
	{
		if (tess_layout_column(&child->layout, found->resno - 1) < 0)
			elog(ERROR, "TessAgg argument is missing from its child");
		return (Node *) makeVar(INDEX_VAR, found->resno, exprType(node),
								exprTypmod(node), exprCollation(node), 0);
	}
	if (IsA(node, Var))
		elog(ERROR, "TessAgg argument is missing from its child");
	return expression_tree_mutator(node, resolve_argument, child);
}

/*
 * The parameters of the arguments, which the planner must see in
 * custom_exprs to count them among the plan's: a node above that
 * rescans its child only when its parameters changed would otherwise
 * keep a stale result.
 */
static bool
collect_params(Node *node, List **params)
{
	if (node == NULL)
		return false;
	if (IsA(node, Param))
	{
		*params = lappend(*params, node);
		return false;
	}
	return expression_tree_walker(node, collect_params, params);
}

/*
 * The scan tuple is the aggregates themselves, so that the planner turns
 * the targets and HAVING into references to it; the child's columns stay
 * hidden. HAVING is the plan's qual, as it is the core aggregate's, for the
 * query's grouping only (AGG_PATH_HAVING): not for the partial aggregates of
 * a parallel plan, whose Finalize Aggregate applies it, nor for a DISTINCT
 * or a set operation above. The private data carries one argument per
 * aggregate, a NULL constant for count(*), and one FILTER condition, NULL
 * without one.
 */
/*
 * A set operation's counts as aggregates of the scan tuple: count(*) of a
 * group's rows, and sum over the side, 0 for the left and 1 for the right,
 * which each side's projection computes as a constant of its own.
 */
static Aggref *
setop_count(bool side)
{
	Aggref	   *agg = makeNode(Aggref);

	agg->aggfnoid = side ? F_SUM_INT4 : F_COUNT_;
	agg->aggtype = INT8OID;
	agg->aggtranstype = INT8OID;
	agg->aggstar = !side;
	agg->aggkind = AGGKIND_NORMAL;
	agg->aggsplit = AGGSPLIT_SIMPLE;
	agg->aggno = -1;
	agg->aggtransno = -1;
	agg->location = -1;
	if (side)
	{
		agg->aggargtypes = list_make1_oid(INT4OID);
		agg->args = list_make1(makeTargetEntry((Expr *) makeConst(INT4OID, -1, InvalidOid,
																  sizeof(int32),
																  Int32GetDatum(0),
																  false, true),
											   1, NULL, false));
	}
	return agg;
}

/*
 * An argument of an aggregate with FILTER: an expression's value where the
 * condition holds, else NULL, so that a row the filter drops is never
 * computed, as the executor evaluates the arguments of the rows it keeps
 * alone; a chain computes a CASE branch over the rows it takes only. A
 * column or a constant, which no row makes fail, stays as it is, the
 * condition not computed again.
 */
static Node *
filtered_argument(Expr *condition, Node *argument)
{
	CaseExpr   *choice;
	CaseWhen   *when;

	if (IsA(argument, Var) || IsA(argument, Const))
		return argument;
	choice = makeNode(CaseExpr);
	when = makeNode(CaseWhen);

	when->expr = (Expr *) copyObject(condition);
	when->result = (Expr *) argument;
	when->location = -1;
	choice->casetype = exprType(argument);
	choice->casecollid = exprCollation(argument);
	choice->args = list_make1(when);
	choice->defresult = (Expr *) makeNullConst(exprType(argument), exprTypmod(argument),
											   exprCollation(argument));
	choice->location = -1;
	return (Node *) choice;
}

/*
 * A FILTER condition as a value the projection computes, true where it
 * holds: over the batch as `CASE WHEN condition THEN true END`, which the
 * expression compiler takes where it takes the condition, else the
 * condition itself, which the executor evaluates a row without the CASE.
 */
static Node *
filter_value(Expr *condition)
{
	CaseExpr   *choice = makeNode(CaseExpr);
	CaseWhen   *when = makeNode(CaseWhen);

	when->expr = condition;
	when->result = (Expr *) makeBoolConst(true, false);
	when->location = -1;
	choice->casetype = BOOLOID;
	choice->casecollid = InvalidOid;
	choice->args = list_make1(when);
	choice->defresult = (Expr *) makeNullConst(BOOLOID, -1, InvalidOid);
	choice->location = -1;
	return tess_expr_supports_value((Node *) choice, 0) ? (Node *) choice : (Node *) condition;
}

static Plan *
agg_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
		 List *tlist, List *clauses, List *custom_plans)
{
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPlanChild child = TESS_STRUCT_INITIALIZER(TessPlanChild);
	List	   *arguments = NIL;
	List	   *more = NIL;
	List	   *filters = NIL;
	List	   *keys = NIL;
	List	   *params = NIL;
	List	   *path_data;
	TessPlanWriter *writer;
	int			nkeys;
	int			setop;
	int			flags;

	tess_path_get_info(best_path, &info);
	if (!tess_plan_child(best_path, custom_plans, 0, &child))
		elog(ERROR, "TessAgg expected a batch child");
	/* Over a set operation's rows: its columns are the child's targets. */
	tlist = (List *) tess_plan_setop_columns((Node *) tlist, child.plan);
	info.expressions = (List *) tess_plan_setop_columns((Node *) info.expressions, child.plan);
	path_data = (List *) info.node_data;
	nkeys = linitial_int(path_data);
	setop = lthird_int(path_data);
	flags = lfourth_int(path_data);
	/* INTERSECT or EXCEPT: the command, and two children. */
	if (setop >= 0)
	{
		TessPlanChild right = TESS_STRUCT_INITIALIZER(TessPlanChild);
		int			position = 0;

		if (!tess_plan_child(best_path, custom_plans, 1, &right))
			elog(ERROR, "TessAgg expected a second batch child");
		/*
		 * The keys are the sides' columns by position, the right side's the
		 * same types: a key found by its expression in the left side's
		 * targets could name another column of the right side.
		 */
		foreach_node(TargetEntry, entry, child.plan->targetlist)
		{
			TargetEntry *other;

			if (entry->resjunk || position == nkeys)
				continue;
			other = list_nth_node(TargetEntry, right.plan->targetlist, position);
			if (exprType((Node *) other->expr) != exprType((Node *) entry->expr))
				elog(ERROR, "TessAgg sides differ in the type of column %d", position + 1);
			keys = lappend(keys, makeVar(INDEX_VAR, entry->resno,
										 exprType((Node *) entry->expr),
										 exprTypmod((Node *) entry->expr),
										 exprCollation((Node *) entry->expr), 0));
			position++;
		}
		if (position != nkeys)
			elog(ERROR, "TessAgg side has %d columns, not %d", position, nkeys);
		info.expressions = lappend(info.expressions,
								   makeTargetEntry((Expr *) setop_count(false),
												   nkeys + 1, NULL, false));
		info.expressions = lappend(info.expressions,
								   makeTargetEntry((Expr *) setop_count(true),
												   nkeys + 2, NULL, false));
	}
	foreach_ptr(TargetEntry, entry, info.expressions)
	{
		Node	   *argument;
		Expr	   *filter;

		/* The grouping expressions, then the aggregates. */
		if (foreach_current_index(entry) < nkeys)
		{
			if (setop < 0)
				keys = lappend(keys, resolve_argument((Node *) copyObject(entry->expr),
													  &child));
			continue;
		}
		/* Above a gather: the aggregate's partial value, a column of the child. */
		if (flags & AGG_PATH_FINALIZE)
		{
			Aggref	   *partial = copyObject((Aggref *) entry->expr);

			mark_partial_aggref(partial, AGGSPLIT_INITIAL_SERIAL);
			arguments = lappend(arguments, resolve_argument((Node *) partial, &child));
			more = lappend(more, NIL);
			filters = lappend(filters, NULL);
			continue;
		}
		/* FILTER (WHERE ...): which rows the aggregate reads, NULL for every row. */
		filter = ((Aggref *) entry->expr)->aggfilter;
		filters = lappend(filters, filter == NULL ? NULL :
						  resolve_argument(filter_value(copyObject(filter)), &child));
		argument = aggregate_argument((Aggref *) entry->expr);
		arguments = lappend(arguments, argument == NULL ?
							(Node *) makeNullConst(INT4OID, -1, InvalidOid) :
							resolve_argument(filter == NULL ? copyObject(argument) :
											 filtered_argument(filter, copyObject(argument)),
											 &child));
		/* The arguments after the first, of an aggregate the core's functions compute. */
		{
			List	   *rest = NIL;

			for (int n = 1; n < list_length(((Aggref *) entry->expr)->args); n++)
			{
				Node	   *other = (Node *) copyObject(list_nth_node(TargetEntry,
																	  ((Aggref *) entry->expr)->args,
																	  n)->expr);

				rest = lappend(rest, resolve_argument(filter == NULL ? other :
													  filtered_argument(filter, other),
													  &child));
			}
			more = lappend(more, rest);
		}
	}
	collect_params((Node *) arguments, &params);
	collect_params((Node *) more, &params);
	collect_params((Node *) filters, &params);
	collect_params((Node *) keys, &params);
	writer = tess_plan_writer_create(TESS_AGG_DATA, TESS_AGG_DATA_VERSION);
	tess_plan_write_list(writer, "arguments", arguments);
	tess_plan_write_list(writer, "more", more);
	tess_plan_write_list(writer, "filters", filters);
	tess_plan_write_list(writer, "keys", keys);
	tess_plan_write_int(writer, "groups", lsecond_int(path_data));
	tess_plan_write_int_list(writer, "key_eqops",
							 list_copy_head(list_copy_tail(path_data, 4), nkeys));
	tess_plan_write_int(writer, "setop", setop);
	tess_plan_write_int(writer, "partial", (flags & AGG_PATH_PARTIAL) != 0);
	tess_plan_write_int(writer, "own_states", (flags & AGG_PATH_OWN_STATES) != 0);
	tess_plan_write_int(writer, "finalize", (flags & AGG_PATH_FINALIZE) != 0);
	config.methods = &tess_agg_scan_methods;
	config.layout_policy = TESS_LAYOUT_DENSE;
	config.qual = (flags & AGG_PATH_HAVING) ? (List *) root->parse->havingQual : NIL;
	config.expressions = params;
	config.scan_targetlist = info.expressions;
	config.scanrelid = 0;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

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
static void rows_spill_free(TessAggState *state);

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

static Size agg_spill_memory(TessAggState *state);
static Size distinct_bytes(TessAggState *state);

/*
 * The bytes of the table now, and the most so far; once it spills, the
 * index and every chunk of every level, which live in the levels' memory.
 */
static void
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
static uint64
first_capacity(uint64 capacity)
{
	uint64		most = get_hash_memory_limit() / 32;

	return Max(Min(capacity, most), AGG_INITIAL_GROUPS);
}

/* An index for capacity groups in the table's memory. */
static void *
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
static void
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

/*
 * Spilling (plan item 5.6, docs/spill.md). The groups fit until the table
 * takes more than hash_mem; then they go into partitions by the hashes'
 * low bits under the one index, and while the table takes more, the
 * largest partition goes to disk whole: its records, each a group's
 * states, are written and freed, and the index is made anew over the
 * rest. Its rows then make new records, which are merged with those on
 * disk once the input is done: partition by partition, the records in
 * memory make a table, and the chunks read back merge into it
 * (tess_table_combine). A partition too large to merge is split first by
 * the next bits of the hash into a level of its own.
 *
 * The chunk arrays keep two slots first: AGG_SOURCE, empty or a chunk read
 * back for a merge or a split, and AGG_EMPTY, an empty chunk a partition
 * without one appends to, which sends its rows back for a chunk.
 */
#define AGG_SPILL_MIN_CHUNK (8 * 1024)
#define AGG_SPILL_MIN_PARTITIONS 4
#define AGG_SPILL_MAX_PARTITIONS 1024
#define AGG_SOURCE 0
#define AGG_EMPTY 1

/* A partition: its chunks in memory, the last the one it appends to. */
typedef struct AggPart
{
	void	  **chunks;
	int			nchunks;
	int			slots;
	Size		bytes;
	/* Records in memory and on disk, and the bytes written. */
	uint64		records;
	uint64		disk_records;
	uint64		disk_bytes;
	/*
	 * The partition's groups, estimated from the hashes of every record
	 * made in it (HyperLogLog, as the core's hash aggregate keeps one per
	 * spilled partition): what a merge holds, however many times a group
	 * went to disk.
	 */
	hyperLogLogState groups;
} AggPart;

/* Registers of a partition's estimate, 2^6 bytes: an error of about 13 %. */
#define AGG_GROUPS_WIDTH 6

typedef struct AggSpill
{
	struct AggSpill *parent;
	/* The chunks, in blocks of their own size; the blocks read back. */
	MemoryContext context;
	MemoryContext block_context;
	uint32		level;
	uint32		shift;
	int			npartitions;
	Size		chunk_len;
	AggPart    *parts;
	/*
	 * A split's chunks: the source, the empty one, and each partition's
	 * current chunk at AGG_EMPTY + 1 + partition, or the empty one.
	 */
	void	  **bases;
	Size	   *lens;
	uint32	   *current;
	TessSpill  *file;
	uint32		next_number;
	/* The input is read; the partition being given out, -1 before any. */
	bool		done_input;
	int			partition;
	/*
	 * Giving out: the partitions wholly in memory first, which merge with
	 * nothing and free their memory, then those with records on disk (pass
	 * 1); whether the current partition was given out, to free it next.
	 */
	int			pass;
	bool		given;
	/* The empty chunks of the two first slots. */
	uint64		source_empty[1];
	uint64		empty[1];
} AggSpill;

static inline uint32
agg_partition(const AggSpill *spill, uint32 hash)
{
	return (hash >> spill->shift) & (uint32) (spill->npartitions - 1);
}

static inline Size
agg_chunk_used(const void *base)
{
	return (Size) *(const uint64 *) base;
}

/* The bytes of a record: header, key slots, flags and the states. */
static Size
agg_record_size(TessAggState *state)
{
	return 16 + 8 * state->nkeys + state->payload_size;
}

static void
part_push(AggSpill *spill, AggPart *part, void *base)
{
	if (part->nchunks == part->slots)
	{
		part->slots = Max(part->slots * 2, 4);
		part->chunks = part->chunks == NULL ?
			MemoryContextAlloc(spill->context, sizeof(void *) * part->slots) :
			repalloc(part->chunks, sizeof(void *) * part->slots);
	}
	part->chunks[part->nchunks++] = base;
	part->bytes += spill->chunk_len;
}

static void *
agg_new_chunk(TessAggState *state, AggSpill *spill)
{
	void	   *base = MemoryContextAlloc(spill->context, spill->chunk_len);

	check(state, state->kernels->table_chunk_init(base, spill->chunk_len,
												  &state->status));
	return base;
}

/* Free a partition's chunks in memory. */
static void
part_release(AggSpill *spill, AggPart *part)
{
	for (int chunk = 0; chunk < part->nchunks; chunk++)
		pfree(part->chunks[chunk]);
	part->nchunks = 0;
	part->bytes = 0;
	part->records = 0;
}

/* Write a chunk of the partition's records, unless it holds none. */
static void
agg_write_chunk(TessAggState *state, AggSpill *spill, int partition, void *base)
{
	Size		used = agg_chunk_used(base);

	if (used <= TESS_TABLE_CHUNK_HEADER)
		return;
	/* disk_bytes of a partition: what it takes read back; the node's, what was stored. */
	state->disk_bytes += tess_spill_write(spill->file, partition, TESS_SPILL_RECORDS,
										  spill->next_number++, base, used, NULL);
	spill->parts[partition].disk_bytes += used;
	spill->parts[partition].disk_records +=
		(used - TESS_TABLE_CHUNK_HEADER) / agg_record_size(state);
	state->spilled++;
}

/*
 * A level of partitions by the hash bits from shift, for expected bytes:
 * the power of two that makes each about half of hash_mem, as long as the
 * bits last and a chunk per partition fits in half of hash_mem.
 */
static AggSpill *
agg_spill_create(TessAggState *state, AggSpill *parent, double expected, uint32 shift)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	Size		limit = get_hash_memory_limit();
	Size		record = agg_record_size(state);
	TessSpillConfig config = TESS_STRUCT_INITIALIZER(TessSpillConfig);
	TessTableRef layout = {0};
	AggSpill   *spill = MemoryContextAllocZero(context, sizeof(AggSpill));
	int			npartitions = AGG_SPILL_MIN_PARTITIONS;
	Size		chunk_len;

	/* Each partition keeps a chunk and its file's buffer of a page. */
	while (npartitions < AGG_SPILL_MAX_PARTITIONS &&
		   (double) npartitions * (limit / 2) < expected &&
		   (Size) npartitions * 2 * (AGG_SPILL_MIN_CHUNK + BLCKSZ) <= limit / 2 &&
		   shift + pg_leftmost_one_pos32(npartitions) + 1 < 32)
		npartitions *= 2;
	chunk_len = limit / (8 * npartitions);
	chunk_len = Min(chunk_len, TESS_TABLE_MAX_CHUNK_LEN);
	chunk_len = Max(chunk_len, AGG_SPILL_MIN_CHUNK);
	chunk_len = Max(chunk_len, TESS_TABLE_CHUNK_HEADER + 4 * record);
	spill->chunk_len = TYPEALIGN_DOWN(8, chunk_len);
	spill->parent = parent;
	spill->level = parent == NULL ? 0 : parent->level + 1;
	spill->shift = shift;
	spill->npartitions = npartitions;
	spill->partition = -1;
	/* Small blocks: a chunk takes a block of its own size. */
	spill->context = AllocSetContextCreate(context, "TessAgg spill",
										   ALLOCSET_SMALL_SIZES);
	spill->block_context = AllocSetContextCreate(spill->context,
												 "TessAgg spilled block",
												 ALLOCSET_SMALL_SIZES);
	spill->parts = MemoryContextAllocZero(spill->context,
										  sizeof(AggPart) * npartitions);
	{
		MemoryContext old = MemoryContextSwitchTo(spill->context);

		for (int partition = 0; partition < npartitions; partition++)
			initHyperLogLog(&spill->parts[partition].groups, AGG_GROUPS_WIDTH);
		MemoryContextSwitchTo(old);
	}
	spill->bases = MemoryContextAlloc(spill->context,
									  sizeof(void *) * (npartitions + 2));
	spill->lens = MemoryContextAlloc(spill->context, sizeof(Size) * (npartitions + 2));
	spill->current = MemoryContextAlloc(spill->context, sizeof(uint32) * npartitions);
	spill->source_empty[0] = TESS_TABLE_CHUNK_HEADER;
	spill->empty[0] = TESS_TABLE_CHUNK_HEADER;
	spill->bases[AGG_SOURCE] = spill->source_empty;
	spill->lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
	for (int partition = 0; partition < npartitions; partition++)
	{
		spill->bases[AGG_EMPTY + 1 + partition] = spill->empty;
		spill->lens[AGG_EMPTY + 1 + partition] = TESS_TABLE_CHUNK_HEADER;
		spill->current[partition] = AGG_EMPTY + 1 + partition;
	}
	spill->bases[AGG_EMPTY] = spill->empty;
	spill->lens[AGG_EMPTY] = TESS_TABLE_CHUNK_HEADER;
	/* The files' fingerprint: the table's layout, from the index. */
	layout.index = state->layout_index;
	layout.index_len = state->layout_len;
	layout.chunks = spill->bases;
	layout.chunk_lens = spill->lens;
	check(state, state->kernels->table_fingerprint(&layout, &config.fingerprint,
												   &state->status));
	config.parent_context = spill->context;
	config.kernels = state->kernels;
	config.npartitions = npartitions;
	config.level = spill->level;
	config.max_len = (uint64) MaxAllocHugeSize;
	config.buffer_len = TESS_SPILL_BUFFER_LEN(get_hash_memory_limit());
	spill->file = tess_spill_create(&config);
	state->partitions = Max(state->partitions, (uint64) npartitions);
	return spill;
}

/* Delete a level's files and free its memory. */
static void
agg_level_free(AggSpill *spill)
{
	tess_spill_free(spill->file);
	MemoryContextDelete(spill->context);
	pfree(spill);
}

static void
agg_spill_free(TessAggState *state)
{
	while (state->spill != NULL)
	{
		AggSpill   *parent = state->spill->parent;

		agg_level_free(state->spill);
		state->spill = parent;
	}
}

/*
 * Split a chunk of records, placed at the source slot, into the level's
 * partitions by the kernel: a partition whose chunk fills keeps it and
 * gets another with keep, or writes it and starts it again otherwise.
 */
static void
agg_split(TessAggState *state, AggSpill *spill, void *base, Size len, bool keep)
{
	TessTableRef ref = {0};
	uint32		offsets[AGG_GROUP_ROWS];
	uint32		hashes[AGG_GROUP_ROWS];
	Size		from = TESS_TABLE_CHUNK_HEADER;

	spill->bases[AGG_SOURCE] = base;
	spill->lens[AGG_SOURCE] = len;
	ref.chunks = spill->bases;
	ref.chunk_lens = spill->lens;
	ref.nchunks = spill->npartitions + 2;
	for (;;)
	{
		int			count;
		int			full;

		check(state, state->kernels->table_split(&ref, state->nkeys, state->kinds,
												 state->payload_size,
												 spill->current, spill->npartitions,
												 spill->shift, AGG_SOURCE, &from,
												 AGG_GROUP_ROWS, offsets, hashes,
												 &count, &full, &state->status));
		for (int index = 0; index < count; index++)
		{
			AggPart    *part = &spill->parts[agg_partition(spill, hashes[index])];

			part->records++;
			addHyperLogLog(&part->groups, murmurhash32(hashes[index]));
		}
		if (full >= 0)
		{
			AggPart    *part = &spill->parts[full];
			int			slot = AGG_EMPTY + 1 + full;

			if (spill->bases[slot] == spill->empty)
			{
				void	   *chunk = agg_new_chunk(state, spill);

				part_push(spill, part, chunk);
				spill->bases[slot] = chunk;
				spill->lens[slot] = spill->chunk_len;
			}
			else if (keep)
			{
				void	   *chunk = agg_new_chunk(state, spill);

				part_push(spill, part, chunk);
				spill->bases[slot] = chunk;
			}
			else
			{
				agg_write_chunk(state, spill, full, spill->bases[slot]);
				part->records = 0;
				check(state, state->kernels->table_chunk_init(spill->bases[slot],
															  spill->chunk_len,
															  &state->status));
			}
		}
		else if (count == 0)
			break;
	}
	spill->bases[AGG_SOURCE] = spill->source_empty;
	spill->lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
}

/*
 * The table over the partitions' chunks in memory: the chunk arrays made
 * anew, each partition appending to its last chunk, and an index for
 * twice their records, into which every record is linked.
 */
static void
agg_table_from_parts(TessAggState *state, AggSpill *spill, uint64 capacity)
{
	Size		payload_size = state->payload_size;
	int			nchunks = 2;
	Size		size;
	void	   *old = state->table.index;

	for (int partition = 0; partition < spill->npartitions; partition++)
		nchunks += spill->parts[partition].nchunks;
	if (nchunks > state->chunk_slots)
	{
		state->chunk_slots = Max(nchunks, state->chunk_slots * 2);
		state->chunk_bases = repalloc(state->chunk_bases,
									  sizeof(void *) * state->chunk_slots);
		state->chunk_lens = repalloc(state->chunk_lens,
									 sizeof(Size) * state->chunk_slots);
	}
	state->chunk_bases[AGG_SOURCE] = spill->source_empty;
	state->chunk_lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
	state->chunk_bases[AGG_EMPTY] = spill->empty;
	state->chunk_lens[AGG_EMPTY] = TESS_TABLE_CHUNK_HEADER;
	nchunks = 2;
	state->table_bytes = 0;
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		AggPart    *part = &spill->parts[partition];

		spill->current[partition] = AGG_EMPTY;
		for (int chunk = 0; chunk < part->nchunks; chunk++)
		{
			state->chunk_bases[nchunks] = part->chunks[chunk];
			state->chunk_lens[nchunks] = spill->chunk_len;
			spill->current[partition] = nchunks++;
			state->table_bytes += spill->chunk_len;
		}
	}
	state->table.chunks = state->chunk_bases;
	state->table.chunk_lens = state->chunk_lens;
	state->table.nchunks = nchunks;
	capacity = first_capacity(capacity);
	state->table.index = new_index(state, capacity, &size);
	state->table.index_len = size;
	check(state, state->kernels->table_create(state->table.index, size, state->nkeys,
											  state->kinds, payload_size,
											  capacity, &state->status));
	for (int chunk = AGG_EMPTY + 1; chunk < nchunks; chunk++)
	{
		Size		from = TESS_TABLE_CHUNK_HEADER;

		check(state, state->kernels->table_link(&state->table, chunk, &from, NULL,
												NULL, &state->status));
	}
	if (old != NULL)
		pfree(old);
	state->table_bytes += size;
	note_memory(state);
}

static bool agg_evict(TessAggState *state, Size extra);
static uint64 agg_records(AggSpill *spill);

/*
 * The table outgrew hash_mem: the first level of partitions, for twice the
 * groups so far or the planner's if more, and the records so far split
 * into them.
 */
static void
agg_start_spill(TessAggState *state)
{
	TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
	double		bytes = state->table_bytes;
	double		expected;
	AggSpill   *spill;
	int			nold = state->table.nchunks;
	void	  **old = palloc(sizeof(void *) * Max(nold, 1));
	Size	   *old_lens = palloc(sizeof(Size) * Max(nold, 1));

	check(state, state->kernels->table_stats(&state->table, &stats, &state->status));
	expected = bytes * 2;
	if (stats.records > 0)
		expected = Max(expected,
					   bytes / stats.records * (double) state->groups_estimate);
	spill = agg_spill_create(state, NULL, expected, 0);
	state->spill = spill;
	memcpy(old, state->chunk_bases, sizeof(void *) * nold);
	memcpy(old_lens, state->chunk_lens, sizeof(Size) * nold);
	for (int chunk = 0; chunk < nold; chunk++)
	{
		agg_split(state, spill, old[chunk], old_lens[chunk], true);
		pfree(old[chunk]);
	}
	pfree(old);
	pfree(old_lens);
	/* Room for the index first: the old one goes, the new one is made last. */
	pfree(state->table.index);
	state->table.index = NULL;
	(void) agg_evict(state, sizeof(uint64) * first_capacity(agg_records(spill) * 2));
	agg_table_from_parts(state, spill, agg_records(spill) * 2);
}

/*
 * Once the table, with its files' buffers, takes more than seven eighths
 * of hash_mem, the partition with the most bytes in memory goes to disk
 * whole, and the next, until the table takes half of hash_mem; the index
 * is then made anew over the rest. Evicting down to the limit only made
 * the index anew after every partition: 5 M groups of a row each at a
 * work_mem of 4 MB made it 3598 times.
 */
static bool
agg_evict(TessAggState *state, Size extra)
{
	AggSpill   *spill = state->spill;
	/* An eighth of hash_mem is left for a batch's new chunks and index. */
	Size		limit = get_hash_memory_limit() / 8 * 7;
	Size		target = get_hash_memory_limit() / 2;
	bool		evicted = false;

	if (agg_spill_memory(state) + extra <= limit)
		return false;
	while (agg_spill_memory(state) + extra > target)
	{
		int			largest = -1;
		Size		bytes = 0;

		for (int partition = 0; partition < spill->npartitions; partition++)
			if (spill->parts[partition].bytes > bytes)
			{
				largest = partition;
				bytes = spill->parts[partition].bytes;
			}
		if (largest < 0)
			break;
		for (int chunk = 0; chunk < spill->parts[largest].nchunks; chunk++)
			agg_write_chunk(state, spill, largest, spill->parts[largest].chunks[chunk]);
		part_release(spill, &spill->parts[largest]);
		state->evictions++;
		evicted = true;
	}
	return evicted;
}

/* The records of the partitions in memory, for their index. */
static uint64
agg_records(AggSpill *spill)
{
	uint64		records = 0;

	for (int partition = 0; partition < spill->npartitions; partition++)
		records += spill->parts[partition].records;
	return records;
}

static void
agg_make_room(TessAggState *state)
{
	TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
	Size		extra = 0;

	/*
	 * An index a batch could fill grows by a new one twice its size next
	 * to it: counted now, so that the partitions go to disk before the
	 * table outgrows hash_mem in the middle of a batch.
	 */
	check(state, state->kernels->table_stats(&state->table, &stats, &state->status));
	if ((stats.records + state->capacity) * 2 >= stats.buckets)
		extra = 2 * state->table.index_len;
	if (agg_evict(state, extra))
		agg_table_from_parts(state, state->spill, agg_records(state->spill) * 2);
}

/*
 * The rows of a batch into the groups of a table that spills: new groups
 * go to their partitions' chunks, a partition without room getting
 * another; a full index grows. Returns with every row resolved.
 */
static void
agg_find_partitioned(TessAggState *state, TessRowMask *pending, TessRowMask *inserted)
{
	AggSpill   *spill = state->spill;
	int			nrows = pending->nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	uint64	   *found = palloc0(sizeof(uint64) * nwords);
	bool	   *seen = palloc(sizeof(bool) * spill->npartitions);

	for (;;)
	{
		TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
		TessRowMask created = {nrows, found};
		int			row = -1;
		bool		index_full;

		memset(found, 0, sizeof(uint64) * nwords);
		check(state, state->kernels->table_find_or_insert_partitioned(&state->table,
																	  spill->current,
																	  spill->npartitions,
																	  spill->shift,
																	  state->hashes,
																	  state->nkeys,
																	  state->table_keys,
																	  pending,
																	  state->offsets,
																	  &created,
																	  &state->status));
		while ((row = tess_row_mask_next(&created, row)) >= 0)
		{
			AggPart    *part = &spill->parts[agg_partition(spill, state->hashes[row])];

			part->records++;
			/* The partition's bits are the hash's low ones: mixed first. */
			addHyperLogLog(&part->groups, murmurhash32(state->hashes[row]));
		}
		for (int word = 0; word < nwords; word++)
			inserted->bits[word] |= found[word];
		if (tess_row_mask_count(pending) == 0)
			break;
		check(state, state->kernels->table_stats(&state->table, &stats,
												 &state->status));
		index_full = stats.records * 2 >= stats.buckets;
		if (index_full)
		{
			regrow_table(state, stats.records);
			continue;
		}
		/*
		 * A new chunk for each partition that has rows left: its chunk ran
		 * out of room, or it had none.
		 */
		memset(seen, 0, sizeof(bool) * spill->npartitions);
		row = -1;
		while ((row = tess_row_mask_next(pending, row)) >= 0)
		{
			int			partition = agg_partition(spill, state->hashes[row]);
			AggPart    *part = &spill->parts[partition];
			int			chunk = state->table.nchunks;

			if (seen[partition])
				continue;
			seen[partition] = true;
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
			if (chunk == TESS_TABLE_MAX_CHUNKS)
				ereport(ERROR,
						(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
						 errmsg("TessAgg group table cannot hold more than %d chunks",
								TESS_TABLE_MAX_CHUNKS)));
			state->chunk_bases[chunk] = agg_new_chunk(state, spill);
			state->chunk_lens[chunk] = spill->chunk_len;
			part_push(spill, part, state->chunk_bases[chunk]);
			spill->current[partition] = chunk;
			state->table.nchunks++;
			state->table_bytes += spill->chunk_len;
		}
		note_memory(state);
	}
	pfree(found);
	pfree(seen);
}

/*
 * Merge a chunk of groups' states, placed at the source slot, into the
 * partition's table by the kernel: a group the table lacks goes to the
 * chunk at *dest, or to a new one of the partition's when that is full.
 */
static void
agg_combine(TessAggState *state, AggSpill *spill, AggPart *part, void *base,
			Size len, int *dest)
{
	Size		from = TESS_TABLE_CHUNK_HEADER;

	state->chunk_bases[AGG_SOURCE] = base;
	state->chunk_lens[AGG_SOURCE] = len;
	for (;;)
	{
		int			merged;
		int			stop;

		/* A partition with nothing in memory takes a chunk for its groups. */
		if (*dest <= AGG_EMPTY)
			stop = TESS_TABLE_COMBINE_CHUNK_FULL;
		else
			check(state, state->kernels->table_combine(&state->table, AGG_SOURCE,
													   &from, *dest, state->nvalues,
													   state->combines, &merged,
													   &stop, &state->status));
		if (stop == TESS_TABLE_COMBINE_DONE)
			break;
		if (stop == TESS_TABLE_COMBINE_INDEX_FULL)
		{
			TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);

			/*
			 * A new index links every chunk's records: the source's, merged
			 * or not, are no groups of the table. Hidden, or a group whose
			 * record the source still holds would be found there and never
			 * merged into its record of the table.
			 */
			state->chunk_bases[AGG_SOURCE] = spill->source_empty;
			state->chunk_lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
			check(state, state->kernels->table_stats(&state->table, &stats,
													 &state->status));
			regrow_table(state, stats.records);
			state->chunk_bases[AGG_SOURCE] = base;
			state->chunk_lens[AGG_SOURCE] = len;
			continue;
		}
		if (state->table.nchunks == state->chunk_slots)
		{
			state->chunk_slots *= 2;
			state->chunk_bases = repalloc(state->chunk_bases,
										  sizeof(void *) * state->chunk_slots);
			state->chunk_lens = repalloc(state->chunk_lens,
										 sizeof(Size) * state->chunk_slots);
			state->table.chunks = state->chunk_bases;
			state->table.chunk_lens = state->chunk_lens;
		}
		*dest = state->table.nchunks++;
		state->chunk_bases[*dest] = agg_new_chunk(state, spill);
		state->chunk_lens[*dest] = spill->chunk_len;
		part_push(spill, part, state->chunk_bases[*dest]);
	}
	state->chunk_bases[AGG_SOURCE] = spill->source_empty;
	state->chunk_lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
}

/*
 * The groups a partition merges into: its estimate with a third more for
 * the estimate's error, at most its records.
 */
static uint64
part_groups(AggPart *part)
{
	double		groups = estimateHyperLogLog(&part->groups) * 4 / 3;

	return (uint64) Min(groups, (double) (part->records + part->disk_records));
}

/*
 * The table of one partition, with an index for its records in memory and
 * on disk: its chunks in memory linked, when they hold each group once, as
 * those the first level found by the index do, and merged by the kernel
 * otherwise, as a level below's split them; then its chunks read back
 * merged in, a group the table lacks copied to its last chunk or a new one.
 */
static void
agg_merge(TessAggState *state, AggSpill *spill, int partition)
{
	AggPart    *part = &spill->parts[partition];
	Size		payload_size = state->payload_size;
	uint64		capacity = first_capacity(part_groups(part));
	bool		unique = spill->parent == NULL;
	void	  **split = NULL;
	int			nsplit = 0;
	TessSpillReader *reader;
	TessSpillHeader header;
	int			nchunks = 2;
	Size		size;
	int			dest;

	/* A level below's chunks are merged as sources, the partition's afresh. */
	if (!unique && part->nchunks > 0)
	{
		nsplit = part->nchunks;
		split = palloc(sizeof(void *) * nsplit);
		memcpy(split, part->chunks, sizeof(void *) * nsplit);
		part->nchunks = 0;
		part->bytes = 0;
	}

	if (state->table.index != NULL)
		pfree(state->table.index);
	if (part->nchunks + 3 > state->chunk_slots)
	{
		state->chunk_slots = Max(part->nchunks + 3, state->chunk_slots * 2);
		state->chunk_bases = repalloc(state->chunk_bases,
									  sizeof(void *) * state->chunk_slots);
		state->chunk_lens = repalloc(state->chunk_lens,
									 sizeof(Size) * state->chunk_slots);
	}
	state->chunk_bases[AGG_SOURCE] = spill->source_empty;
	state->chunk_lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
	state->chunk_bases[AGG_EMPTY] = spill->empty;
	state->chunk_lens[AGG_EMPTY] = TESS_TABLE_CHUNK_HEADER;
	for (int chunk = 0; chunk < part->nchunks; chunk++)
	{
		state->chunk_bases[nchunks] = part->chunks[chunk];
		state->chunk_lens[nchunks++] = spill->chunk_len;
	}
	state->table.chunks = state->chunk_bases;
	state->table.chunk_lens = state->chunk_lens;
	state->table.nchunks = nchunks;
	state->table.index = new_index(state, capacity, &size);
	state->table.index_len = size;
	check(state, state->kernels->table_create(state->table.index, size, state->nkeys,
											  state->kinds, payload_size,
											  capacity, &state->status));
	for (int chunk = AGG_EMPTY + 1; chunk < nchunks; chunk++)
	{
		Size		from = TESS_TABLE_CHUNK_HEADER;

		check(state, state->kernels->table_link(&state->table, chunk, &from, NULL,
												NULL, &state->status));
	}
	dest = nchunks - 1;
	for (int chunk = 0; chunk < nsplit; chunk++)
	{
		CHECK_FOR_INTERRUPTS();
		agg_combine(state, spill, part, split[chunk], spill->chunk_len, &dest);
		pfree(split[chunk]);
	}
	if (split != NULL)
		pfree(split);
	reader = tess_spill_open(spill->file, 0, partition);
	while (reader != NULL && tess_spill_read_header(reader, &header))
	{
		void	   *body;

		CHECK_FOR_INTERRUPTS();
		body = MemoryContextAllocExtended(spill->block_context, Max(header.len, 8),
										  MCXT_ALLOC_HUGE);
		tess_spill_read_body(reader, body, header.len);
		agg_combine(state, spill, part, body, header.len, &dest);
		MemoryContextReset(spill->block_context);
	}
	if (reader != NULL)
		tess_spill_close(reader);
	tess_spill_drop(spill->file, partition);
	state->table_bytes = size + part->bytes;
	note_memory(state);
}

/*
 * Before a level's partitions are given out: a partition with records on
 * disk writes its chunks in memory too, since it merges from disk anyway;
 * kept, they would narrow the room every other partition merges in, and
 * a partition that does not fit splits, writing all its records again.
 * Partitions wholly in memory stay.
 */
static void
agg_flush_spilled(TessAggState *state, AggSpill *spill)
{
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		AggPart    *part = &spill->parts[partition];

		if (part->disk_records == 0 || part->nchunks == 0)
			continue;
		for (int chunk = 0; chunk < part->nchunks; chunk++)
			agg_write_chunk(state, spill, partition, part->chunks[chunk]);
		part_release(spill, part);
	}
}

/*
 * A partition too large to merge splits by the next bits of the hash
 * into a level of its own: its chunks read back, then those in memory,
 * each split into the new level's partitions, which keep a chunk each in
 * memory and write the others. The new level is given out next.
 */
static void
agg_split_level(TessAggState *state, AggSpill *spill, int partition)
{
	AggPart    *part = &spill->parts[partition];
	AggSpill   *level = agg_spill_create(state, spill,
										 (double) part->disk_bytes + part->bytes,
										 spill->shift + pg_leftmost_one_pos32(spill->npartitions));
	TessSpillReader *reader = tess_spill_open(spill->file, 0, partition);
	TessSpillHeader header;

	state->splits++;
	while (reader != NULL && tess_spill_read_header(reader, &header))
	{
		void	   *body;

		CHECK_FOR_INTERRUPTS();
		body = MemoryContextAllocExtended(level->block_context, Max(header.len, 8),
										  MCXT_ALLOC_HUGE);
		tess_spill_read_body(reader, body, header.len);
		agg_split(state, level, body, header.len, false);
		MemoryContextReset(level->block_context);
	}
	if (reader != NULL)
		tess_spill_close(reader);
	for (int chunk = 0; chunk < part->nchunks; chunk++)
	{
		CHECK_FOR_INTERRUPTS();
		agg_split(state, level, part->chunks[chunk], spill->chunk_len, false);
	}
	part_release(spill, part);
	tess_spill_drop(spill->file, partition);
	level->done_input = true;
	agg_flush_spilled(state, level);
	tess_spill_finish(level->file);
	state->spill = level;
	note_memory(state);
}

/*
 * The next partition to give out, merged into a table: after the input,
 * the partitions of the first level in turn, those wholly in memory
 * first, so that a partition read back from disk merges with the most
 * room; a level below given out whole where one split, and then the level
 * above again. False once every group is out.
 */
static bool
agg_advance(TessAggState *state)
{
	Size		limit = get_hash_memory_limit();

	for (;;)
	{
		AggSpill   *spill = state->spill;
		AggPart    *part;
		Size		others = 0;
		Size		size;

		CHECK_FOR_INTERRUPTS();

		if (spill->given)
			part_release(spill, &spill->parts[spill->partition]);
		spill->given = false;
		if (++spill->partition >= spill->npartitions)
		{
			if (spill->pass == 0)
			{
				spill->pass = 1;
				spill->partition = -1;
				continue;
			}
			if (spill->parent == NULL)
				return false;
			state->spill = spill->parent;
			agg_level_free(spill);
			continue;
		}
		part = &spill->parts[spill->partition];
		if (part->records == 0 && part->disk_records == 0)
			continue;
		if (spill->pass == 0 && part->disk_records > 0)
			continue;
		/*
		 * What the partition takes merged: a record per group and its
		 * index, and a block read back, next to the chunks every level
		 * keeps. A group written many times merges into one record, so the
		 * groups decide, not the file.
		 */
		size = part_groups(part) * (agg_record_size(state) + 2 * sizeof(uint64)) +
			spill->chunk_len;
		for (AggSpill *level = spill; level != NULL; level = level->parent)
			for (int partition = 0; partition < level->npartitions; partition++)
				if (level != spill || partition != spill->partition)
					others += level->parts[partition].bytes;
		if (part->disk_bytes > 0 && others + size > limit &&
			spill->shift + pg_leftmost_one_pos32(spill->npartitions) + 2 <= 32)
		{
			agg_split_level(state, spill, spill->partition);
			continue;
		}
		agg_merge(state, spill, spill->partition);
		spill->given = true;
		return true;
	}
}

/* The input is done: the partitions are given out one by one. */
static void
agg_finish_input(TessAggState *state)
{
	AggSpill   *spill = state->spill;

	spill->done_input = true;
	spill->partition = -1;
	agg_flush_spilled(state, spill);
	tess_spill_finish(spill->file);
	if (state->table.index != NULL)
		pfree(state->table.index);
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->table_bytes = 0;
	state->cursor = 0;
	if (!agg_advance(state))
		state->table.nchunks = 0;
}

static Size
agg_spill_memory(TessAggState *state)
{
	Size		memory = state->table.index != NULL ? state->table.index_len : 0;

	for (AggSpill *spill = state->spill; spill != NULL; spill = spill->parent)
		memory += MemoryContextMemAllocated(spill->context, true);
	return memory;
}

/* A computed column of the projection's wrapper, checked. */
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

/* ------------------------------------------------------ rows past hash_mem */

/* Hash bits a level of partitions of rows takes, and the most levels. */
#define ROWS_PART_BITS 5
#define ROWS_PARTS (1 << ROWS_PART_BITS)
#define ROWS_MAX_LEVELS (32 / ROWS_PART_BITS)
/* Rows of a block, and the first bytes of its values. */
#define ROWS_BLOCK_ROWS 256
#define ROWS_BLOCK_VALUES 8192

/* The block a partition fills: its columns and its by-reference values. */
typedef struct RowWriter
{
	void	   *chunk;
	Size		chunk_len;
	uint32		capacity;
	uint32		rows;
	char	   *values;
	Size		values_len;
	Size		values_used;
	uint64		written;
} RowWriter;

/* A level of partitions: one set of files, a partition per ROWS_PART_BITS bits. */
typedef struct RowSpill
{
	TessSpill  *file;
	int			level;
	RowWriter	writers[ROWS_PARTS];
	/* Reading: the next partition to read. */
	int			next;
} RowSpill;

/* A partition being read back: the block in hand and its next row. */
typedef struct RowReader
{
	TessSpillReader *file;
	void	   *chunk;
	Size		chunk_len;
	char	   *values;
	Size		values_len;
	uint32		rows;
	uint32		next;
	/* The batch given to group_batch: a window of the block. */
	TessBatch	batch;
	uint64		bits;
	Datum	  **column_values;
	bool	  **column_isnull;
} RowReader;

/* Words of a block: one per computed column. */
static int
rows_words(TessAggState *state)
{
	return state->ncomputed;
}

static void
rows_writer_reset(TessAggState *state, RowWriter *writer)
{
	Size		capacity;

	check(state, state->kernels->spill_columns_init(writer->chunk, writer->chunk_len,
													rows_words(state), &capacity,
													&state->status));
	writer->capacity = (uint32) capacity;
	writer->rows = 0;
	writer->values_used = 0;
}

/* A level of partitions, their files not made until written. */
static RowSpill *
rows_spill_create(TessAggState *state, int level)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	TessSpillConfig config = TESS_STRUCT_INITIALIZER(TessSpillConfig);
	RowSpill   *spill = MemoryContextAllocZero(context, sizeof(RowSpill));
	int			null_lanes = tess_spill_columns_null_lanes(rows_words(state));

	config.parent_context = context;
	config.kernels = state->kernels;
	config.npartitions = ROWS_PARTS;
	config.level = (uint32) level;
	config.fingerprint = (uint64) rows_words(state);
	config.max_len = MaxAllocHugeSize;
	config.buffer_len = TESS_SPILL_BUFFER_LEN(get_hash_memory_limit());
	spill->file = tess_spill_create(&config);
	spill->level = level;
	for (int part = 0; part < ROWS_PARTS; part++)
	{
		RowWriter  *writer = &spill->writers[part];

		writer->chunk_len = TESS_SPILL_COLUMNS_HEADER +
			sizeof(uint64) * ROWS_BLOCK_ROWS * (null_lanes + rows_words(state));
		writer->chunk = MemoryContextAlloc(context, writer->chunk_len);
		writer->values_len = ROWS_BLOCK_VALUES;
		writer->values = MemoryContextAlloc(context, writer->values_len);
		rows_writer_reset(state, writer);
	}
	state->partitions += ROWS_PARTS;
	return spill;
}

/* Write a partition's block: its values, then its columns. */
static void
rows_flush(TessAggState *state, RowSpill *spill, int part)
{
	RowWriter  *writer = &spill->writers[part];

	if (writer->rows == 0)
		return;
	tess_spill_columns_set_rows(writer->chunk, writer->rows);
	state->disk_bytes += tess_spill_write(spill->file, part, TESS_SPILL_VALUES, 0,
										  writer->values, writer->values_used, NULL);
	state->disk_bytes += tess_spill_write(spill->file, part, TESS_SPILL_COLUMNS, 0,
										  writer->chunk, writer->chunk_len, NULL);
	state->spilled++;
	writer->written += writer->rows;
	rows_writer_reset(state, writer);
}

/*
 * The rows of rows to their partitions by the bits of their hash of the
 * spill's level: the values of every computed column, a by-reference one
 * copied into the block's values.
 */
static void
rows_write(TessAggState *state, RowSpill *spill, const TessRowMask *rows)
{
	int			null_lanes = tess_spill_columns_null_lanes(rows_words(state));
	int			shift = 32 - ROWS_PART_BITS * (spill->level + 1);
	int			row = -1;

	/* By the values' hashes when a key has a dictionary: the numbers are one table's. */
	const uint32 *hashes = state->has_dicts ? state->value_hashes : state->hashes;

	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		int			part = (int) ((hashes[row] >> shift) & (ROWS_PARTS - 1));
		RowWriter  *writer = &spill->writers[part];
		Size		need = 0;
		uint64	   *nulls;
		uint64	   *words;

		for (int column = 0; column < state->ncomputed; column++)
			if (!state->computed_byvals[column] && !state->computed_columns[column].isnull[row])
				need += MAXALIGN(datumGetSize(state->computed_columns[column].values[row], false,
											  state->computed_lens[column]));
		if (writer->rows == writer->capacity ||
			(writer->rows > 0 && writer->values_used + need > writer->values_len))
			rows_flush(state, spill, part);
		if (writer->values_used + need > writer->values_len)
		{
			writer->values_len = Max(writer->values_len * 2, writer->values_used + need);
			writer->values = repalloc_huge(writer->values, writer->values_len);
		}
		nulls = tess_spill_columns_lane(writer->chunk, 0) + writer->rows;
		words = nulls + (Size) writer->capacity * null_lanes;
		for (int lane = 0; lane < null_lanes; lane++)
			nulls[(Size) writer->capacity * lane] = 0;
		for (int column = 0; column < state->ncomputed; column++)
		{
			const TessDatumColumn *from = &state->computed_columns[column];
			uint64	   *lane = words + (Size) writer->capacity * column;

			if (from->isnull[row])
			{
				nulls[(Size) writer->capacity * (column / 64)] |= UINT64CONST(1) << (column % 64);
				lane[0] = 0;
			}
			else if (state->computed_byvals[column])
				lane[0] = (uint64) from->values[row];
			else
			{
				Size		size = datumGetSize(from->values[row], false,
												state->computed_lens[column]);

				memcpy(writer->values + writer->values_used,
					   DatumGetPointer(from->values[row]), size);
				lane[0] = writer->values_used;
				writer->values_used += MAXALIGN(size);
			}
		}
		writer->rows++;
		state->spilled_rows++;
	}
}

/* End a level's writes and put it on the stack of partitions to read. */
static void
rows_spill_close(TessAggState *state)
{
	RowSpill   *spill = state->rows_spill;

	if (spill == NULL)
		return;
	for (int part = 0; part < ROWS_PARTS; part++)
		rows_flush(state, spill, part);
	tess_spill_finish(spill->file);
	spill->next = 0;
	state->rows_pending = lcons(spill, state->rows_pending);
	state->rows_spill = NULL;
}

static void
rows_spill_free_one(RowSpill *spill)
{
	for (int part = 0; part < ROWS_PARTS; part++)
	{
		pfree(spill->writers[part].chunk);
		pfree(spill->writers[part].values);
	}
	tess_spill_free(spill->file);
	pfree(spill);
}

/* Forget every level of partitions and the partition being read. */
static void
rows_spill_free(TessAggState *state)
{
	if (state->reader != NULL)
	{
		if (state->reader->file != NULL)
			tess_spill_close(state->reader->file);
		state->reader->file = NULL;
	}
	if (state->rows_spill != NULL)
		rows_spill_free_one(state->rows_spill);
	state->rows_spill = NULL;
	foreach_ptr(RowSpill, spill, state->rows_pending)
		rows_spill_free_one(spill);
	list_free(state->rows_pending);
	state->rows_pending = NIL;
	state->frozen = false;
	state->replaying = false;
}

/* A column of the window of rows read back: the computed column it holds. */
static void
reader_get_column(TessBatch *batch, int column, const TessRowMask *rows,
				  TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessAggState *state = (TessAggState *) batch->private_data;
	int			computed = column - state->child_layout.ncolumns;

	if (computed < 0 || computed >= state->ncomputed)
		elog(ERROR, "TessAgg read back no column %d", column);
	result->values = state->reader->column_values[computed];
	result->isnull = state->reader->column_isnull[computed];
	result->nrows = batch->rows.nrows;
}

static const TessBatchOps reader_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = reader_get_column,
};

/*
 * The next window of up to 64 rows of the partition being read, as a
 * batch whose computed columns are the values written; NULL at the end.
 */
static TessBatch *
reader_next(TessAggState *state)
{
	RowReader  *reader = state->reader;
	int			null_lanes = tess_spill_columns_null_lanes(rows_words(state));
	uint32		take;
	Size		capacity;

	while (reader->next >= reader->rows)
	{
		TessSpillHeader header;

		CHECK_FOR_INTERRUPTS();
		if (reader->file == NULL || !tess_spill_read_header(reader->file, &header))
			return NULL;
		if (header.kind != TESS_SPILL_VALUES)
			elog(ERROR, "TessAgg read a damaged partition of rows");
		if (header.len > reader->values_len)
		{
			reader->values_len = Max(header.len, reader->values_len * 2);
			reader->values = repalloc_huge(reader->values, reader->values_len);
		}
		tess_spill_read_body(reader->file, reader->values, header.len);
		if (!tess_spill_read_header(reader->file, &header) ||
			header.kind != TESS_SPILL_COLUMNS || header.len > reader->chunk_len)
			elog(ERROR, "TessAgg read a damaged partition of rows");
		tess_spill_read_body(reader->file, reader->chunk, header.len);
		reader->rows = tess_spill_columns_rows(reader->chunk);
		reader->next = 0;
	}
	capacity = tess_spill_columns_capacity(reader->chunk);
	take = Min(reader->rows - reader->next, 64);
	for (int column = 0; column < state->ncomputed; column++)
	{
		const uint64 *nulls = tess_spill_columns_lane(reader->chunk, 0) +
			capacity * (column / 64) + reader->next;
		const uint64 *lane = tess_spill_columns_lane(reader->chunk, 0) +
			capacity * (null_lanes + column) + reader->next;

		for (uint32 row = 0; row < take; row++)
		{
			bool		isnull = ((nulls[row] >> (column % 64)) & 1) != 0;

			reader->column_isnull[column][row] = isnull;
			reader->column_values[column][row] = isnull ? (Datum) 0 :
				state->computed_byvals[column] ? (Datum) lane[row] :
				PointerGetDatum(reader->values + lane[row]);
		}
	}
	reader->next += take;
	reader->bits = take == 64 ? UINT64_MAX : (UINT64CONST(1) << take) - 1;
	reader->batch.rows.nrows = (int) take;
	reader->batch.rows.bits = &reader->bits;
	return &reader->batch;
}

/*
 * Open the next partition to read, depth first: the last level written
 * first; false when none is left. Its groups start in a table of their
 * own, and a partition too large again spills into a level below.
 */
static bool
rows_next_partition(TessAggState *state)
{
	RowReader  *reader = state->reader;

	if (reader->file != NULL)
		tess_spill_close(reader->file);
	reader->file = NULL;
	while (state->rows_pending != NIL)
	{
		RowSpill   *spill = linitial(state->rows_pending);

		while (spill->next < ROWS_PARTS)
		{
			int			part = spill->next++;

			if (spill->writers[part].written == 0)
				continue;
			reader->file = tess_spill_open(spill->file, 0, part);
			reader->rows = 0;
			reader->next = 0;
			/* The level below takes the next bits, the last level none. */
			state->rows_level = spill->level + 1;
			return true;
		}
		state->rows_pending = list_delete_first(state->rows_pending);
		rows_spill_free_one(spill);
	}
	return false;
}

/* The reader's buffers and batch, once. */
static void
rows_reader_init(TessAggState *state)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	RowReader  *reader = MemoryContextAllocZero(context, sizeof(RowReader));
	int			null_lanes = tess_spill_columns_null_lanes(rows_words(state));

	reader->chunk_len = TESS_SPILL_COLUMNS_HEADER +
		sizeof(uint64) * ROWS_BLOCK_ROWS * (null_lanes + rows_words(state));
	reader->chunk = MemoryContextAlloc(context, reader->chunk_len);
	reader->values_len = ROWS_BLOCK_VALUES;
	reader->values = MemoryContextAlloc(context, reader->values_len);
	reader->column_values = MemoryContextAlloc(context, sizeof(Datum *) * state->ncomputed);
	reader->column_isnull = MemoryContextAlloc(context, sizeof(bool *) * state->ncomputed);
	for (int column = 0; column < state->ncomputed; column++)
	{
		reader->column_values[column] = MemoryContextAllocZero(context, sizeof(Datum) * 64);
		reader->column_isnull[column] = MemoryContextAllocZero(context, sizeof(bool) * 64);
	}
	reader->batch.abi_version = TESS_BATCH_ABI_VERSION;
	reader->batch.struct_size = sizeof(TessBatch);
	reader->batch.ops = &reader_batch_ops;
	reader->batch.private_data = state;
	reader->batch.table_oid = InvalidOid;
	state->reader = reader;
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

void
tess_agg_planner_init(void)
{
	previous_create_upper_paths_hook = create_upper_paths_hook;
	create_upper_paths_hook = create_upper_paths;
}

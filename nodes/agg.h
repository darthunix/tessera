/*
 * Definitions shared by the planner (agg_planner.c), the executor (agg.c)
 * and the spill (agg_spill.c) of TessAgg.
 */
#ifndef TESSERA_NODES_AGG_H
#define TESSERA_NODES_AGG_H

#include "postgres.h"

#include "nodes/execnodes.h"
#include "nodes/primnodes.h"

#include "tessera/kernel_ops.h"

#include "table_chunks.h"
#include "tessera/runtime.h"
#include "tessera/table.h"

#include "memory_limit.h"

/* A generic sum state's payload words: the kernels' state, then the rest's address. */
#define AGG_SUM_STATE_WORDS (TESS_TABLE_SUM_WORDS + 1)

/* min or max of numeric with groups: the kernels' extreme state (TESS_TABLE_EXTREME_WORDS). */
#define AGG_EXTREME_STATE_WORDS TESS_TABLE_EXTREME_WORDS

/*
 * The words of flags of a group's record of n states, before the states:
 * one for each 64 and one for none, the flag of state i the bit i % 64 of
 * word i / 64, as tess_table_combine reads them.
 */
#define AGG_FLAG_WORDS(n) Max(1, ((n) + 63) / 64)

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
 * An entry of a key dictionary (agg.c): a value, its hash, its number and
 * the hash table's status; the planner sizes the dictionaries by it.
 */
typedef struct KeyEntry
{
	Datum		value;
	uint32		hash;
	uint32		number;
	char		status;
} KeyEntry;

/* The planner's judgement of an aggregate, which the executor repeats. */
extern int	aggregate_kind(Oid aggfnoid);
extern bool generic_supported(const Aggref *agg);
extern bool batch_aggregate(const Aggref *agg);
extern bool distinct_matters(const Aggref *agg);
extern bool sum_state_aggregate(const Aggref *agg);
extern bool own_partial_aggregate(const Aggref *agg);
extern Size agg_float_state_bytes(const Aggref *agg);
#ifdef HAVE_INT128
extern Datum agg_int128_numeric(int128 value, int scale);
#endif

/*
 * An aggregate of the scan tuple the node derives from states it keeps
 * for several aggregates of one expression e, its argument a form a·e +
 * b of e (agg_derive): from S, the sum of e over the rows it reads, and
 * n, their count. Its kind is its aggregate's, by the type of its form.
 */
typedef enum AggDerivedKind
{
	AGG_DERIVED_SUM_INT8,		/* sum of a smallint or integer form: a bigint */
	AGG_DERIVED_SUM_NUMERIC,	/* sum of a bigint form: a numeric */
	AGG_DERIVED_AVG_INT8,		/* avg of a smallint or integer form, its sum a bigint */
	AGG_DERIVED_AVG_NUMERIC,	/* avg of a bigint form */
	AGG_DERIVED_COUNT,
	AGG_DERIVED_MIN,			/* min or max: the form at an extreme of e */
	AGG_DERIVED_MAX
} AggDerivedKind;

/*
 * The plan data's derivations: for each derived output, its states, the
 * sum of e and their count (-1 where the sum state counts them, for a
 * bigint e), or for min and max the extreme of e it takes and -1, and its
 * constants a and b, each an int8 Const or, past an int8, a numeric one. Its checks of errors: for each, the state whose
 * argument is e and e's type, and the probes, each form's argument with
 * e an inner column (AggCheck).
 */
#define AGG_DERIVED_STATES 2
#define AGG_DERIVED_CONSTANTS 2
#define AGG_CHECK_FIELDS 2

#ifdef HAVE_INT128
/*
 * The sum of e over the rows a derived output reads, at its scale, its
 * rest past 128 bits or NaN, and their count.
 */
typedef struct AggSum
{
	int128		sum;
	int			scale;
	Datum		rest;			/* a numeric to add, or 0 */
	int64		count;
} AggSum;
#endif

/* An aggregate of the scan tuple: a state's value, or derived from states. */
typedef struct AggOutput
{
	int			state;			/* the state, or -1 when derived */
	AggDerivedKind kind;
	/* The sum of e, or an extreme of e; the count of e, or -1. */
	int			source;
	int			count_state;
	/* min and max: the form's type. */
	Oid			type;
	/*
	 * a and b as decimals, a mantissa at a scale, where both are within
	 * 128 bits (exact), and as numerics.
	 */
	bool		exact;
#ifdef HAVE_INT128
	int128		a;
	int128		b;
#endif
	int			a_scale;
	int			b_scale;
	Datum		a_numeric;
	Datum		b_numeric;
} AggOutput;

/*
 * The state of the node and the groups it gives out (agg.c), read and
 * written by its spill (agg_spill.c).
 */
/* Groups per output batch. */
#define AGG_GROUP_ROWS 64

typedef struct AggValue AggValue;
typedef struct KeyDict KeyDict;

/*
 * A DISTINCT set's pairs past hash_mem (agg_spill.c), as a frozen table's
 * rows: the machine of the set in memory, which freezes it; the level its
 * pairs go to once frozen, NULL before; the stack of levels to read, the
 * last written on top; the reader of their partitions; and the memory the
 * machine last weighed with a batch of pairs in the set, 0 before, since
 * its answer then moves with the memory alone.
 */
typedef struct PairSpill
{
	TessRowsMachine machine;
	struct RowSpill *writing;
	List	   *pending;
	struct RowReader *reader;
	Size		weighed;
} PairSpill;

/* The execution state of TessAgg. */
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
	 * copies of it still to go (-1 before they are counted). The rows of
	 * the left side read since it started: without any, the right side is
	 * not read, as the core's SetOp does not read its inner input.
	 */
	int			setop;
	int			side;
	uint64		setop_left_rows;
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
	/*
	 * The states the node keeps, and for each aggregate of the scan tuple
	 * after the keys, its output: the state whose value it is.
	 */
	AggValue   *values;
	int			nvalues;
	int			noutputs;
	AggOutput  *outputs;
	/* The outputs derived, and whether one is by reference, a numeric. */
	int			nderived;
	bool		derived_by_ref;
	/* The checks of errors of the forms folded (AggCheck). */
	struct AggCheck *checks;
	int			nchecks;
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
	 * table of their own, so the groups do not spill. With GROUP BY, the
	 * set of an argument a word holds sends its pairs to disk past hash_mem
	 * (spilling_sets of them, what every level of their pairs shares, the
	 * pairs written), read back into their groups before they go out.
	 */
	bool		has_distinct;
	int			spilling_sets;
	TessRowsParams pairs_params;
	uint64		spilled_pairs;
	/*
	 * The bytes the planner expects the groups to take, where it gave the
	 * node a grouping whose pairs do not fit by its estimate, else 0: while
	 * the input is read the sets weigh the groups by the most of their
	 * bytes and these, so that a set that freezes early keeps their room
	 * for the groups still to come.
	 */
	Size		groups_bytes_estimate;
	/* Whether an ungrouped COUNT(DISTINCT)'s argument may come as bytes. */
	bool		has_distinct_bytes;
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
	/*
	 * Of each read column, whether a computed argument reads it as bytes
	 * (tess_projection_reads_bytes), found at the first read: then each
	 * batch is asked whether it offers the form for that column alone.
	 */
	bool	   *read_as_bytes;
	/* Generic aggregates: their states' context, and the AggState they see. */
	AggState   *generic_agg;
	/*
	 * GROUP BY with a generic aggregate: the groups' states are words of
	 * their records, a by-reference one the address of its copy, so the
	 * groups do not spill; a group's final values live until the next.
	 */
	bool		has_generic;
	MemoryContext generic_output;
	/* Per batch: the payloads of the records of the rows it reads. */
	uint8	  **payloads;
	/*
	 * Such groups past hash_mem go the core's way: the table takes no new
	 * group (frozen), and the rows of the groups it lacks go, their keys'
	 * and arguments' values, to partitions on disk by bits of their hash,
	 * each read back later into a table of its own; a partition too large
	 * again splits by the next bits. The sets of partitions still to read
	 * wait in a stack, the one being written on top.
	 */
	bool		frozen;
	/*
	 * The level of rows being written, the stack of those to read, the
	 * machine of the table (relspill's Rows), which freezes it, and what
	 * every level of rows shares.
	 */
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
	 * A key through a dictionary that reads bytes (TESS_KEY_BYTES_WHOLE)
	 * and is a column of the child: that column, which a batch may offer in
	 * the bytes form, else -1; of the batch at hand, whether it did, and
	 * the form it gave, which the dictionary numbers in place of Datums.
	 */
	int			key_bytes_column[TESS_TABLE_MAX_KEYS];
	bool		key_by_bytes[TESS_TABLE_MAX_KEYS];
	TessBytesColumn key_bytes[TESS_TABLE_MAX_KEYS];
	/* The values a frozen table makes of such keys for its rows to disk, a batch's. */
	MemoryContext key_values_context;

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
	/* The words of flags of a batch of groups, AGG_GROUP_ROWS a word of each. */
	uint64	   *flag_words;
	int			nflag_words;
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
	/* GROUP BY: the bytes of a group's record (tess_table_record_size). */
	Size		record_size;
	/* The payloads of the walk's groups, for the sum and extreme states. */
	uint8	   *walked_payloads[AGG_GROUP_ROWS];
	TessRowsMachine rows_machine;
	TessRowsParams rows_params;
	/*
	 * The machine of the groups' spill (relspill's group), which watches a
	 * build from its start, and what every level of it shares.
	 */
	TessGroupMachine machine;
	TessGroupParams spill_params;
} TessAggState;

/* The eighths of hash_mem past which the groups spill, the rest a batch's. */
#define AGG_SPILL_FILL 7

/* Raise the error a kernel or table call stored, if it failed. */
#define check(state, code) tess_status_check((code), &(state)->status)
/* The same for a call of the table, damaged data over chunks read back. */
#define check_table(state, read_back, code) \
	((read_back) ? tess_spill_table_check : tess_status_check) ((code), &(state)->status)

/*
 * Once the groups spill, the chunk arrays keep two slots first:
 * AGG_SOURCE, empty or a chunk read back for a merge or a split, and
 * AGG_EMPTY, an empty chunk a partition without one appends to, which
 * sends its rows back for a chunk. Neither is the table's own.
 */
#define AGG_SOURCE 0
#define AGG_EMPTY 1

/* The executor's side of the table, which the spill calls (agg.c). */
extern void note_memory(TessAggState *state);
extern void note_memory_alive(TessAggState *state, Size alive);
extern uint64 first_capacity(uint64 capacity);
extern void *new_index(TessAggState *state, uint64 capacity, Size *size);
extern void regrow_table(TessAggState *state, uint64 groups);
extern void agg_reserve_chunks(TessAggState *state, int nchunks);

/*
 * The spill (agg_spill.c): the groups' partitions past hash_mem and their
 * merge, and the rows of generic aggregates past it, read back by level.
 */
/*
 * The partitions of every level of rows, and the hash bits each takes; the
 * level plan gives a level its bits, and says whether a level fits below.
 */
#define ROWS_PART_BITS 5
#define ROWS_PARTS (1 << ROWS_PART_BITS)
extern void agg_spill_begin(TessAggState *state);
extern void agg_spill_check(TessAggState *state, bool force);
extern TessGroupActionKind agg_partial_check(TessAggState *state, Size memory);
extern void agg_make_room(TessAggState *state);
extern void agg_find_partitioned(TessAggState *state, TessRowMask *pending,
								 TessRowMask *inserted);
extern bool agg_advance(TessAggState *state);
extern void agg_finish_input(TessAggState *state);
extern Size agg_spill_memory(TessAggState *state);
extern void agg_spill_free(TessAggState *state);
extern struct RowSpill *rows_spill_create(TessAggState *state, const TessRowsAction *freeze);
extern void rows_machine_begin(TessAggState *state);
extern TessRowsAction rows_step(TessAggState *state, TessRowsMachine *machine,
								const uint64 *written, int nwritten, TessRowsEvent event);
extern void rows_write(TessAggState *state, struct RowSpill *spill,
					   const TessRowMask *rows);
extern void rows_spill_close(TessAggState *state);
extern Size rows_spill_memory(TessAggState *state);
extern void rows_spill_free(TessAggState *state);
extern TessBatch *reader_next(TessAggState *state);
extern bool rows_next_partition(TessAggState *state);
extern void rows_reader_init(TessAggState *state);
extern void pairs_params_init(TessAggState *state, int sets);
extern void pairs_begin(TessAggState *state, PairSpill *pairs);
extern void pairs_grown(TessAggState *state, PairSpill *pairs, Size memory, uint64 records);
extern void pairs_write(TessAggState *state, PairSpill *pairs, const TessRowMask *rows,
						const Datum *refs, const Datum *values, const uint32 *hashes);
extern void pairs_close(TessAggState *state, PairSpill *pairs);
extern bool pairs_next_partition(TessAggState *state, PairSpill *pairs, bool last);
extern int	pairs_read(TessAggState *state, PairSpill *pairs, Datum **columns);
extern Size pairs_memory(const PairSpill *pairs);
extern void pairs_free(PairSpill *pairs);

#endif							/* TESSERA_NODES_AGG_H */

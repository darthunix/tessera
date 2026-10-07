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
#include "tessera/runtime.h"
#include "tessera/table.h"

#include "memory_limit.h"

/* A generic sum state's payload words: the kernels' state, then the rest's address. */
#define AGG_SUM_STATE_WORDS (TESS_TABLE_SUM_WORDS + 1)

/* min or max of numeric with groups: the kernels' extreme state (TESS_TABLE_EXTREME_WORDS). */
#define AGG_EXTREME_STATE_WORDS TESS_TABLE_EXTREME_WORDS

/* A group's aggregate states have one flag bit each in a payload word. */
#define AGG_MAX_GROUPED 64

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
extern bool sum_state_aggregate(const Aggref *agg);
extern bool own_partial_aggregate(const Aggref *agg);

/*
 * The state of the node and the groups it gives out (agg.c), read and
 * written by its spill (agg_spill.c).
 */
/* Groups per output batch. */
#define AGG_GROUP_ROWS 64

typedef struct AggValue AggValue;
typedef struct KeyDict KeyDict;

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
	/* GROUP BY: the bytes of a group's record (tess_table_record_size). */
	Size		record_size;
} TessAggState;

/* Raise the error a kernel or table call stored, if it failed. */
#define check(state, code) tess_status_check((code), &(state)->status)

/* The executor's side of the table, which the spill calls (agg.c). */
extern void note_memory(TessAggState *state);
extern uint64 first_capacity(uint64 capacity);
extern void *new_index(TessAggState *state, uint64 capacity, Size *size);
extern void regrow_table(TessAggState *state, uint64 groups);

/*
 * The spill (agg_spill.c): the groups' partitions past hash_mem and their
 * merge, and the rows of generic aggregates past it, read back by level.
 */
/* Hash bits a level of partitions of rows takes, and the most levels. */
#define ROWS_PART_BITS 5
#define ROWS_PARTS (1 << ROWS_PART_BITS)
#define ROWS_MAX_LEVELS (32 / ROWS_PART_BITS)
extern void agg_start_spill(TessAggState *state);
extern void agg_make_room(TessAggState *state);
extern void agg_find_partitioned(TessAggState *state, TessRowMask *pending,
								 TessRowMask *inserted);
extern bool agg_advance(TessAggState *state);
extern void agg_finish_input(TessAggState *state);
extern Size agg_spill_memory(TessAggState *state);
extern void agg_spill_free(TessAggState *state);
extern struct RowSpill *rows_spill_create(TessAggState *state, int level);
extern void rows_write(TessAggState *state, struct RowSpill *spill,
					   const TessRowMask *rows);
extern void rows_spill_close(TessAggState *state);
extern void rows_spill_free(TessAggState *state);
extern TessBatch *reader_next(TessAggState *state);
extern bool rows_next_partition(TessAggState *state);
extern void rows_reader_init(TessAggState *state);

#endif							/* TESSERA_NODES_AGG_H */

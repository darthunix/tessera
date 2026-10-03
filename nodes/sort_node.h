/*
 * The state of TessSort and what its files share: the node (sort.c), its
 * top-N heap (sort_topn.c) and its external sort (sort_external.c).
 */
#ifndef TESSERA_NODES_SORT_NODE_H
#define TESSERA_NODES_SORT_NODE_H

#include "postgres.h"

#include "lib/binaryheap.h"
#include "nodes/execnodes.h"
#include "utils/sortsupport.h"

#include "tessera/kernel_ops.h"
#include "tessera/runtime.h"
#include "tessera/sort.h"

#include "internal.h"

/* Rows of an output batch. */
#define SORT_ROWS 64

/*
 * External sort. Runs are written into sets of files (runtime/spill.c), a
 * run a partition of its set, the runs of the input into one set, those
 * of a pass that merges them into another: one file each, not one per
 * run, whose closing and deleting took 8 % of a sort of 2 M rows in 44
 * runs. A set goes once its last run does.
 */
#define SORT_SET_RUNS 1024

typedef struct RunSet
{
	TessSpill  *file;
	int			used;
	int			alive;
	bool		finished;
} RunSet;

/*
 * A run is a sorted part of the input on disk, a partition of its set:
 * pairs of blocks, one of the by-reference values of some rows,
 * all of them one after another, and one of the rows as a chunk of columns
 * (tessera/spill.h): a lane of the output columns' NULL bits, a lane per
 * output column (a by-value Datum, or a value's byte in its block of
 * values) and a lane per word of the rows' sort items, the reference
 * left out, which the merge compares.
 */
typedef struct SortRun
{
	RunSet	   *set;
	int			partition;
	int			nblocks;
	int			slots;
	/* Where each block pair starts, its rows and the rows before it. */
	TessSpillPosition *positions;
	uint32	   *block_rows;
	uint64	   *block_first;
	uint64		rows;
} SortRun;

/* A run being written: its chunk of columns and its block of values. */
typedef struct RunWriter
{
	SortRun    *run;
	char	   *chunk;
	Size		chunk_len;
	/* The chunk's lanes of NULL bits. */
	int			null_lanes;
	uint32		capacity;
	uint32		rows;
	char	   *values;
	Size		values_len;
	Size		values_used;
	/* The columns of the rows being added (tess_spill_columns_append). */
	TessDatumColumn *columns;
} RunWriter;

/* A run being merged: its reader, the block pair in memory and the next row of it. */
typedef struct MergeInput
{
	SortRun    *run;
	TessSpillReader *reader;
	int			block;
	char	   *values;
	void	   *chunk;
	uint32		rows;
	uint32		place;
} MergeInput;

/*
 * What EXPLAIN shows, summed over the participants of a parallel plan
 * (TessSharedStats): the batches and rows read, memory and its overrun
 * past each one's work_mem, the participants that sorted, sorted
 * externally or kept a top-N heap, the runs, passes and bytes written, the
 * rows rebuilt, the participants that gave up abbreviated keys.
 */
enum
{
	SORT_BATCHES,
	SORT_INPUT_ROWS,
	SORT_MEMORY,
	SORT_OVERRUN,
	SORT_SORTED,
	SORT_EXTERNAL,
	SORT_TOPN,
	SORT_RUNS,
	SORT_PASSES,
	SORT_DISK,
	SORT_REBUILT,
	SORT_ABBREV_GIVEN_UP,
	SORT_NCOUNTERS
};

/* The counters of the node. */
typedef struct SortCounters
{
	uint64		batches;
	uint64		rows;
	Size		memory;
} SortCounters;

/* How a type's abbreviated key orders, to make it a word the kernels order. */
typedef enum SortAbbrev
{
	SORT_ABBREV_NONE,
	SORT_ABBREV_UNSIGNED,
	SORT_ABBREV_SIGNED,
	/* numeric: the reverse of a signed integer's order. */
	SORT_ABBREV_REVERSED,
	SORT_ABBREV_UINT32,
	SORT_ABBREV_INT32
} SortAbbrev;

typedef struct TessSortState
{
	CustomScanState css;
	PlanState  *child;
	TessInput  *input;
	TessOutput *output;
	const TessKernelOps *kernels;
	/* The output columns: each one's column in the child's batches. */
	int			ncolumns;
	int		   *child_columns;
	/* The keys: each one's output column, kind and flags as planned. */
	int			nkeys;
	int		   *key_columns;
	TessSortKey keys[TESS_TABLE_MAX_KEYS];
	/* A key held a NULL: its items take the bit for it. */
	bool		key_nulls[TESS_TABLE_MAX_KEYS];
	/*
	 * Other types: the kernels order the keys up to the first one they do
	 * not order by words (generic, -1 for none), nkernel of them, and its
	 * word is its abbreviated key, or 0 for a type without one; the rows
	 * whose words are equal are ordered in C by the comparisons of that key
	 * and the ones after it (ssup, one per key, from generic on). The
	 * abbreviated keys of a batch, in a context reset per batch; the rows
	 * of a group, their keys' values and their order.
	 */
	int			nkernel;
	int			generic;
	SortSupportData *ssup;
	TessSortAbbrev abbrev;
	MemoryContext abbrev_context;
	Datum	   *abbrev_values;
	bool	   *abbrev_isnull;
	TessDatumColumn abbrev_column;
	int			abbrev_capacity;
	/*
	 * The rows whose abbreviated keys were made, and the count at which
	 * the type's abort test runs next; whether the keys were given up.
	 */
	uint64		abbrev_rows;
	uint64		abbrev_next;
	bool		abbrev_given_up;
	/* The order of the type's abbreviated keys the node took, for EXPLAIN. */
	SortAbbrev	abbrev_taken;
	struct TieRow *tie_rows;
	Datum	   *tie_values;
	bool	   *tie_isnull;
	uint64		tie_capacity;
	binaryheap *merge_heap;
	MergeInput *merging;
	TessRows   *rows;
	/* What the rows are made with, to make them anew. */
	TessRowsConfig rows_config;
	TessTableKeyKind kinds[TESS_TABLE_MAX_KEYS];
	/*
	 * Top-N: the rows a parent needs (-1 for all), as it set them; the
	 * bound the rows were read under; the heap of the best rows' items,
	 * its capacity and length, the items an external sort's, of
	 * item_words with every key's bit for NULL (ext_keys), so that the
	 * width never changes; the rebuilds of the rows from the heap's.
	 */
	int64		bound;
	int64		used_bound;
	bool		topn;
	uint64	   *heap;
	Size		heap_capacity;
	uint64		heap_len;
	uint64		compactions;
	/*
	 * Top-N of a generic key: the heap is the node's, in C, one slot more
	 * than the bound for the row coming in, ordered by the items' words and
	 * then the comparisons, with the values of the keys from the first
	 * generic one on of each item, pointers into the records; the lanes of
	 * a batch's items and each row's lane.
	 */
	Datum	   *top_values;
	bool	   *top_isnull;
	uint64	   *top_lanes[TESS_SORT_MAX_ITEM_WORDS];
	int		   *top_lane_of;
	Datum	   *top_gathered;
	bool	   *top_gathered_null;
	int			top_batch_capacity;
	/* The child's columns of a batch, one per output column. */
	TessDatumColumn *columns;
	TessTableKey table_keys[TESS_TABLE_MAX_KEYS];
	uint32	   *batch_refs;
	int			capacity;
	/* The records in order, once sorted. */
	bool		sorted;
	uint32	   *refs;
	uint64		count;
	/*
	 * The rows returned: the last one's place in the order, -1 before the
	 * first and count after the last; the batch being shown or read, its
	 * first place and its rows.
	 */
	int64		current;
	TessBatch	batch;
	bool		published;
	uint64		start;
	uint64		window_bits[1];
	Datum	  **values;
	bool	  **isnull;
	bool	   *gathered;
	/*
	 * Columns gathered for the parent of the batch shown, and of the one
	 * before: all of them once it read more than one.
	 */
	int			columns_read;
	int			columns_read_before;
	SortCounters counters;
	/*
	 * External sort: the flags the node began with; whether the rows went
	 * to runs, the runs to merge, the keys every item has a bit for NULL
	 * in and its words (a top-N heap's too), the rows of a block, the passes that merged runs
	 * into longer ones and the bytes written. The last merge streams from
	 * the inputs, or, for a scan backward, reads one run by blocks: the
	 * block in memory. Blocks the rows put out may point into are freed
	 * with the next rows.
	 */
	int			eflags;
	/* The participants' counters, under a Gather Merge. */
	TessSharedStats *stats;
	bool		external;
	SortRun   **runs;
	int			nruns;
	int			run_slots;
	TessSortKey ext_keys[TESS_TABLE_MAX_KEYS];
	int			item_words;
	int			ext_words;
	uint32		block_rows;
	Size		block_values;
	int			fan_in;
	RunSet	   *writing;
	uint32		merge_state[TESS_SORT_MERGE_STATE_WORDS];
	int			merge_passes;
	int			runs_written;
	uint64		disk_bytes;
	MergeInput *inputs;
	int			ninputs;
	List	   *retired;
	bool		single;
	MergeInput	shown;
} TessSortState;

/* The functions one file of the node calls in another. */
extern void sort_abbreviate_column(TessSortState *state, const TessDatumColumn *column,
								   const TessRowMask *rows);
extern void sort_ties(TessSortState *state, uint64 **items, int words, uint32 *refs, uint64 count,
					  bool free_items);
extern void sort_note_memory(TessSortState *state, Size extra);
extern void sort_batch_column(TessSortState *state, TessBatch *batch, int column);
extern void sort_batch_keys(TessSortState *state, TessBatch *batch);
extern bool sort_is_key_column(TessSortState *state, int column);
extern void sort_append_rows(TessSortState *state, TessBatch *batch);
extern void sort_top_batch_generic(TessSortState *state, TessBatch *batch);
extern void sort_top_batch(TessSortState *state, TessBatch *batch);
extern bool sort_choose_topn(TessSortState *state);

/* Top-N: the rows past which the rows are made anew from the heap's (compact_rows). */
static inline double
sort_topn_rebuild_rows(double capacity)
{
	return Max(4 * capacity, 65536.0);
}

/* Top-N: the words of a heap of capacity items; a generic key's has a slot more. */
static inline double
sort_topn_heap_words(const TessSortState *state, double capacity)
{
	return (capacity + (state->generic >= 0 ? 1 : 0)) * state->item_words;
}

/* Top-N: a generic key's heap's values, of the keys from the generic one on. */
static inline double
sort_topn_value_slots(const TessSortState *state, double capacity)
{
	return state->generic >= 0 ? (capacity + 1) * (state->nkeys - state->generic) : 0;
}

/* Top-N: the bytes of a heap of capacity items, with a generic key's values. */
static inline double
sort_topn_heap_bytes(const TessSortState *state, double capacity)
{
	return sort_topn_heap_words(state, capacity) * sizeof(uint64) +
		sort_topn_value_slots(state, capacity) * (sizeof(Datum) + sizeof(bool));
}
extern int sort_run_words(TessSortState *state);
extern void sort_set_finish(TessSortState *state);
extern void sort_spill_run(TessSortState *state);
extern void sort_plan_external(TessSortState *state);
extern void sort_merge_runs(TessSortState *state);
extern void sort_restart_merge(TessSortState *state);
extern void sort_free_external(TessSortState *state);
extern void sort_show_block_of(TessSortState *state, uint64 place);
extern int sort_external_window(TessSortState *state, uint64 start, bool backward);

#endif							/* TESSERA_NODES_SORT_NODE_H */

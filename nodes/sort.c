#include "postgres.h"

#include "catalog/pg_collation_d.h"
#include "catalog/pg_opfamily_d.h"
#include "catalog/pg_type_d.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/paths.h"
#include "optimizer/planner.h"
#include "optimizer/tlist.h"
#include "lib/binaryheap.h"
#include "utils/builtins.h"
#include "utils/datum.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"
#include "utils/ruleutils.h"
#include "utils/sortsupport.h"

#include "tessera/kernel_ops.h"
#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * TessSort stands in for the core's Sort under ORDER BY: it reads every
 * batch of its batch child into records of the table format (TessRows),
 * each with the sort keys in its slots and the output columns in its
 * payload, sorts the records by their keys with the kernels
 * (tessera/sort.h) and returns them in order, in batches whose columns are
 * gathered from the records when a parent asks for them, or row by row,
 * forward and backward, to a row-wise parent. Keys are values of the
 * output: the kernels order words (integers, dates, times, booleans); a
 * key of another type orders by its type's comparison (sort support), its
 * word its abbreviated key, when it has one, and the rows the words leave
 * equal by the comparison in C (see "Other types"). Rows past work_mem are sorted into runs on disk
 * and merged (the external sort below). See docs/nodes.md.
 */

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
 * rows rebuilt.
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
	 * its capacity, length and item width; the keys it orders by, every
	 * one with its bit for NULL, so that the width never changes; the
	 * rebuilds of the rows from the heap's.
	 */
	int64		bound;
	int64		used_bound;
	bool		topn;
	uint64	   *heap;
	Size		heap_capacity;
	uint64		heap_len;
	int			words;
	TessSortKey top_keys[TESS_TABLE_MAX_KEYS];
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
	SortCounters counters;
	/*
	 * External sort: the flags the node began with; whether the rows went
	 * to runs, the runs to merge, the keys every item has a bit for NULL
	 * in and its words, the rows of a block, the passes that merged runs
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

static const CustomExecMethods sort_exec_methods;
static void reread_child(TessSortState *state);
static void compact_rows(TessSortState *state);
static void plan_external(TessSortState *state);
static create_upper_paths_hook_type previous_create_upper_paths_hook = NULL;

static Plan *sort_plan(PlannerInfo *root, RelOptInfo *rel,
					   CustomPath *best_path, List *tlist, List *clauses,
					   List *custom_plans);

static const CustomPathMethods sort_path_methods = {
	.CustomName = "TessSort",
	.PlanCustomPath = sort_plan,
};

/*
 * The place in the target of the expression a path key orders by, with
 * its kind and flags: an int4 or int8 of the integer operator family,
 * ascending or descending. False for any other key.
 */
bool
tess_sort_key_of(PathKey *pathkey, PathTarget *target, Relids relids, int *place,
			TessSortKey *key)
{
	EquivalenceClass *ec = pathkey->pk_eclass;

	if (ec->ec_has_volatile ||
		(pathkey->pk_cmptype != COMPARE_LT && pathkey->pk_cmptype != COMPARE_GT))
		return false;
	foreach_ptr(Expr, expr, target->exprs)
	{
		Oid			type = exprType((Node *) expr);
		TessTableKeyKind kind;

		if (!tess_word_key_order(type, pathkey->pk_opfamily) ||
			!tess_word_key_kind(type, &kind) ||
			find_ec_member_matching_expr(ec, expr, relids) == NULL)
			continue;
		*place = foreach_current_index(expr);
		key->kind = kind;
		key->flags = (pathkey->pk_cmptype == COMPARE_GT ? TESS_SORT_DESCENDING : 0) |
			(pathkey->pk_nulls_first ? TESS_SORT_NULLS_FIRST : 0);
		return true;
	}
	return false;
}

/*
 * A key the kernels do not order by words: its expression's place in the
 * target, the operator that orders it (of the path key's operator family,
 * < or >) and its collation. False for a volatile key or a type without
 * one.
 */
bool
tess_sort_generic_key(PathKey *pathkey, PathTarget *target, Relids relids, int *place,
					  Oid *sortop, Oid *collation)
{
	EquivalenceClass *ec = pathkey->pk_eclass;

	if (ec->ec_has_volatile ||
		(pathkey->pk_cmptype != COMPARE_LT && pathkey->pk_cmptype != COMPARE_GT))
		return false;
	foreach_ptr(Expr, expr, target->exprs)
	{
		EquivalenceMember *member = find_ec_member_matching_expr(ec, expr, relids);

		if (member == NULL)
			continue;
		/* The family's type, as varchar is compared as text. */
		*sortop = get_opfamily_member_for_cmptype(pathkey->pk_opfamily, member->em_datatype,
												  member->em_datatype, pathkey->pk_cmptype);
		if (!OidIsValid(*sortop))
			continue;
		*place = foreach_current_index(expr);
		*collation = ec->ec_collation;
		return true;
	}
	return false;
}

/*
 * The node's path in place of the core's full sort: the same planner
 * properties over the batch child of the sort's input, with the key
 * expressions, which the sort's targets hold, and their kinds and flags. NULL when a key is not one the
 * kernels sort, the output has too many columns, or the input cannot be
 * read in batches.
 */
static CustomPath *
make_sort_path(PlannerInfo *root, SortPath *sort)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	Path	   *input = sort->subpath;
	PathTarget *target = input->pathtarget;
	List	   *exprs = NIL;
	List	   *kinds = NIL;
	List	   *flags = NIL;
	List	   *sortops = NIL;
	List	   *collations = NIL;
	int			nkeys = list_length(sort->path.pathkeys);
	int			ncolumns = list_length(target->exprs);
	Path	   *child;

	if (nkeys == 0 || nkeys > TESS_TABLE_MAX_KEYS ||
		ncolumns == 0 || ncolumns > TESS_ROWS_MAX_COLUMNS)
		return NULL;
	foreach_node(PathKey, pathkey, sort->path.pathkeys)
	{
		TessSortKey key;
		int			place;
		Oid			sortop = InvalidOid;
		Oid			collation = InvalidOid;

		/*
		 * A key of another type orders by its type's comparison (sort
		 * support): the kernels order by its abbreviated key where it has
		 * one, and equal words by the comparison.
		 */
		if (tess_sort_key_of(pathkey, target, input->parent->relids, &place, &key))
		{
			/* Its comparison, for the rows a generic key before it leaves equal. */
			Oid			type = exprType(list_nth(target->exprs, place));

			sortop = get_opfamily_member_for_cmptype(pathkey->pk_opfamily, type, type,
													 pathkey->pk_cmptype);
			collation = pathkey->pk_eclass->ec_collation;
		}
		else if (tess_sort_generic_key(pathkey, target, input->parent->relids, &place,
								  &sortop, &collation))
		{
			/*
			 * A first key without an abbreviated key (float8, text under a
			 * collation of libc): every row one group, which the node
			 * orders by the comparison, as the core's sort does, and was
			 * measured no slower over the same scan (a million rows: text
			 * 2712 ms against 2640, float8 90.7 against 89.1, float8 and
			 * an integer 125.0 against 92.6); with a key before it, groups
			 * of that key's values. Under a limit, a type passed by value
			 * compares cheaply, and the core's bounded heap of tuples
			 * stays ahead of the node's (float8, LIMIT 10: 10.5 ms against
			 * 16.5; text 132.7 against 117.0 the other way): the core's.
			 */
			if (foreach_current_index(pathkey) == 0 && root->limit_tuples >= 0 &&
				get_typbyval(exprType(list_nth(target->exprs, place))))
				return NULL;
			key.kind = TESS_SORT_KIND_GENERIC;
			key.flags = (pathkey->pk_cmptype == COMPARE_GT ? TESS_SORT_DESCENDING : 0) |
				(pathkey->pk_nulls_first ? TESS_SORT_NULLS_FIRST : 0);
		}
		else
			return NULL;
		exprs = lappend(exprs, list_nth(target->exprs, place));
		kinds = lappend_int(kinds, (int) key.kind);
		flags = lappend_int(flags, (int) key.flags);
		sortops = lappend_int(sortops, (int) sortop);
		collations = lappend_int(collations, (int) collation);
	}
	/* Rows past work_mem go to runs on disk and merge: no gate on the rows. */
	child = tess_batch_input_path(root, input);
	if (child == NULL)
		return NULL;
	config.template_path = &sort->path;
	config.methods = &sort_path_methods;
	config.node = &tess_sort_node;
	config.children = list_make1(child);
	config.expressions = exprs;
	config.node_data = (Node *) list_make4(kinds, flags, sortops, collations);
	config.flags = CUSTOMPATH_SUPPORT_BACKWARD_SCAN;
	return tess_path_create(&config);
}

/*
 * The node's path in place of each of the core's full sorts of the
 * ordered relation, also one under a projection. Under LIMIT the limit
 * sets the node a bound at execution and it keeps the best rows in a heap
 * (top-N); WITH TIES, which passes no bound, keeps the core's sort, as
 * does a backend without the kernels module.
 */
static void
create_upper_paths(PlannerInfo *root, UpperRelationKind stage,
				   RelOptInfo *input_rel, RelOptInfo *output_rel, void *extra)
{
	ListCell   *lc;
	const TessKernelOps *kernels;

	if (previous_create_upper_paths_hook != NULL)
		previous_create_upper_paths_hook(root, stage, input_rel, output_rel,
										 extra);
	if (!*tess_runtime_api()->settings->enable || stage != UPPERREL_ORDERED ||
		root->parse->limitOption == LIMIT_OPTION_WITH_TIES)
		return;
	/* Without the kernels module there is nothing to sort with. */
	kernels = tess_runtime_kernels();
	if (kernels == NULL || !TESS_ABI_HAS_FIELD(kernels, TessKernelOps, sort))
		return;
	foreach(lc, output_rel->pathlist)
	{
		Path	   *path = lfirst(lc);
		ProjectionPath *projection = NULL;
		CustomPath *sort;

		if (IsA(path, ProjectionPath))
		{
			projection = (ProjectionPath *) path;
			path = projection->subpath;
		}
		/*
		 * A sort in every participant under a Gather Merge: the node sorts
		 * each participant's share, the Gather Merge merges them as it
		 * merges the core's sorts.
		 */
		if (IsA(path, GatherMergePath))
		{
			GatherMergePath *gather = (GatherMergePath *) path;
			ProjectionPath *below = NULL;
			Path	   *subpath = gather->subpath;

			if (IsA(subpath, ProjectionPath))
			{
				below = (ProjectionPath *) subpath;
				subpath = below->subpath;
			}
			if (!IsA(subpath, SortPath))
				continue;
			sort = make_sort_path(root, (SortPath *) subpath);
			if (sort == NULL || !sort->path.parallel_safe)
				continue;
			/* Parallel-aware for the counters the participants share. */
			sort->path.parallel_aware = true;
			if (below != NULL)
				below->subpath = &sort->path;
			else
				gather->subpath = &sort->path;
			continue;
		}
		if (!IsA(path, SortPath))
			continue;
		sort = make_sort_path(root, (SortPath *) path);
		if (sort == NULL)
			continue;
		/*
		 * Replace the path in place rather than through add_path: nothing is
		 * freed, and the sort's input lives on in the input relation.
		 */
		if (projection != NULL)
			projection->subpath = &sort->path;
		else
			lfirst(lc) = sort;
	}

	/*
	 * The node's sort of the cheapest partial path under TessGatherMerge,
	 * at the batch gather's cost of a row: the core's Gather Merge of the
	 * same sort, costed at its own, may have lost to a serial sort already.
	 */
	if (output_rel->consider_parallel && root->sort_pathkeys != NIL &&
		input_rel->partial_pathlist != NIL && output_rel->pathlist != NIL)
	{
		Path	   *input = linitial(input_rel->partial_pathlist);
		CustomPath *sort;
		Path	   *path;

		/* make_sort_path reads a core scan through the node's batch input. */
		if (pathkeys_contained_in(root->sort_pathkeys, input->pathkeys))
			return;
		sort = make_sort_path(root, create_sort_path(root, output_rel, input,
													 root->sort_pathkeys,
													 root->limit_tuples));
		if (sort == NULL || !sort->path.parallel_safe)
			return;
		sort->path.parallel_aware = true;
		/* Every path of the ordered relation emits its target; the relation keeps none. */
		path = tess_gather_merge_path(root, output_rel, &sort->path,
									  ((Path *) linitial(output_rel->pathlist))->pathtarget);
		if (path != NULL)
			add_path(output_rel, path);
	}
}

/*
 * The output columns are the plan's targets, each a column of the child's
 * target list; the keys are targets too, found by their expressions. The
 * keys' expressions travel in custom_exprs, for EXPLAIN.
 */
static Plan *
sort_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
		  List *tlist, List *clauses, List *custom_plans)
{
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPlanChild child = TESS_STRUCT_INITIALIZER(TessPlanChild);
	List	   *data;
	List	   *columns = NIL;
	List	   *keys = NIL;
	List	   *key_exprs = NIL;
	TessPlanWriter *writer;

	tess_path_get_info(best_path, &info);
	if (!tess_plan_child(best_path, custom_plans, 0, &child))
		elog(ERROR, "TessSort expected a batch child");
	/* Over a set operation's rows: its columns are the child's targets. */
	tlist = (List *) tess_plan_setop_columns((Node *) tlist, child.plan);
	info.expressions = (List *) tess_plan_setop_columns((Node *) info.expressions, child.plan);
	data = (List *) info.node_data;
	foreach_node(TargetEntry, entry, tlist)
	{
		TargetEntry *found = tlist_member(entry->expr, child.plan->targetlist);
		int			column = found == NULL ? -1 :
			tess_layout_column(&child.layout, found->resno - 1);

		if (column < 0)
			elog(ERROR, "TessSort target is missing from its child");
		columns = lappend_int(columns, column);
	}
	foreach_ptr(Expr, expr, info.expressions)
	{
		TargetEntry *found = tlist_member(expr, tlist);

		if (found == NULL)
			elog(ERROR, "TessSort key is missing from its targets");
		keys = lappend_int(keys, found->resno - 1);
		key_exprs = lappend(key_exprs, copyObject(expr));
	}
	writer = tess_plan_writer_create(TESS_SORT_DATA, TESS_SORT_DATA_VERSION);
	tess_plan_write_int_list(writer, "columns", columns);
	tess_plan_write_int_list(writer, "keys", keys);
	tess_plan_write_int_list(writer, "kinds", linitial(data));
	tess_plan_write_int_list(writer, "flags", lsecond(data));
	tess_plan_write_int_list(writer, "sortops", lthird(data));
	tess_plan_write_int_list(writer, "collations", lfourth(data));
	config.methods = &tess_sort_scan_methods;
	config.layout_policy = TESS_LAYOUT_DENSE;
	config.expressions = key_exprs;
	config.scanrelid = 0;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

/* The rows' by-reference values are copies: gathered, they stay valid. */
static void
sort_get_column(TessBatch *batch, int column, const TessRowMask *rows,
				TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessSortState *state = (TessSortState *) batch->private_data;
	TessRowMask window = {batch->rows.nrows, state->window_bits};

	if (column < 0 || column >= state->ncolumns)
		elog(ERROR, "TessSort has no column %d", column);
	/* The whole batch at once: at most SORT_ROWS rows. */
	if (!state->gathered[column])
	{
		tess_rows_gather(state->rows, column, &state->refs[state->start],
						 &window, state->values[column], state->isnull[column]);
		state->gathered[column] = true;
	}
	result->values = state->values[column];
	result->isnull = state->isnull[column];
	result->nrows = batch->rows.nrows;
}

static const TessBatchOps sort_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = sort_get_column,
};

/*
 * The order of a type's abbreviated keys, from sort support prepared with
 * abbreviate: a comparison of unsigned or signed integers, or numeric's,
 * which is a signed integer's reversed; NONE without one the node takes.
 */
static SortAbbrev
abbrev_order_of(SortSupport abbrev, Oid type)
{
	if (abbrev->abbrev_converter == NULL)
		return SORT_ABBREV_NONE;
	if (abbrev->comparator == ssup_datum_uint64_cmp)
		return SORT_ABBREV_UNSIGNED;
	if (abbrev->comparator == ssup_datum_int64_cmp)
		return SORT_ABBREV_SIGNED;
	if (abbrev->comparator == ssup_datum_uint32_cmp)
		return SORT_ABBREV_UINT32;
	if (abbrev->comparator == ssup_datum_int32_cmp)
		return SORT_ABBREV_INT32;
	if (type == NUMERICOID &&
		abbrev->comparator(Int64GetDatum(0), Int64GetDatum(1), abbrev) > 0 &&
		abbrev->comparator(Int64GetDatum(-1), Int64GetDatum(0), abbrev) > 0)
		return SORT_ABBREV_REVERSED;
	return SORT_ABBREV_NONE;
}

/* Sort support for a key's comparison: its ordering operator, collation and place of NULLs. */
void
tess_sort_support(SortSupport ssup, Oid sortop, Oid collation, bool nulls_first)
{
	memset(ssup, 0, sizeof(SortSupportData));
	ssup->ssup_cxt = CurrentMemoryContext;
	ssup->ssup_collation = collation;
	ssup->ssup_nulls_first = nulls_first;
	ssup->abbreviate = false;
	PrepareSortSupportFromOrderingOp(sortop, ssup);
}

/* A key's abbreviated keys, when its type has ones the node takes. */
void
tess_sort_abbrev_init(TessSortAbbrev *abbrev, Oid sortop, Oid collation,
					  bool nulls_first, Oid type)
{
	SortSupport ssup = &abbrev->ssup;

	memset(ssup, 0, sizeof(SortSupportData));
	ssup->ssup_cxt = CurrentMemoryContext;
	ssup->ssup_collation = collation;
	ssup->ssup_nulls_first = nulls_first;
	ssup->abbreviate = true;
	PrepareSortSupportFromOrderingOp(sortop, ssup);
	abbrev->order = abbrev_order_of(ssup, type);
}

bool
tess_sort_abbreviates(const TessSortAbbrev *abbrev)
{
	return abbrev->order != SORT_ABBREV_NONE;
}

/*
 * The word of a value that is not NULL: its abbreviated key as a signed
 * integer in its order, or 0 without one. The converter may allocate in
 * the current context.
 */
int64
tess_sort_abbrev_word(TessSortAbbrev *abbrev, Datum value)
{
	Datum		abbreviated;

	if (abbrev->order == SORT_ABBREV_NONE)
		return 0;
	abbreviated = abbrev->ssup.abbrev_converter(value, &abbrev->ssup);
	switch ((SortAbbrev) abbrev->order)
	{
		case SORT_ABBREV_UNSIGNED:
			return (int64) (DatumGetUInt64(abbreviated) ^ (UINT64CONST(1) << 63));
		case SORT_ABBREV_SIGNED:
			return DatumGetInt64(abbreviated);
		case SORT_ABBREV_REVERSED:
			return ~DatumGetInt64(abbreviated);
		case SORT_ABBREV_UINT32:
			return (int64) DatumGetUInt32(abbreviated);
		case SORT_ABBREV_INT32:
			return (int64) DatumGetInt32(abbreviated);
		case SORT_ABBREV_NONE:
			break;
	}
	return 0;
}

/*
 * Other types: the comparison of every key from the first generic one on,
 * by its ordering operator, collation and place of NULLs, and the
 * abbreviated key of the first, when the type has one whose order the
 * node can make a word's: a comparison of unsigned or signed integers, or
 * numeric's, which is a signed integer's reversed.
 */
static void
generic_begin(TessSortState *state, TupleDesc desc, List *sortops, List *collations)
{
	int			first = state->generic;
	Oid			type = TupleDescAttr(desc, state->key_columns[first])->atttypid;

	state->ssup = palloc0_array(SortSupportData, state->nkeys);
	for (int key = first; key < state->nkeys; key++)
	{
		Oid			sortop = (Oid) list_nth_int(sortops, key);

		if (!OidIsValid(sortop))
			elog(ERROR, "TessSort received a foreign plan");
		tess_sort_support(&state->ssup[key], sortop, (Oid) list_nth_int(collations, key),
						  (state->keys[key].flags & TESS_SORT_NULLS_FIRST) != 0);
	}
	tess_sort_abbrev_init(&state->abbrev, (Oid) list_nth_int(sortops, first),
						  state->ssup[first].ssup_collation,
						  state->ssup[first].ssup_nulls_first, type);
	state->abbrev_context = AllocSetContextCreate(CurrentMemoryContext,
												  "TessSort abbreviated keys",
												  ALLOCSET_DEFAULT_SIZES);
}

/*
 * The first generic key's words of the selected rows of its column: its
 * abbreviated keys as signed integers in their order, or 0 without one;
 * NULL kept.
 */
static void
abbreviate_column(TessSortState *state, const TessDatumColumn *column,
				  const TessRowMask *rows)
{
	MemoryContext old;
	int			row = -1;

	if (column->nrows > state->abbrev_capacity)
	{
		MemoryContext context = state->css.ss.ps.state->es_query_cxt;

		if (state->abbrev_values != NULL)
		{
			pfree(state->abbrev_values);
			pfree(state->abbrev_isnull);
		}
		state->abbrev_capacity = Max(column->nrows, SORT_ROWS);
		state->abbrev_values = MemoryContextAllocZero(context,
													  sizeof(Datum) * state->abbrev_capacity);
		state->abbrev_isnull = MemoryContextAllocZero(context,
													  sizeof(bool) * state->abbrev_capacity);
	}
	MemoryContextReset(state->abbrev_context);
	old = MemoryContextSwitchTo(state->abbrev_context);
	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		state->abbrev_isnull[row] = column->isnull[row];
		state->abbrev_values[row] = Int64GetDatum(column->isnull[row] ? 0 :
												  tess_sort_abbrev_word(&state->abbrev,
																		column->values[row]));
	}
	MemoryContextSwitchTo(old);
	state->abbrev_column = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
	state->abbrev_column.values = state->abbrev_values;
	state->abbrev_column.isnull = state->abbrev_isnull;
	state->abbrev_column.nrows = column->nrows;
}

/*
 * A row of a group: the first generic key's value in place, as the core's
 * SortTuple keeps it, the other keys' values at its row of tie_values.
 */
typedef struct TieRow
{
	Datum		value;
	uint32		ref;
	/* The row in the group, and the value's NULL flag in the top bit. */
	uint32		row;
} TieRow;

#define TIE_NULL ((uint32) 1 << 31)

/* The comparison of two rows of a group by the keys from the first generic one on. */
static inline int
compare_ties(const TieRow *left, const TieRow *right, TessSortState *state)
{
	uint64		n = state->tie_capacity;
	int			result = ApplySortComparator(left->value, (left->row & TIE_NULL) != 0,
											 right->value, (right->row & TIE_NULL) != 0,
											 &state->ssup[state->generic]);

	for (int key = state->generic + 1; result == 0 && key < state->nkeys; key++)
	{
		uint64		base = (uint64) (key - state->generic - 1) * n;

		uint32		x = left->row & ~TIE_NULL;
		uint32		y = right->row & ~TIE_NULL;

		result = ApplySortComparator(state->tie_values[base + x], state->tie_isnull[base + x],
									 state->tie_values[base + y], state->tie_isnull[base + y],
									 &state->ssup[key]);
	}
	return result;
}

/*
 * The core's sort template, as its own sorts use it: the rows swapped as
 * TieRows and compared inline, not through qsort_arg's bytes and pointer.
 */
#define ST_SORT sort_tie_rows
#define ST_ELEMENT_TYPE TieRow
#define ST_COMPARE(a, b, state) compare_ties(a, b, state)
#define ST_COMPARE_ARG_TYPE TessSortState
#define ST_CHECK_FOR_INTERRUPTS
#define ST_SCOPE static
#define ST_DEFINE
#include "lib/sort_template.h"

/* Order the n records refs of one group of equal words by the comparisons. */
static void
sort_group(TessSortState *state, uint32 *refs, uint64 n)
{
	int			nafter = state->nkeys - state->generic - 1;
	TieRow	   *ties;

	if (n >= TIE_NULL)
		elog(ERROR, "TessSort cannot order " UINT64_FORMAT " rows of equal keys", n);
	if (n > state->tie_capacity)
	{
		MemoryContext context = state->css.ss.ps.state->es_query_cxt;
		uint64		capacity = Max(n, (uint64) 1024);

		if (state->tie_rows != NULL)
			pfree(state->tie_rows);
		if (state->tie_values != NULL)
		{
			pfree(state->tie_values);
			pfree(state->tie_isnull);
		}
		state->tie_rows = MemoryContextAllocExtended(context, mul_size(sizeof(TieRow), capacity),
													 MCXT_ALLOC_HUGE);
		/* Keys after the first generic one only. */
		if (nafter > 0)
		{
			state->tie_values = MemoryContextAllocExtended(context,
														   mul_size(sizeof(Datum) * nafter, capacity),
														   MCXT_ALLOC_HUGE);
			state->tie_isnull = MemoryContextAllocExtended(context,
														   mul_size(sizeof(bool) * nafter, capacity),
														   MCXT_ALLOC_HUGE);
		}
		state->tie_capacity = capacity;
	}
	ties = state->tie_rows;
	for (uint64 first = 0; first < n; first += SORT_ROWS)
	{
		int			count = (int) Min((uint64) SORT_ROWS, n - first);
		uint64		bits[1] = {count == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << count) - 1};
		TessRowMask mask = {count, bits};
		Datum		values[SORT_ROWS];
		bool		isnull[SORT_ROWS];

		tess_rows_gather(state->rows, state->key_columns[state->generic], &refs[first], &mask,
						 values, isnull);
		for (int row = 0; row < count; row++)
		{
			ties[first + row].value = values[row];
			ties[first + row].ref = refs[first + row];
			ties[first + row].row = (uint32) (first + row) | (isnull[row] ? TIE_NULL : 0);
		}
		for (int key = state->generic + 1; key < state->nkeys; key++)
		{
			uint64		base = (uint64) (key - state->generic - 1) * state->tie_capacity + first;

			tess_rows_gather(state->rows, state->key_columns[key], &refs[first], &mask,
							 &state->tie_values[base], &state->tie_isnull[base]);
		}
	}
	sort_tie_rows(ties, n, state);
	for (uint64 row = 0; row < n; row++)
		refs[row] = ties[row].ref;
}

/* The rows of groups, freed once the rows are sorted. */
static void
free_ties(TessSortState *state)
{
	if (state->tie_rows != NULL)
		pfree(state->tie_rows);
	if (state->tie_values != NULL)
	{
		pfree(state->tie_values);
		pfree(state->tie_isnull);
	}
	state->tie_rows = NULL;
	state->tie_values = NULL;
	state->tie_isnull = NULL;
	state->tie_capacity = 0;
}

/*
 * After the kernels sorted count items of words words by the keys up to
 * the first generic one: the records of each run of items whose keys'
 * words are equal (all but the reference's 32 bits) ordered by the
 * comparisons of the keys from that one on. The runs' starts are marked
 * first, so that a caller done with the items frees them (*items NULL)
 * before the rows of the runs take their memory.
 */
static void
sort_ties(TessSortState *state, uint64 **items, int words, uint32 *refs, uint64 count,
		  bool free_items)
{
	uint64	   *starts = palloc0_array(uint64, (count + 64) / 64);
	const uint64 *item = *items;
	uint64		first = 0;

	for (uint64 place = 1; place < count; place++)
	{
		const uint64 *next = item + words;

		if ((words > 1 && memcmp(item, next, sizeof(uint64) * (words - 1)) != 0) ||
			((item[words - 1] ^ next[words - 1]) >> 32) != 0)
			starts[place / 64] |= UINT64CONST(1) << (place % 64);
		item = next;
	}
	starts[count / 64] |= UINT64CONST(1) << (count % 64);
	if (free_items)
	{
		pfree(*items);
		*items = NULL;
	}
	while (first < count)
	{
		uint64		end = first + 1;

		while (((starts[end / 64] >> (end % 64)) & 1) == 0)
			end++;
		if (end - first > 1)
			sort_group(state, &refs[first], end - first);
		first = end;
		CHECK_FOR_INTERRUPTS();
	}
	pfree(starts);
}

static void
sort_begin(CustomScanState *css, EState *estate, int eflags)
{
	TessSortState *state = (TessSortState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
	TessRowsConfig rows = TESS_STRUCT_INITIALIZER(TessRowsConfig);
	TupleTableSlot *result = css->ss.ps.ps_ResultTupleSlot;
	TupleDesc	desc = result->tts_tupleDescriptor;
	TessTableKeyKind kinds[TESS_TABLE_MAX_KEYS];
	Bitmapset  *projection = NULL;
	TessPlanReader *reader;
	List	   *columns;
	List	   *keys;
	List	   *key_kinds;
	List	   *key_flags;
	List	   *key_sortops;
	List	   *key_collations;
	int16	   *typlens;
	bool	   *typbyvals;

	/* The planner puts Material above a batch subtree for mark/restore. */
	if (eflags & EXEC_FLAG_MARK)
		elog(ERROR, "TessSort does not support mark/restore");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_sort_node || info.nchildren != 1 ||
		info.child_names[0] == NULL)
		elog(ERROR, "TessSort received a foreign plan");
	reader = tess_plan_reader_create((List *) info.node_data, TESS_SORT_DATA,
									 TESS_SORT_DATA_VERSION);
	columns = tess_plan_read_int_list(reader, "columns");
	keys = tess_plan_read_int_list(reader, "keys");
	key_kinds = tess_plan_read_int_list(reader, "kinds");
	key_flags = tess_plan_read_int_list(reader, "flags");
	key_sortops = tess_plan_read_int_list(reader, "sortops");
	key_collations = tess_plan_read_int_list(reader, "collations");
	tess_plan_reader_finish(reader);
	state->ncolumns = list_length(columns);
	state->nkeys = list_length(keys);
	if (state->ncolumns != desc->natts || state->ncolumns == 0 ||
		state->ncolumns > TESS_ROWS_MAX_COLUMNS || state->nkeys == 0 ||
		state->nkeys > TESS_TABLE_MAX_KEYS ||
		list_length(key_kinds) != state->nkeys ||
		list_length(key_flags) != state->nkeys ||
		list_length(key_sortops) != state->nkeys ||
		list_length(key_collations) != state->nkeys)
		elog(ERROR, "TessSort received a foreign plan");
	state->kernels = tess_runtime_kernels();
	if (state->kernels == NULL ||
		!TESS_ABI_HAS_FIELD(state->kernels, TessKernelOps, sort_top_push))
		elog(ERROR, "TessSort needs the kernels module");

	/* The child is read forward once, as the core's sort reads its own. */
	state->child = ExecInitNode(linitial(cscan->custom_plans), estate,
								eflags & ~(EXEC_FLAG_REWIND | EXEC_FLAG_BACKWARD |
										   EXEC_FLAG_MARK));
	css->custom_ps = list_make1(state->child);
	state->input = tess_input_create(estate->es_query_cxt, state->child);
	state->child_columns = palloc_array(int, state->ncolumns);
	typlens = palloc_array(int16, state->ncolumns);
	typbyvals = palloc_array(bool, state->ncolumns);
	foreach_int(column, columns)
	{
		int			index = foreach_current_index(column);
		Form_pg_attribute attribute = TupleDescAttr(desc, index);

		state->child_columns[index] = column;
		projection = bms_add_member(projection, column);
		typlens[index] = attribute->attlen;
		typbyvals[index] = attribute->attbyval;
	}
	state->key_columns = palloc_array(int, state->nkeys);
	state->generic = -1;
	foreach_int(key, keys)
	{
		int			index = foreach_current_index(key);
		int			kind = list_nth_int(key_kinds, index);

		if (key < 0 || key >= state->ncolumns)
			elog(ERROR, "TessSort received a foreign plan");
		state->key_columns[index] = key;
		/* A generic key's word is its abbreviated key, an int8. */
		if (kind == TESS_SORT_KIND_GENERIC && state->generic < 0)
			state->generic = index;
		kinds[index] = kind == TESS_SORT_KIND_GENERIC ? TESS_TABLE_KEY_INT8 :
			(TessTableKeyKind) kind;
		state->keys[index].kind = kinds[index];
		state->keys[index].flags = (uint32) list_nth_int(key_flags, index);
	}
	state->nkernel = state->generic < 0 ? state->nkeys : state->generic + 1;
	if (state->generic >= 0)
		generic_begin(state, desc, key_sortops, key_collations);
	/* Whole batches: every column of the rows kept. */
	request.projection_columns = projection;
	request.output_mode = TESS_OUTPUT_BATCH;
	tess_input_set_request(state->input, &request);

	memcpy(state->kinds, kinds, sizeof(TessTableKeyKind) * state->nkernel);
	rows.parent_context = estate->es_query_cxt;
	rows.kernels = state->kernels;
	rows.nkeys = state->nkernel;
	rows.kinds = state->kinds;
	rows.ncolumns = state->ncolumns;
	rows.typlens = typlens;
	rows.typbyvals = typbyvals;
	/* Chunks of an eighth of work_mem, so that runs fill it evenly. */
	rows.chunk_len = Max((Size) work_mem * 1024 / 8, (Size) 8192);
	state->rows_config = rows;
	state->rows = tess_rows_create(&rows);
	state->bound = -1;
	state->used_bound = -1;
	state->columns = palloc0_array(TessDatumColumn, state->ncolumns);
	state->values = palloc_array(Datum *, state->ncolumns);
	state->isnull = palloc_array(bool *, state->ncolumns);
	state->gathered = palloc0_array(bool, state->ncolumns);
	for (int column = 0; column < state->ncolumns; column++)
	{
		state->values[column] = palloc0_array(Datum, SORT_ROWS);
		state->isnull[column] = palloc0_array(bool, SORT_ROWS);
	}
	state->batch.abi_version = TESS_BATCH_ABI_VERSION;
	state->batch.struct_size = sizeof(TessBatch);
	state->batch.table_oid = InvalidOid;
	state->batch.ops = &sort_batch_ops;
	state->batch.private_data = state;
	state->current = -1;
	state->eflags = eflags;
	plan_external(state);
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   result, &info.layout);
}

/* The records, what their items and references took at the sort, and the peak. */
static void
note_memory(TessSortState *state, Size extra)
{
	Size		memory = add_size(tess_rows_memory(state->rows), extra);

	state->counters.memory = Max(state->counters.memory, memory);
}

/* Output column `column` of the batch, for its selected rows. */
static void
batch_column(TessSortState *state, TessBatch *batch, int column)
{
	TessDatumColumn *values = &state->columns[column];

	*values = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
	batch->ops->get_datum_column(batch, state->child_columns[column],
								 &batch->rows, TESS_COLUMN_FOR_PROJECTION,
								 values);
	if (values->values == NULL || values->isnull == NULL ||
		values->nrows != batch->rows.nrows)
		elog(ERROR, "TessSort child returned an invalid column");
}

/*
 * The key columns of the batch the kernels order, as the table takes
 * them: a generic key's, its abbreviated keys.
 */
static void
batch_keys(TessSortState *state, TessBatch *batch)
{
	for (int key = 0; key < state->nkernel; key++)
	{
		batch_column(state, batch, state->key_columns[key]);
		state->table_keys[key].kind = state->keys[key].kind;
		state->table_keys[key].column = &state->columns[state->key_columns[key]];
		state->table_keys[key].prepared = NULL;
		if (key == state->generic)
		{
			abbreviate_column(state, state->table_keys[key].column, &batch->rows);
			state->table_keys[key].column = &state->abbrev_column;
		}
	}
}

/* Whether an output column is a key's the kernels order, fetched with the keys. */
static bool
is_key_column(TessSortState *state, int column)
{
	for (int key = 0; key < state->nkernel; key++)
		if (state->key_columns[key] == column)
			return true;
	return false;
}

/* The selected rows of the batch into records, their references into batch_refs. */
static void
append_rows(TessSortState *state, TessBatch *batch)
{
	int			nrows = batch->rows.nrows;

	if (nrows > state->capacity)
	{
		state->capacity = Max(nrows, SORT_ROWS);
		state->batch_refs = state->batch_refs == NULL ?
			MemoryContextAlloc(state->css.ss.ps.state->es_query_cxt,
							   sizeof(uint32) * state->capacity) :
			repalloc(state->batch_refs, sizeof(uint32) * state->capacity);
	}
	tess_rows_append(state->rows, state->table_keys, state->columns,
					 &batch->rows, state->batch_refs);
}

/* The rows of one batch of the child into records. */
static void
append_batch(TessSortState *state, TessBatch *batch)
{
	batch_keys(state, batch);
	for (int column = 0; column < state->ncolumns; column++)
		if (!is_key_column(state, column))
			batch_column(state, batch, column);
	for (int key = 0; key < state->nkernel; key++)
		if (!state->key_nulls[key])
			state->key_nulls[key] =
				tess_rows_selected_null(&batch->rows,
										state->table_keys[key].column->isnull);
	append_rows(state, batch);
}

static void
check_kernel(TessStatusCode code, TessStatus *status)
{
	if (code != TESS_OK)
		tess_status_report(status);
}

/* The reference of a heap item: the low 32 bits of its last word. */
static uint32
item_ref(TessSortState *state, uint64 item)
{
	return (uint32) state->heap[item * state->words + state->words - 1];
}

/* The values of item slot's keys from the first generic one on. */
static inline Datum *
top_slot_values(TessSortState *state, uint64 slot)
{
	return &state->top_values[slot * (state->nkeys - state->generic)];
}

static inline bool *
top_slot_isnull(TessSortState *state, uint64 slot)
{
	return &state->top_isnull[slot * (state->nkeys - state->generic)];
}

/*
 * The values of the generic keys of records refs[row] for the rows of
 * mask, into the slots from `first` on, in the order of the rows.
 */
static void
top_gather(TessSortState *state, TessRows *rows, const uint32 *refs,
		   const TessRowMask *mask, uint64 first)
{
	int			ngeneric = state->nkeys - state->generic;

	for (int key = state->generic; key < state->nkeys; key++)
	{
		int			row = -1;
		uint64		slot = first;

		tess_rows_gather(rows, state->key_columns[key], refs, mask,
						 state->top_gathered, state->top_gathered_null);
		while ((row = tess_row_mask_next(mask, row)) >= 0)
		{
			state->top_values[slot * ngeneric + key - state->generic] = state->top_gathered[row];
			state->top_isnull[slot * ngeneric + key - state->generic] = state->top_gathered_null[row];
			slot++;
		}
	}
}

/*
 * The order of two slots of a generic key's heap: their words, the
 * reference's bits left out, then the comparisons.
 */
static int
compare_slots(TessSortState *state, uint64 a, uint64 b)
{
	const uint64 *x = &state->heap[a * state->words];
	const uint64 *y = &state->heap[b * state->words];
	int			last = state->words - 1;
	Datum	   *xv = top_slot_values(state, a);
	Datum	   *yv = top_slot_values(state, b);
	bool	   *xn = top_slot_isnull(state, a);
	bool	   *yn = top_slot_isnull(state, b);

	for (int word = 0; word < last; word++)
		if (x[word] != y[word])
			return x[word] < y[word] ? -1 : 1;
	if ((x[last] >> 32) != (y[last] >> 32))
		return (x[last] >> 32) < (y[last] >> 32) ? -1 : 1;
	for (int key = state->generic; key < state->nkeys; key++)
	{
		int			at = key - state->generic;
		int			result = ApplySortComparator(xv[at], xn[at], yv[at], yn[at],
												 &state->ssup[key]);

		if (result != 0)
			return result;
	}
	return 0;
}

/* Copy slot `from` of the heap to slot `to`. */
static void
copy_slot(TessSortState *state, uint64 to, uint64 from)
{
	int			ngeneric = state->nkeys - state->generic;

	memcpy(&state->heap[to * state->words], &state->heap[from * state->words],
		   sizeof(uint64) * state->words);
	memcpy(top_slot_values(state, to), top_slot_values(state, from), sizeof(Datum) * ngeneric);
	memcpy(top_slot_isnull(state, to), top_slot_isnull(state, from), sizeof(bool) * ngeneric);
}

/*
 * The item in the spare slot (heap_capacity) into the max-heap: added
 * while there is room, else in place of the worst when it is better.
 */
static void
top_push_slot(TessSortState *state)
{
	uint64		spare = state->heap_capacity;
	uint64		place;

	if (state->heap_len < state->heap_capacity)
	{
		place = state->heap_len++;
		while (place > 0)
		{
			uint64		parent = (place - 1) / 2;

			if (compare_slots(state, spare, parent) <= 0)
				break;
			copy_slot(state, place, parent);
			place = parent;
		}
		copy_slot(state, place, spare);
		return;
	}
	if (compare_slots(state, spare, 0) >= 0)
		return;
	place = 0;
	for (;;)
	{
		uint64		child = 2 * place + 1;

		if (child >= state->heap_len)
			break;
		if (child + 1 < state->heap_len && compare_slots(state, child + 1, child) > 0)
			child++;
		if (compare_slots(state, child, spare) <= 0)
			break;
		copy_slot(state, place, child);
		place = child;
	}
	copy_slot(state, place, spare);
}

/*
 * Top-N of a generic key: a batch's key columns and its items' lanes
 * first; once the heap is full, the batch keeps only the rows that order
 * before the worst kept by their lanes, or by the comparisons when the
 * lanes are equal; those have their other columns read, are appended and
 * go into the heap one by one.
 */
static void
top_batch_generic(TessSortState *state, TessBatch *batch)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	int			nrows = batch->rows.nrows;
	int			ngeneric = state->nkeys - state->generic;
	int			words = state->words;
	uint64		spare = state->heap_capacity;
	int			count;
	int			row = -1;
	int			lane = 0;

	if (nrows > state->top_batch_capacity)
	{
		MemoryContext context = state->css.ss.ps.state->es_query_cxt;

		state->top_batch_capacity = Max(nrows, SORT_ROWS);
		for (int word = 0; word < words; word++)
		{
			if (state->top_lanes[word] != NULL)
				pfree(state->top_lanes[word]);
			state->top_lanes[word] = MemoryContextAlloc(context,
														sizeof(uint64) * state->top_batch_capacity);
		}
		if (state->top_lane_of != NULL)
		{
			pfree(state->top_lane_of);
			pfree(state->top_gathered);
			pfree(state->top_gathered_null);
		}
		state->top_lane_of = MemoryContextAlloc(context, sizeof(int) * state->top_batch_capacity);
		state->top_gathered = MemoryContextAllocZero(context,
													 sizeof(Datum) * state->top_batch_capacity);
		state->top_gathered_null = MemoryContextAllocZero(context,
														  sizeof(bool) * state->top_batch_capacity);
	}
	batch_keys(state, batch);
	for (int key = state->generic + 1; key < state->nkeys; key++)
		batch_column(state, batch, state->key_columns[key]);
	check_kernel(state->kernels->sort_key_lanes(state->nkernel, state->top_keys,
												state->table_keys, &batch->rows, words,
												state->top_lanes, state->top_batch_capacity,
												&count, &status),
				 &status);
	while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
		state->top_lane_of[row] = lane++;
	if (state->heap_len == state->heap_capacity)
	{
		/* The rows that do not beat the worst kept leave the mask. */
		row = -1;
		while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
		{
			int			at = state->top_lane_of[row];
			int			result = 0;

			for (int word = 0; word < words; word++)
			{
				uint64		x = state->top_lanes[word][at];
				uint64		y = state->heap[word];

				/* The worst's reference is no key. */
				if (word == words - 1)
				{
					x >>= 32;
					y >>= 32;
				}
				if (x != y)
				{
					result = x < y ? -1 : 1;
					break;
				}
			}
			for (int key = state->generic; result == 0 && key < state->nkeys; key++)
			{
				const TessDatumColumn *column = &state->columns[state->key_columns[key]];

				result = ApplySortComparator(column->values[row], column->isnull[row],
											 top_slot_values(state, 0)[key - state->generic],
											 top_slot_isnull(state, 0)[key - state->generic],
											 &state->ssup[key]);
			}
			if (result >= 0)
				batch->rows.bits[row / 64] &= ~(UINT64CONST(1) << (row % 64));
		}
		if (tess_row_mask_count(&batch->rows) == 0)
			return;
	}
	for (int column = 0; column < state->ncolumns; column++)
		if (!is_key_column(state, column))
			batch_column(state, batch, column);
	append_rows(state, batch);
	/* The kept rows' values from their records, which outlive the batch. */
	{
		Datum	   *values = palloc_array(Datum, (Size) nrows * ngeneric);
		bool	   *isnull = palloc_array(bool, (Size) nrows * ngeneric);

		for (int key = state->generic; key < state->nkeys; key++)
			tess_rows_gather(state->rows, state->key_columns[key], state->batch_refs,
							 &batch->rows, &values[(key - state->generic) * nrows],
							 &isnull[(key - state->generic) * nrows]);
		row = -1;
		while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
		{
			int			at = state->top_lane_of[row];
			uint64	   *item = &state->heap[spare * words];

			for (int word = 0; word < words; word++)
				item[word] = state->top_lanes[word][at];
			item[words - 1] |= state->batch_refs[row];
			for (int key = 0; key < ngeneric; key++)
			{
				top_slot_values(state, spare)[key] = values[key * nrows + row];
				top_slot_isnull(state, spare)[key] = isnull[key * nrows + row];
			}
			top_push_slot(state);
		}
		pfree(values);
		pfree(isnull);
	}
	note_memory(state, (state->heap_capacity + 1) *
				(words * sizeof(uint64) + ngeneric * (sizeof(Datum) + sizeof(bool))));
	if (tess_rows_count(state->rows) > Max(4 * state->heap_capacity, 65536))
		compact_rows(state);
}

/*
 * The records outnumber what the heap needs: make the rows anew from the
 * heap's records and the heap from them, so that memory stays bounded when
 * every row beats the ones kept, as keys in the reverse of the order do.
 */
static void
compact_rows(TessSortState *state)
{
	TessRows   *rows = tess_rows_create(&state->rows_config);
	uint64		len = state->heap_len;
	uint32	   *kept = palloc_array(uint32, Max(len, 1));
	uint32		new_refs[SORT_ROWS];
	int			ncolumns = Max(state->ncolumns, 1);
	Datum	   *values = palloc_array(Datum, ncolumns * SORT_ROWS);
	bool	   *nulls = palloc_array(bool, ncolumns * SORT_ROWS);
	TessDatumColumn *columns = palloc_array(TessDatumColumn, ncolumns);
	TessTableKey keys[TESS_TABLE_MAX_KEYS];

	/* The heap is made anew below: its records are taken first. */
	for (uint64 item = 0; item < len; item++)
		kept[item] = item_ref(state, item);
	state->heap_len = 0;
	for (uint64 first = 0; first < len; first += SORT_ROWS)
	{
		int			n = (int) Min((uint64) SORT_ROWS, len - first);
		uint64		bits[1] = {n == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << n) - 1};
		TessRowMask mask = {n, bits};

		for (int column = 0; column < state->ncolumns; column++)
		{
			tess_rows_gather(state->rows, column, &kept[first], &mask,
							 &values[column * SORT_ROWS], &nulls[column * SORT_ROWS]);
			columns[column] = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
			columns[column].values = &values[column * SORT_ROWS];
			columns[column].isnull = &nulls[column * SORT_ROWS];
			columns[column].nrows = n;
		}
		for (int key = 0; key < state->nkernel; key++)
		{
			keys[key].kind = state->keys[key].kind;
			keys[key].column = &columns[state->key_columns[key]];
			keys[key].prepared = NULL;
			if (key == state->generic)
			{
				abbreviate_column(state, keys[key].column, &mask);
				keys[key].column = &state->abbrev_column;
			}
		}
		tess_rows_append(rows, keys, columns, &mask, new_refs);
		if (state->generic >= 0)
		{
			/* The items stay where they are: their references and values change. */
			for (int row = 0; row < n; row++)
			{
				uint64	   *last = &state->heap[(first + row) * state->words + state->words - 1];

				*last = (*last & ~UINT64CONST(0xFFFFFFFF)) | new_refs[row];
			}
			top_gather(state, rows, new_refs, &mask, first);
			continue;
		}
		/* The items of the kept rows go in anew, by their new records. */
		tess_rows_top_push(rows, state->top_keys, new_refs, &mask, state->heap,
						   state->heap_capacity, &state->heap_len);
	}
	if (state->generic >= 0)
		state->heap_len = len;
	pfree(kept);
	pfree(values);
	pfree(nulls);
	pfree(columns);
	tess_rows_free(state->rows);
	state->rows = rows;
	state->compactions++;
}

/*
 * Top-N: a batch's key columns first; once the heap is full, the batch
 * keeps only the rows whose keys beat the worst kept, and only those have
 * their other columns read, are appended and go into the heap.
 */
static void
top_batch(TessSortState *state, TessBatch *batch)
{
	batch_keys(state, batch);
	if (state->heap_len == state->heap_capacity)
	{
		TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
		int			kept;

		check_kernel(state->kernels->sort_top_candidates(state->nkernel,
														 state->top_keys,
														 state->table_keys,
														 &batch->rows,
														 state->heap, &kept,
														 &status),
					 &status);
		if (kept == 0)
			return;
	}
	for (int column = 0; column < state->ncolumns; column++)
		if (!is_key_column(state, column))
			batch_column(state, batch, column);
	append_rows(state, batch);
	tess_rows_top_push(state->rows, state->top_keys, state->batch_refs,
					   &batch->rows, state->heap, state->heap_capacity,
					   &state->heap_len);
	note_memory(state, state->heap_capacity * state->words * sizeof(uint64));
	if (tess_rows_count(state->rows) > Max(4 * state->heap_capacity, 65536))
		compact_rows(state);
}

/*
 * Whether a bound makes a top-N sort: its heap and the rows that may be
 * appended before a rebuild fit work_mem. Every key takes its bit for NULL.
 */
static bool
choose_topn(TessSortState *state)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	double		bytes;

	if (state->bound < 0)
		return false;
	for (int key = 0; key < state->nkernel; key++)
	{
		state->top_keys[key] = state->keys[key];
		state->top_keys[key].flags |= TESS_SORT_NULLABLE;
	}
	check_kernel(state->kernels->sort_item_words(state->nkernel, state->top_keys,
												 &state->words, &status),
				 &status);
	bytes = (double) state->bound * state->words * sizeof(uint64) +
		(double) Max(4 * (double) state->bound, 65536.0) *
		(16.0 + 8.0 * (state->nkernel + (state->ncolumns + 63) / 64 + state->ncolumns));
	/* A generic key's heap: a slot more, and the values of its keys. */
	if (state->generic >= 0)
		bytes += (double) (state->bound + 1) *
			(state->words * sizeof(uint64) +
			 (state->nkeys - state->generic) * (sizeof(Datum) + sizeof(bool)));
	return bytes <= (double) work_mem * 1024.0;
}

/* The words of a run's chunk of columns: NULL bits, the columns, the item's words. */
static int
run_words(TessSortState *state)
{
	return state->ncolumns + state->ext_words;
}

/* The set runs are written into ends its writes; its runs can be read. */
static void
set_finish(TessSortState *state)
{
	if (state->writing == NULL)
		return;
	tess_spill_finish(state->writing->file);
	state->writing->finished = true;
	state->writing = NULL;
}

/* A run of no blocks yet, the next partition of the set being written. */
static SortRun *
run_create(TessSortState *state)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	SortRun    *run = MemoryContextAllocZero(context, sizeof(SortRun));

	if (state->writing != NULL && state->writing->used == SORT_SET_RUNS)
		set_finish(state);
	if (state->writing == NULL)
	{
		TessSpillConfig config = TESS_STRUCT_INITIALIZER(TessSpillConfig);
		RunSet	   *set = MemoryContextAllocZero(context, sizeof(RunSet));

		config.parent_context = context;
		config.kernels = state->kernels;
		config.npartitions = SORT_SET_RUNS;
		config.level = 0;
		config.fingerprint = ((uint64) state->ncolumns << 32) | (uint64) state->ext_words;
		config.max_len = (uint64) MaxAllocHugeSize;
		config.buffer_len = TESS_SPILL_BUFFER_LEN((Size) work_mem * 1024);
		set->file = tess_spill_create(&config);
		state->writing = set;
	}
	run->set = state->writing;
	run->partition = state->writing->used++;
	run->set->alive++;
	if (state->nruns == state->run_slots)
	{
		state->run_slots = Max(state->run_slots * 2, 8);
		state->runs = state->runs == NULL ?
			MemoryContextAlloc(context, sizeof(SortRun *) * state->run_slots) :
			repalloc(state->runs, sizeof(SortRun *) * state->run_slots);
	}
	return run;
}

/* Forget a run; its set goes with its last run. */
static void
run_free(SortRun *run)
{
	RunSet	   *set = run->set;

	tess_spill_drop(set->file, run->partition);
	if (--set->alive == 0 && set->finished)
	{
		tess_spill_free(set->file);
		pfree(set);
	}
	if (run->positions != NULL)
	{
		pfree(run->positions);
		pfree(run->block_rows);
		pfree(run->block_first);
	}
	pfree(run);
}

static void
writer_reset_chunk(TessSortState *state, RunWriter *writer)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	Size		capacity;

	check_kernel(state->kernels->spill_columns_init(writer->chunk, writer->chunk_len,
													run_words(state), &capacity,
													&status),
				 &status);
	writer->capacity = (uint32) capacity;
	writer->rows = 0;
	writer->values_used = 0;
}

static void
writer_start(TessSortState *state, RunWriter *writer, SortRun *run)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;

	writer->run = run;
	writer->null_lanes = tess_spill_columns_null_lanes(run_words(state));
	writer->chunk_len = TESS_SPILL_COLUMNS_HEADER +
		sizeof(uint64) * (Size) state->block_rows * (writer->null_lanes + run_words(state));
	writer->chunk = MemoryContextAllocExtended(context, writer->chunk_len, MCXT_ALLOC_HUGE);
	writer->values_len = state->block_values;
	writer->values = MemoryContextAllocExtended(context, writer->values_len, MCXT_ALLOC_HUGE);
	writer_reset_chunk(state, writer);
}

/* Write the block pair the writer holds: its values, then its rows. */
static void
writer_flush(TessSortState *state, RunWriter *writer)
{
	SortRun    *run = writer->run;
	TessSpillPosition position;

	if (writer->rows == 0)
		return;
	if (run->nblocks == run->slots)
	{
		MemoryContext context = state->css.ss.ps.state->es_query_cxt;

		run->slots = Max(run->slots * 2, 16);
		run->positions = run->positions == NULL ?
			MemoryContextAlloc(context, sizeof(TessSpillPosition) * run->slots) :
			repalloc(run->positions, sizeof(TessSpillPosition) * run->slots);
		run->block_rows = run->block_rows == NULL ?
			MemoryContextAlloc(context, sizeof(uint32) * run->slots) :
			repalloc(run->block_rows, sizeof(uint32) * run->slots);
		run->block_first = run->block_first == NULL ?
			MemoryContextAlloc(context, sizeof(uint64) * run->slots) :
			repalloc(run->block_first, sizeof(uint64) * run->slots);
	}
	tess_spill_columns_set_rows(writer->chunk, writer->rows);
	state->disk_bytes += tess_spill_write(run->set->file, run->partition, TESS_SPILL_VALUES,
										  (uint32) run->nblocks, writer->values,
										  writer->values_used, &position);
	state->disk_bytes += tess_spill_write(run->set->file, run->partition, TESS_SPILL_COLUMNS,
										  (uint32) run->nblocks, writer->chunk,
										  writer->chunk_len, NULL);
	run->positions[run->nblocks] = position;
	run->block_rows[run->nblocks] = writer->rows;
	run->block_first[run->nblocks] = run->rows;
	run->rows += writer->rows;
	run->nblocks++;
	writer_reset_chunk(state, writer);
}

static void
writer_finish(TessSortState *state, RunWriter *writer)
{
	writer_flush(state, writer);
	pfree(writer->chunk);
	pfree(writer->values);
	state->runs[state->nruns++] = writer->run;
	state->runs_written++;
}

/*
 * Append n rows to the run: column c of row r is values[c][r] unless
 * isnull[c][r], and its item's words are at keys[r]. A by-reference value
 * is copied into the block's values; a block fills with rows or values.
 */
static void
writer_add(TessSortState *state, RunWriter *writer, int n, Datum *const *values,
		   bool *const *isnull, const uint64 *const *keys)
{
	const int16 *typlens = state->rows_config.typlens;
	const bool *typbyvals = state->rows_config.typbyvals;
	Size		capacity = writer->capacity;
	bool		byref = false;

	for (int column = 0; column < state->ncolumns; column++)
		byref |= !typbyvals[column];
	/*
	 * By-value columns only: each column's words copied into its lane a
	 * run of rows at a time, the NULL bits set where a column has NULLs;
	 * row by row this was 7 % of an external sort.
	 */
	if (!byref)
	{
		int			row = 0;

		while (row < n)
		{
			int			take;
			uint64	   *nulls;
			uint64	   *words;

			if (writer->rows == writer->capacity)
				writer_flush(state, writer);
			take = Min(n - row, (int) (writer->capacity - writer->rows));
			nulls = tess_spill_columns_lane(writer->chunk, 0) + writer->rows;
			words = nulls + capacity * writer->null_lanes;
			for (int lane = 0; lane < writer->null_lanes; lane++)
				memset(nulls + capacity * lane, 0, sizeof(uint64) * take);
			for (int column = 0; column < state->ncolumns; column++)
			{
				uint64	   *lane = words + capacity * column;
				uint64	   *column_nulls = nulls + capacity * (column / 64);
				const bool *flags = &isnull[column][row];

				memcpy(lane, &values[column][row], sizeof(uint64) * take);
				if (memchr(flags, true, take) == NULL)
					continue;
				for (int at = 0; at < take; at++)
					if (flags[at])
					{
						column_nulls[at] |= UINT64CONST(1) << (column % 64);
						lane[at] = 0;
					}
			}
			for (int word = 0; word < state->ext_words; word++)
			{
				uint64	   *lane = words + capacity * (state->ncolumns + word);

				for (int at = 0; at < take; at++)
					lane[at] = keys[row + at][word];
			}
			writer->rows += take;
			row += take;
		}
		return;
	}
	for (int row = 0; row < n; row++)
	{
		Size		need = 0;
		uint32		place;
		uint64	   *nulls;
		uint64	   *words;

		for (int column = 0; column < state->ncolumns; column++)
			if (!isnull[column][row] && !typbyvals[column])
				need += MAXALIGN(datumGetSize(values[column][row], false, typlens[column]));
		if (writer->rows == writer->capacity ||
			(writer->rows > 0 && writer->values_used + need > writer->values_len))
			writer_flush(state, writer);
		if (writer->values_used + need > writer->values_len)
		{
			writer->values_len = Max(writer->values_len * 2, writer->values_used + need);
			writer->values = repalloc_huge(writer->values, writer->values_len);
		}
		place = writer->rows++;
		nulls = tess_spill_columns_lane(writer->chunk, 0) + place;
		words = nulls + capacity * writer->null_lanes;
		for (int lane = 0; lane < writer->null_lanes; lane++)
			nulls[capacity * lane] = 0;
		for (int column = 0; column < state->ncolumns; column++)
		{
			uint64	   *lane = words + capacity * column;

			if (isnull[column][row])
			{
				nulls[capacity * (column / 64)] |= UINT64CONST(1) << (column % 64);
				lane[0] = 0;
			}
			else if (typbyvals[column])
				lane[0] = (uint64) values[column][row];
			else
			{
				Size		size = datumGetSize(values[column][row], false, typlens[column]);

				memcpy(writer->values + writer->values_used,
					   DatumGetPointer(values[column][row]), size);
				lane[0] = writer->values_used;
				writer->values_used += MAXALIGN(size);
			}
		}
		for (int word = 0; word < state->ext_words; word++)
			words[capacity * (state->ncolumns + word)] = keys[row][word];
	}
}

/*
 * The rows in memory go to a run, sorted, and memory is freed for the
 * next: their items with every key's bit for NULL, the reference in the
 * last word's low bits left out of the run's lanes.
 */
static void
spill_run(TessSortState *state)
{
	uint64		count = tess_rows_count(state->rows);
	uint32	   *refs;
	uint64	   *items;
	int			words;
	RunWriter	writer;
	Datum	  **values = state->values;
	bool	  **nulls = state->isnull;
	const uint64 *keys[SORT_ROWS];
	uint64		copies[SORT_ROWS][TESS_SORT_MAX_ITEM_WORDS];

	if (count == 0)
		return;
	refs = MemoryContextAllocExtended(state->css.ss.ps.state->es_query_cxt,
									  mul_size(sizeof(uint32), count), MCXT_ALLOC_HUGE);
	items = tess_rows_sort_items(state->rows, state->ext_keys, refs, &words);
	if (words != state->item_words)
		elog(ERROR, "TessSort items of %d words, not %d", words, state->item_words);
	if (state->generic >= 0)
		sort_ties(state, &items, words, refs, count, false);
	note_memory(state, mul_size(count, sizeof(uint32) + sizeof(uint64) * words));
	writer_start(state, &writer, run_create(state));
	for (uint64 first = 0; first < count; first += SORT_ROWS)
	{
		int			n = (int) Min((uint64) SORT_ROWS, count - first);
		uint64		bits[1] = {n == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << n) - 1};
		TessRowMask mask = {n, bits};

		for (int column = 0; column < state->ncolumns; column++)
			tess_rows_gather(state->rows, column, &refs[first], &mask,
							 values[column], nulls[column]);
		/* The reference left out: its word dropped, or its bits zeroed in a copy. */
		for (int row = 0; row < n; row++)
		{
			const uint64 *item = &items[(first + row) * words];

			if (state->ext_words < words)
			{
				keys[row] = item;
				continue;
			}
			memcpy(copies[row], item, sizeof(uint64) * words);
			copies[row][words - 1] &= ~UINT64CONST(0xFFFFFFFF);
			keys[row] = copies[row];
		}
		writer_add(state, &writer, n, values, nulls, keys);
		CHECK_FOR_INTERRUPTS();
	}
	writer_finish(state, &writer);
	pfree(items);
	pfree(refs);
	tess_rows_reset(state->rows);
}

/*
 * What an external sort would take: the keys of every run's items, each
 * with its bit for NULL, so that every run's items have one width, and
 * the blocks' size.
 */
static void
plan_external(TessSortState *state)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	Size		row_bytes;
	Size		block_bytes;

	for (int key = 0; key < state->nkernel; key++)
	{
		state->ext_keys[key] = state->keys[key];
		state->ext_keys[key].flags |= TESS_SORT_NULLABLE;
	}
	check_kernel(state->kernels->sort_item_words(state->nkernel, state->ext_keys,
												 &state->item_words, &status),
				 &status);
	/*
	 * A run keeps its items' words without the reference: the last word
	 * goes when it holds no key's bits, as an int4 key's 33 bits leave it,
	 * and the merge compares one word, not two.
	 */
	{
		int			bits = 0;

		for (int key = 0; key < state->nkernel; key++)
			bits += (state->ext_keys[key].kind == TESS_TABLE_KEY_INT8 ? 64 : 32) + 1;
		state->ext_words = bits <= 64 * (state->item_words - 1) ?
			state->item_words - 1 : state->item_words;
	}
	/*
	 * A merge holds a block pair of each run it takes, rows and values, and
	 * briefly the pairs a batch put out points into: blocks of a 128th of
	 * work_mem, 64 rows to 256 kB, so that a work_mem of 4 MB merges 64
	 * runs at once, and a merge of 6 runs at least, as the core's (a small
	 * work_mem is passed then), up to TESS_SORT_MAX_MERGE_RUNS. Blocks of
	 * a 64th and two pairs each took a pass more for 44 runs.
	 */
	row_bytes = sizeof(uint64) *
		(tess_spill_columns_null_lanes(run_words(state)) + run_words(state));
	block_bytes = Min((Size) work_mem * 1024 / 128, (Size) 256 * 1024);
	state->block_rows = (uint32) Max(block_bytes / row_bytes, (Size) SORT_ROWS);
	state->block_values = Max(block_bytes, (Size) 4096);
	block_bytes = Max(block_bytes, (Size) state->block_rows * row_bytes);
	state->fan_in = (int) ((Size) work_mem * 1024 / (2 * block_bytes));
	state->fan_in = Max(state->fan_in, 6);
	state->fan_in = Min(state->fan_in, TESS_SORT_MAX_MERGE_RUNS);
}

/*
 * Whether the rows in memory, what sorting them takes and a chunk more,
 * which the next batch may need, pass work_mem.
 */
static bool
rows_full(TessSortState *state)
{
	uint64		count = tess_rows_count(state->rows);
	Size		bytes = tess_rows_memory(state->rows) +
		(Size) count * (sizeof(uint32) + sizeof(uint64) * state->item_words) +
		state->rows_config.chunk_len;

	return bytes > (Size) work_mem * 1024;
}

/* Blocks rows put out may point into, freed now that those rows are done with. */
static void
free_retired(TessSortState *state)
{
	foreach_ptr(void, block, state->retired)
		pfree(block);
	list_free(state->retired);
	state->retired = NIL;
}

/* The input's next block pair, or false at the run's end; its last one retires. */
static bool
input_load(TessSortState *state, MergeInput *input)
{
	TessSpillHeader header;
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	MemoryContext old;

	if (input->values != NULL)
	{
		old = MemoryContextSwitchTo(context);
		state->retired = lappend(state->retired, input->values);
		state->retired = lappend(state->retired, input->chunk);
		MemoryContextSwitchTo(old);
		input->values = NULL;
		input->chunk = NULL;
	}
	input->rows = 0;
	input->place = 0;
	if (input->block >= input->run->nblocks)
		return false;
	if (!tess_spill_read_header(input->reader, &header) || header.kind != TESS_SPILL_VALUES)
		elog(ERROR, "TessSort run lost its block of values %d", input->block);
	input->values = MemoryContextAllocExtended(context, Max(header.len, 8), MCXT_ALLOC_HUGE);
	tess_spill_read_body(input->reader, input->values, header.len);
	if (!tess_spill_read_header(input->reader, &header) || header.kind != TESS_SPILL_COLUMNS)
		elog(ERROR, "TessSort run lost its block of rows %d", input->block);
	input->chunk = MemoryContextAllocExtended(context, Max(header.len, 8), MCXT_ALLOC_HUGE);
	tess_spill_read_body(input->reader, input->chunk, header.len);
	input->rows = tess_spill_columns_rows(input->chunk);
	if (input->rows != input->run->block_rows[input->block])
		elog(ERROR, "TessSort run block %d has %u rows, not %u", input->block,
			 input->rows, input->run->block_rows[input->block]);
	input->block++;
	return true;
}

static void
input_open(TessSortState *state, MergeInput *input, SortRun *run)
{
	memset(input, 0, sizeof(MergeInput));
	input->run = run;
	input->reader = tess_spill_open(run->set->file, 0, run->partition);
	if (input->reader == NULL && run->nblocks > 0)
		elog(ERROR, "TessSort lost a run");
	if (input->reader != NULL)
		(void) input_load(state, input);
}

static void
input_close(MergeInput *input)
{
	if (input->reader != NULL)
		tess_spill_close(input->reader);
	if (input->values != NULL)
		pfree(input->values);
	if (input->chunk != NULL)
		pfree(input->chunk);
	memset(input, 0, sizeof(MergeInput));
}

/* Row place of the input's block into the columns' arrays at out, and its item's words. */
static void
input_take(TessSortState *state, MergeInput *input, uint32 place, int out,
		   Datum *const *values, bool *const *isnull, uint64 *keys)
{
	const bool *typbyvals = state->rows_config.typbyvals;
	Size		capacity = tess_spill_columns_capacity(input->chunk);
	const uint64 *nulls = tess_spill_columns_lane(input->chunk, 0) + place;
	const uint64 *words = tess_spill_columns_word(input->chunk, 0) + place;

	for (int column = 0; column < state->ncolumns; column++)
	{
		uint64		word = words[capacity * column];

		isnull[column][out] = (nulls[capacity * (column / 64)] >> (column % 64)) & 1;
		if (isnull[column][out])
			values[column][out] = (Datum) 0;
		else if (typbyvals[column])
			values[column][out] = (Datum) word;
		else
			values[column][out] = PointerGetDatum(input->values + word);
	}
	if (keys != NULL)
		for (int word = 0; word < state->ext_words; word++)
			keys[word] = words[capacity * (state->ncolumns + word)];
}

/* Column `column` of the input's row place, as input_take reads it. */
static Datum
input_value(TessSortState *state, MergeInput *input, int column, bool *isnull)
{
	Size		capacity = tess_spill_columns_capacity(input->chunk);
	uint64		nulls = tess_spill_columns_lane(input->chunk, 0)[capacity * (column / 64) +
																input->place];
	uint64		word = tess_spill_columns_word(input->chunk, 0)[capacity * column + input->place];

	*isnull = (nulls >> (column % 64)) & 1;
	if (*isnull)
		return (Datum) 0;
	if (state->rows_config.typbyvals[column])
		return (Datum) word;
	return PointerGetDatum(input->values + word);
}

/*
 * The order of two inputs' next rows for a merge of a generic key: their
 * items' words, then the comparisons of the keys from the first generic
 * one on; the binary heap keeps the greatest first, so the result is
 * reversed.
 */
static int
compare_inputs(bh_node_type a, bh_node_type b, void *arg)
{
	TessSortState *state = arg;
	MergeInput *left = &state->merging[DatumGetInt32(a)];
	MergeInput *right = &state->merging[DatumGetInt32(b)];
	Size		left_capacity = tess_spill_columns_capacity(left->chunk);
	Size		right_capacity = tess_spill_columns_capacity(right->chunk);
	const uint64 *left_words = tess_spill_columns_word(left->chunk, state->ncolumns);
	const uint64 *right_words = tess_spill_columns_word(right->chunk, state->ncolumns);

	for (int word = 0; word < state->ext_words; word++)
	{
		uint64		x = left_words[left_capacity * word + left->place];
		uint64		y = right_words[right_capacity * word + right->place];

		if (x != y)
			return x < y ? 1 : -1;
	}
	for (int key = state->generic; key < state->nkeys; key++)
	{
		int			column = state->key_columns[key];
		bool		left_null;
		bool		right_null;
		Datum		x = input_value(state, left, column, &left_null);
		Datum		y = input_value(state, right, column, &right_null);
		int			result = ApplySortComparator(x, left_null, y, right_null,
												 &state->ssup[key]);

		if (result != 0)
			return -result;
	}
	return 0;
}

/*
 * merge_rows for a generic key: the kernels' merge compares words only,
 * so the inputs' next rows are kept in a binary heap by compare_inputs,
 * which is made anew at every call.
 */
static int
merge_rows_generic(TessSortState *state, MergeInput *inputs, int ninputs, int max,
				   Datum *const *values, bool *const *isnull,
				   uint64 (*keys)[TESS_SORT_MAX_ITEM_WORDS])
{
	binaryheap *heap;
	int			taken = 0;

	if (state->merge_heap == NULL || state->merge_heap->bh_space < ninputs)
	{
		MemoryContext old = MemoryContextSwitchTo(state->css.ss.ps.state->es_query_cxt);

		if (state->merge_heap != NULL)
			binaryheap_free(state->merge_heap);
		state->merge_heap = binaryheap_allocate(ninputs, compare_inputs, state);
		MemoryContextSwitchTo(old);
	}
	heap = state->merge_heap;
	binaryheap_reset(heap);
	state->merging = inputs;
	for (int input = 0; input < ninputs; input++)
	{
		MergeInput *in = &inputs[input];

		if (in->place == in->rows && in->reader != NULL)
			(void) input_load(state, in);
		if (in->place < in->rows)
			binaryheap_add_unordered(heap, Int32GetDatum(input));
	}
	binaryheap_build(heap);
	while (taken < max && !binaryheap_empty(heap))
	{
		int			input = DatumGetInt32(binaryheap_first(heap));
		MergeInput *in = &inputs[input];

		input_take(state, in, in->place++, taken, values, isnull,
				   keys == NULL ? NULL : keys[taken]);
		taken++;
		/* A block given out whole: the next one of the run, if any. */
		if (in->place == in->rows &&
			(in->reader == NULL || !input_load(state, in)))
			(void) binaryheap_remove_first(heap);
		else
			binaryheap_replace_first(heap, Int32GetDatum(input));
	}
	return taken;
}

/*
 * Up to max rows in order from the inputs into the columns' arrays, and
 * their items' words when keys is not NULL; 0 once every input is done.
 */
static int
merge_rows(TessSortState *state, MergeInput *inputs, int ninputs, uint32 *tree, int max,
		   Datum *const *values, bool *const *isnull,
		   uint64 (*keys)[TESS_SORT_MAX_ITEM_WORDS])
{
	const uint64 *lanes[TESS_SORT_MAX_MERGE_RUNS * TESS_SORT_MAX_ITEM_WORDS];
	uint32		left[TESS_SORT_MAX_MERGE_RUNS];
	bool		more[TESS_SORT_MAX_MERGE_RUNS];
	uint32		order[SORT_ROWS];
	int			taken = 0;

	free_retired(state);
	if (state->generic >= 0)
		return merge_rows_generic(state, inputs, ninputs, max, values, isnull, keys);
	while (taken < max)
	{
		TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
		int			count;
		int			refill;

		for (int input = 0; input < ninputs; input++)
		{
			MergeInput *in = &inputs[input];

			/* A block given out whole: the next one of the run. */
			if (in->place == in->rows && in->reader != NULL)
				(void) input_load(state, in);
			left[input] = in->rows - in->place;
			more[input] = in->reader != NULL && in->block < in->run->nblocks;
			for (int word = 0; word < state->ext_words; word++)
				lanes[input * state->ext_words + word] = left[input] == 0 ? NULL :
					tess_spill_columns_word(in->chunk, state->ncolumns + word) + in->place;
		}
		check_kernel(state->kernels->sort_merge(ninputs, state->ext_words, lanes, left, more,
												tree, order, max - taken, &count, &refill,
												&status),
					 &status);
		for (int row = 0; row < count; row++)
		{
			MergeInput *in = &inputs[order[row]];

			input_take(state, in, in->place++, taken + row, values, isnull,
					   keys == NULL ? NULL : keys[taken + row]);
		}
		taken += count;
		if (count == 0 && refill < 0)
			break;
	}
	return taken;
}

/* Merge the runs from `first`, `count` of them, into one run, and free them. */
static void
merge_pass(TessSortState *state, int first, int count, RunWriter *writer)
{
	MergeInput *inputs = palloc0_array(MergeInput, count);
	Datum	  **values = state->values;
	bool	  **nulls = state->isnull;
	uint64		keys[SORT_ROWS][TESS_SORT_MAX_ITEM_WORDS];
	const uint64 *pointers[SORT_ROWS];
	uint32		tree[TESS_SORT_MERGE_STATE_WORDS] = {0};

	for (int row = 0; row < SORT_ROWS; row++)
		pointers[row] = keys[row];
	for (int input = 0; input < count; input++)
		input_open(state, &inputs[input], state->runs[first + input]);
	for (;;)
	{
		int			n = merge_rows(state, inputs, count, tree, SORT_ROWS, values, nulls, keys);

		if (n == 0)
			break;
		writer_add(state, writer, n, values, nulls, pointers);
		CHECK_FOR_INTERRUPTS();
	}
	free_retired(state);
	for (int input = 0; input < count; input++)
	{
		input_close(&inputs[input]);
		run_free(state->runs[first + input]);
	}
	pfree(inputs);
}

/*
 * After the input: the runs merge, fan_in at a time, into longer runs
 * until one merge takes them all; for a scan backward, into one run, read
 * by blocks. Then the last merge's inputs open.
 */
static void
merge_runs(TessSortState *state)
{
	bool		one = (state->eflags & EXEC_FLAG_BACKWARD) != 0;

	while (state->nruns > (one ? 1 : state->fan_in))
	{
		int			nold = state->nruns;
		SortRun   **old = palloc_array(SortRun *, nold);
		int			done = 0;

		memcpy(old, state->runs, sizeof(SortRun *) * nold);
		state->nruns = 0;
		while (done < nold)
		{
			int			count = Min(state->fan_in, nold - done);
			RunWriter	writer;

			/* A run left alone goes on as it is. */
			if (count == 1)
			{
				state->runs[state->nruns++] = old[done++];
				continue;
			}
			writer_start(state, &writer, run_create(state));
			/* merge_pass reads its runs from state->runs: put them there for it. */
			{
				SortRun   **saved = state->runs;
				int			nsaved = state->nruns;

				state->runs = old;
				merge_pass(state, done, count, &writer);
				state->runs = saved;
				state->nruns = nsaved;
			}
			writer_finish(state, &writer);
			done += count;
		}
		pfree(old);
		/* The pass's runs are read by the next. */
		set_finish(state);
		state->merge_passes++;
	}
	state->single = state->nruns == 1;
	if (state->single)
	{
		memset(&state->shown, 0, sizeof(MergeInput));
		state->shown.run = state->runs[0];
		state->shown.reader = tess_spill_open(state->runs[0]->set->file, 0,
											  state->runs[0]->partition);
		state->shown.block = -1;
		return;
	}
	state->merge_state[0] = 0;
	state->ninputs = state->nruns;
	state->inputs = MemoryContextAllocZero(state->css.ss.ps.state->es_query_cxt,
										   sizeof(MergeInput) * state->ninputs);
	for (int input = 0; input < state->ninputs; input++)
		input_open(state, &state->inputs[input], state->runs[input]);
}

/* The last merge from the first row again, for a rescan. */
static void
restart_merge(TessSortState *state)
{
	free_retired(state);
	if (state->single)
	{
		if (state->shown.values != NULL)
			pfree(state->shown.values);
		if (state->shown.chunk != NULL)
			pfree(state->shown.chunk);
		state->shown.values = NULL;
		state->shown.chunk = NULL;
		state->shown.block = -1;
		return;
	}
	state->merge_state[0] = 0;
	for (int input = 0; input < state->ninputs; input++)
	{
		input_close(&state->inputs[input]);
		input_open(state, &state->inputs[input], state->runs[input]);
	}
}

/* Free every run and input of an external sort, for the end or a new read. */
static void
free_external(TessSortState *state)
{
	/* A set cut short by an error or a new read is finished, to go with its runs. */
	set_finish(state);
	free_retired(state);
	for (int input = 0; input < state->ninputs; input++)
		input_close(&state->inputs[input]);
	if (state->inputs != NULL)
		pfree(state->inputs);
	state->inputs = NULL;
	state->ninputs = 0;
	if (state->single)
	{
		if (state->shown.reader != NULL)
			tess_spill_close(state->shown.reader);
		if (state->shown.values != NULL)
			pfree(state->shown.values);
		if (state->shown.chunk != NULL)
			pfree(state->shown.chunk);
		memset(&state->shown, 0, sizeof(MergeInput));
	}
	for (int run = 0; run < state->nruns; run++)
		run_free(state->runs[run]);
	state->nruns = 0;
	state->external = false;
	state->single = false;
	state->merge_passes = 0;
	state->runs_written = 0;
	state->disk_bytes = 0;
}

/* The single run's block holding row place, in memory. */
static void
show_block_of(TessSortState *state, uint64 place)
{
	SortRun    *run = state->shown.run;
	int			low = 0;
	int			high = run->nblocks - 1;
	TessSpillHeader header;
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;

	while (low < high)
	{
		int			middle = (low + high + 1) / 2;

		if (run->block_first[middle] <= place)
			low = middle;
		else
			high = middle - 1;
	}
	if (state->shown.block == low)
		return;
	/* The block of the rows shown so far goes with the next rows. */
	if (state->shown.values != NULL)
	{
		MemoryContext old = MemoryContextSwitchTo(context);

		state->retired = lappend(state->retired, state->shown.values);
		state->retired = lappend(state->retired, state->shown.chunk);
		MemoryContextSwitchTo(old);
	}
	tess_spill_seek(state->shown.reader, run->positions[low]);
	if (!tess_spill_read_header(state->shown.reader, &header) || header.kind != TESS_SPILL_VALUES)
		elog(ERROR, "TessSort run lost its block of values %d", low);
	state->shown.values = MemoryContextAllocExtended(context, Max(header.len, 8), MCXT_ALLOC_HUGE);
	tess_spill_read_body(state->shown.reader, state->shown.values, header.len);
	if (!tess_spill_read_header(state->shown.reader, &header) || header.kind != TESS_SPILL_COLUMNS)
		elog(ERROR, "TessSort run lost its block of rows %d", low);
	state->shown.chunk = MemoryContextAllocExtended(context, Max(header.len, 8), MCXT_ALLOC_HUGE);
	tess_spill_read_body(state->shown.reader, state->shown.chunk, header.len);
	state->shown.rows = tess_spill_columns_rows(state->shown.chunk);
	state->shown.block = low;
}

/*
 * The external sort's rows from place start into the batch: from the
 * single run's block that holds it, the rows of that block from there; or
 * the next rows of the last merge. Returns the rows, 0 at the end.
 */
static int
external_window(TessSortState *state, uint64 start, bool backward)
{
	int			n;

	if (!state->single)
		n = merge_rows(state, state->inputs, state->ninputs, state->merge_state, SORT_ROWS,
					   state->values, state->isnull, NULL);
	else
	{
		SortRun    *run = state->shown.run;
		uint64		first;
		uint64		end;

		free_retired(state);
		if (start >= run->rows)
			return 0;
		show_block_of(state, start);
		first = run->block_first[state->shown.block];
		end = first + state->shown.rows;
		n = (int) Min((uint64) SORT_ROWS, end - start);
		(void) backward;
		for (int row = 0; row < n; row++)
			input_take(state, &state->shown, (uint32) (start - first + row), row,
					   state->values, state->isnull, NULL);
	}
	state->start = start;
	state->window_bits[0] = n == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << n) - 1;
	state->batch.rows.nrows = n;
	state->batch.rows.bits = state->window_bits;
	memset(state->gathered, true, sizeof(bool) * state->ncolumns);
	return n;
}

/*
 * Read every batch of the child and sort the records. The child runs
 * forward whatever direction the first fetch has.
 */
static void
sort_rows(TessSortState *state)
{
	EState	   *estate = state->css.ss.ps.state;
	ScanDirection direction = estate->es_direction;
	TessSortKey keys[TESS_TABLE_MAX_KEYS];

	state->topn = choose_topn(state);
	state->used_bound = state->topn ? state->bound : -1;
	if (state->topn)
	{
		Size		words = mul_size((Size) state->bound, state->words);

		if (state->heap != NULL)
			pfree(state->heap);
		state->heap_capacity = (Size) state->bound;
		state->heap_len = 0;
		/* A generic key's heap has a spare slot, and the keys' values. */
		if (state->generic >= 0)
		{
			Size		slots = mul_size((Size) state->bound + 1,
										 state->nkeys - state->generic);

			words = add_size(words, state->words);
			if (state->top_values != NULL)
			{
				pfree(state->top_values);
				pfree(state->top_isnull);
			}
			state->top_values = MemoryContextAllocExtended(estate->es_query_cxt,
														   mul_size(slots, sizeof(Datum)),
														   MCXT_ALLOC_HUGE);
			state->top_isnull = MemoryContextAllocExtended(estate->es_query_cxt, slots,
														   MCXT_ALLOC_HUGE);
		}
		state->heap = MemoryContextAllocExtended(estate->es_query_cxt,
												 mul_size(Max(words, 1), sizeof(uint64)),
												 MCXT_ALLOC_HUGE);
	}
	estate->es_direction = ForwardScanDirection;
	/* A bound of no rows reads nothing. */
	while (!(state->topn && state->heap_capacity == 0))
	{
		TessBatch  *batch = tess_input_next(state->input);
		int			rows;

		if (batch == NULL)
			break;
		rows = tess_row_mask_count(&batch->rows);
		state->counters.batches++;
		state->counters.rows += rows;
		if (rows > 0 && state->topn && state->generic >= 0)
			top_batch_generic(state, batch);
		else if (rows > 0 && state->topn)
			top_batch(state, batch);
		else if (rows > 0)
			append_batch(state, batch);
		tess_input_finish(state->input);
		/* Past work_mem: the rows so far go to a run, sorted. */
		if (!state->topn && rows > 0 && rows_full(state))
		{
			state->external = true;
			spill_run(state);
		}
		CHECK_FOR_INTERRUPTS();
	}
	estate->es_direction = direction;
	if (state->external)
	{
		spill_run(state);
		free_ties(state);
		set_finish(state);
		merge_runs(state);
		note_memory(state, (Size) Max(state->ninputs, 1) *
					(state->block_values + sizeof(uint64) * state->block_rows *
					 (tess_spill_columns_null_lanes(run_words(state)) + run_words(state))));
		state->count = 0;
		for (int run = 0; run < state->nruns; run++)
			state->count += state->runs[run]->rows;
		state->sorted = true;
		state->current = -1;
		return;
	}
	if (state->topn)
	{
		TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

		/* The heap's items, sorted, give the best rows in order. */
		state->count = state->heap_len;
		if (state->refs != NULL)
			pfree(state->refs);
		state->refs = MemoryContextAllocExtended(estate->es_query_cxt,
												 mul_size(sizeof(uint32),
														  Max(state->count, 1)),
												 MCXT_ALLOC_HUGE);
		if (state->count > 0)
			check_kernel(state->kernels->sort(state->heap, (Size) state->count,
											  state->words, state->refs, &status),
						 &status);
		/* Items of equal words by the comparisons, as a full sort orders them. */
		if (state->count > 0 && state->generic >= 0)
		{
			uint64	   *items = state->heap;

			sort_ties(state, &items, state->words, state->refs, state->count, false);
			free_ties(state);
		}
		note_memory(state, state->heap_capacity * state->words * sizeof(uint64));
		state->sorted = true;
		state->current = -1;
		return;
	}
	state->count = tess_rows_count(state->rows);
	/* A key takes the bit for NULL only when one of its rows held one. */
	for (int key = 0; key < state->nkernel; key++)
	{
		keys[key] = state->keys[key];
		if (state->key_nulls[key])
			keys[key].flags |= TESS_SORT_NULLABLE;
	}
	if (state->refs != NULL)
		pfree(state->refs);
	state->refs = MemoryContextAllocExtended(estate->es_query_cxt,
											 mul_size(sizeof(uint32),
													  Max(state->count, 1)),
											 MCXT_ALLOC_HUGE);
	if (state->count > 0)
	{
		int			words;
		TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

		if (state->kernels->sort_item_words(state->nkernel, keys, &words,
											&status) != TESS_OK)
			tess_status_report(&status);
		note_memory(state, mul_size(state->count,
									sizeof(uint32) + sizeof(uint64) * words));
		if (state->generic >= 0)
		{
			uint64	   *items = tess_rows_sort_items(state->rows, keys, state->refs, &words);

			sort_ties(state, &items, words, state->refs, state->count, true);
			note_memory(state, mul_size(state->count, sizeof(uint32)) +
						state->tie_capacity * sizeof(TieRow));
			free_ties(state);
		}
		else
			tess_rows_sort(state->rows, keys, state->refs);
	}
	else
		note_memory(state, 0);
	state->sorted = true;
	state->current = -1;
}

/* Make the rows from place start the batch, its columns not gathered yet. */
static void
show_window(TessSortState *state, uint64 start)
{
	int			n = (int) Min((uint64) SORT_ROWS, state->count - start);

	state->start = start;
	state->window_bits[0] = n == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << n) - 1;
	state->batch.rows.nrows = n;
	state->batch.rows.bits = state->window_bits;
	memset(state->gathered, 0, sizeof(bool) * state->ncolumns);
}

/* The next batch of rows in order for a batch-aware parent, or NULL. */
static TupleTableSlot *
next_batch(TessSortState *state)
{
	uint64		start = (uint64) (state->current + 1);

	tess_output_release(state->output);
	state->published = false;
	if (start >= state->count)
	{
		state->current = (int64) state->count;
		return NULL;
	}
	if (state->external)
	{
		if (external_window(state, start, false) == 0)
		{
			state->current = (int64) state->count;
			return NULL;
		}
	}
	else
		show_window(state, start);
	state->current = (int64) (start + state->batch.rows.nrows - 1);
	state->published = true;
	return tess_output_publish(state->output, &state->batch);
}

/*
 * The next row in the scan's direction for a row-wise parent: a place
 * outside the batch shown makes the batch the rows from it on, or, going
 * backward, the rows up to it.
 */
static TupleTableSlot *
next_row(TessSortState *state, bool forward)
{
	int64		place = state->current + (forward ? 1 : -1);
	TupleTableSlot *slot;

	if (place < 0)
	{
		state->current = -1;
		return NULL;
	}
	if ((uint64) place >= state->count)
	{
		state->current = (int64) state->count;
		return NULL;
	}
	state->current = place;
	if (state->published && (uint64) place >= state->start &&
		(uint64) place < state->start + state->batch.rows.nrows)
		return tess_output_select(state->output, (int) (place - state->start));
	if (state->published)
		tess_output_finish(state->output);
	tess_output_release(state->output);
	if (state->external)
	{
		uint64		start = (uint64) place;

		/* Backward: the rows of place's block up to it, from the single run. */
		if (!forward)
		{
			if (!state->single)
				elog(ERROR, "TessSort returns rows backward only when planned for it");
			show_block_of(state, (uint64) place);
			start = Max(state->shown.run->block_first[state->shown.block],
						(uint64) Max(place - (SORT_ROWS - 1), 0));
		}
		else if (!state->single && state->published &&
				 (uint64) place != state->start + state->batch.rows.nrows)
			elog(ERROR, "TessSort merges its runs forward only");
		if (external_window(state, start, !forward) == 0)
		{
			state->published = false;
			state->current = (int64) state->count;
			return NULL;
		}
	}
	else
		show_window(state, forward ? (uint64) place :
					(uint64) Max(place - (SORT_ROWS - 1), 0));
	state->published = true;
	slot = tess_output_publish(state->output, &state->batch);
	if ((uint64) place == state->start)
		return slot;
	return tess_output_select(state->output, (int) (place - state->start));
}

static TupleTableSlot *
sort_exec(CustomScanState *css)
{
	TessSortState *state = (TessSortState *) css;
	bool		forward = ScanDirectionIsForward(css->ss.ps.state->es_direction);
	bool		rows = tess_output_request(state->output)->output_mode ==
		TESS_OUTPUT_ROWS;

	/* Rows read for fewer than the parent now needs are read again. */
	if (state->sorted && state->used_bound >= 0 &&
		(state->bound < 0 || state->bound > state->used_bound))
		reread_child(state);
	if (!state->sorted)
		sort_rows(state);
	if (rows)
		return next_row(state, forward);
	if (!forward)
		elog(ERROR, "TessSort returns batches only forward");
	return next_batch(state);
}

static void
sort_end(CustomScanState *css)
{
	TessSortState *state = (TessSortState *) css;

	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	tess_output_end(state->output);
	ExecEndNode(state->child);
	free_external(state);
	tess_rows_free(state->rows);
}

/* Rescan the child and forget the rows, to read and sort them anew. */
static void
reread_child(TessSortState *state)
{
	tess_output_clear(state->output);
	state->published = false;
	state->current = -1;
	ExecReScan(state->child);
	tess_input_rescan(state->input);
	tess_rows_reset(state->rows);
	free_external(state);
	memset(state->key_nulls, 0, sizeof(state->key_nulls));
	state->sorted = false;
	state->count = 0;
	state->heap_len = 0;
}

/*
 * A rescan returns the sorted rows again from the first, unless a
 * parameter of the child changed: then the child is read and sorted anew.
 * A parent may set another bound before it fetches; the execution reads
 * again when the rows kept are too few for it.
 */
static void
sort_rescan(CustomScanState *css)
{
	TessSortState *state = (TessSortState *) css;

	tess_output_clear(state->output);
	state->published = false;
	state->current = -1;
	state->bound = -1;
	if (css->ss.ps.chgParam == NULL && state->sorted)
	{
		/* The runs stay on disk: the last merge starts again. */
		if (state->external)
			restart_merge(state);
		return;
	}
	/* The core passes changed parameters to outer and inner plans only. */
	if (css->ss.ps.chgParam != NULL)
		UpdateChangedParamSet(state->child, css->ss.ps.chgParam);
	if (!state->sorted && state->child->chgParam == NULL)
		return;
	reread_child(state);
}

static void
sort_set_tuple_bound(CustomScanState *css, int64 tuples_needed)
{
	TessSortState *state = (TessSortState *) css;

	state->bound = tuples_needed < 0 ? -1 : tuples_needed;
}

/* This participant's counters. */
static void
sort_counters(TessSortState *state, uint64 *values)
{
	Size		limit = (Size) work_mem * 1024;

	memset(values, 0, sizeof(uint64) * SORT_NCOUNTERS);
	values[SORT_BATCHES] = state->counters.batches;
	values[SORT_INPUT_ROWS] = state->counters.rows;
	values[SORT_MEMORY] = state->counters.memory;
	values[SORT_OVERRUN] = state->counters.memory > limit ? state->counters.memory - limit : 0;
	values[SORT_SORTED] = state->sorted ? 1 : 0;
	values[SORT_EXTERNAL] = state->sorted && state->external ? 1 : 0;
	values[SORT_TOPN] = state->sorted && state->topn ? 1 : 0;
	values[SORT_RUNS] = (uint64) state->runs_written;
	values[SORT_PASSES] = (uint64) state->merge_passes;
	values[SORT_DISK] = state->disk_bytes;
	values[SORT_REBUILT] = state->topn ? state->compactions : 0;
}

static void
sort_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	TessSortState *state = (TessSortState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	List	   *context = set_deparse_context_plan(es->deparse_cxt,
												   css->ss.ps.plan, ancestors);
	bool		useprefix = es->rtable_size > 1 || es->verbose;
	List	   *keys = NIL;
	const uint64 *totals = NULL;
	uint64		own[SORT_NCOUNTERS];

	foreach_ptr(Node, expr, cscan->custom_exprs)
	{
		char	   *key = deparse_expression(expr, context, useprefix, false);
		uint32		flags = state->keys[foreach_current_index(expr)].flags;
		bool		descending = (flags & TESS_SORT_DESCENDING) != 0;
		bool		nulls_first = (flags & TESS_SORT_NULLS_FIRST) != 0;

		int			index = foreach_current_index(expr);
		Oid			collation = state->ssup != NULL && index >= state->generic ?
			state->ssup[index].ssup_collation : InvalidOid;

		/* As the core shows them: what is not the default is spelled out. */
		if (OidIsValid(collation) && collation != DEFAULT_COLLATION_OID)
			key = psprintf("%s COLLATE %s", key,
						   quote_identifier(get_collation_name(collation)));
		if (descending)
			key = psprintf("%s DESC", key);
		if (nulls_first != descending)
			key = psprintf("%s NULLS %s", key, nulls_first ? "FIRST" : "LAST");
		keys = lappend(keys, key);
	}
	ExplainPropertyList("Sort Key", keys, es);
	if (!es->analyze)
		return;
	if (state->stats != NULL)
		totals = tess_shared_stats_totals(state->stats);
	if (totals == NULL)
	{
		sort_counters(state, own);
		totals = own;
	}
	if (totals[SORT_SORTED] == 0)
		return;
	ExplainPropertyText("Sort Method", totals[SORT_TOPN] > 0 ? "top-N in memory" :
						totals[SORT_EXTERNAL] > 0 ? "external merge" : "in memory", es);
	ExplainPropertyInteger("Memory Usage", "kB", (totals[SORT_MEMORY] + 1023) / 1024, es);
	if (totals[SORT_EXTERNAL] > 0)
	{
		ExplainPropertyInteger("Disk Usage", "kB", (totals[SORT_DISK] + 1023) / 1024, es);
		ExplainPropertyInteger("Runs", NULL, totals[SORT_RUNS], es);
		if (totals[SORT_PASSES] > 0)
			ExplainPropertyInteger("Merge Passes", NULL, totals[SORT_PASSES], es);
	}
	if (totals[SORT_OVERRUN] > 0)
		ExplainPropertyInteger("Overrun", "kB", (totals[SORT_OVERRUN] + 1023) / 1024, es);
	ExplainPropertyInteger("Input Batches", NULL, totals[SORT_BATCHES], es);
	ExplainPropertyInteger("Input Rows", NULL, totals[SORT_INPUT_ROWS], es);
	if (totals[SORT_REBUILT] > 0)
		ExplainPropertyInteger("Rows Rebuilt", NULL, totals[SORT_REBUILT], es);
}

/*
 * Under a Gather Merge every participant sorts its share, and the node,
 * parallel-aware for this alone, shares only its counters, in the rows of
 * its chunk.
 */
static Size
sort_estimate_dsm(CustomScanState *css, ParallelContext *pcxt)
{
	return tess_shared_stats_estimate(SORT_NCOUNTERS, pcxt->nworkers);
}

static void
sort_initialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate)
{
	TessSortState *state = (TessSortState *) css;

	/* A Gather Merge a limit above shut down sets up anew when rescanned. */
	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	state->stats = tess_shared_stats_init(css->ss.ps.state->es_query_cxt, coordinate,
										  SORT_NCOUNTERS, pcxt->nworkers, pcxt->seg);
}

static void
sort_reinitialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate)
{
	TessSortState *state = (TessSortState *) css;

	tess_shared_stats_reset(state->stats);
}

static void
sort_initialize_worker(CustomScanState *css, shm_toc *toc, void *coordinate)
{
	TessSortState *state = (TessSortState *) css;

	state->stats = tess_shared_stats_attach(css->ss.ps.state->es_query_cxt, coordinate,
											ParallelWorkerNumber + 1);
}

static void
sort_shutdown(CustomScanState *css)
{
	TessSortState *state = (TessSortState *) css;
	uint64		values[SORT_NCOUNTERS];

	if (state->stats == NULL)
		return;
	sort_counters(state, values);
	tess_shared_stats_store(state->stats, values);
}

static const CustomExecMethods sort_exec_methods = {
	.CustomName = "TessSort",
	.BeginCustomScan = sort_begin,
	.ExecCustomScan = sort_exec,
	.EndCustomScan = sort_end,
	.ReScanCustomScan = sort_rescan,
	.EstimateDSMCustomScan = sort_estimate_dsm,
	.InitializeDSMCustomScan = sort_initialize_dsm,
	.ReInitializeDSMCustomScan = sort_reinitialize_dsm,
	.InitializeWorkerCustomScan = sort_initialize_worker,
	.ShutdownCustomScan = sort_shutdown,
	.ExplainCustomScan = sort_explain,
};

static Node *
sort_create_state(CustomScan *cscan)
{
	TessSortState *state = (TessSortState *)
		newNode(sizeof(TessSortState), T_CustomScanState);

	state->css.methods = &sort_exec_methods;
	return (Node *) state;
}

const CustomScanMethods tess_sort_scan_methods = {
	.CustomName = "TessSort",
	.CreateCustomScanState = sort_create_state,
};

const TessNode tess_sort_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_SORT_NODE_NAME,
	.set_tuple_bound = sort_set_tuple_bound,
};

void
tess_sort_planner_init(void)
{
	previous_create_upper_paths_hook = create_upper_paths_hook;
	create_upper_paths_hook = create_upper_paths;
}

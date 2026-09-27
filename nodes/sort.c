#include "postgres.h"

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
#include "utils/memutils.h"
#include "utils/ruleutils.h"

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
 * forward and backward, to a row-wise parent. Keys are int4 or int8
 * values of the output; the rows are held in memory, the node taking the
 * path only when the planner expects them within work_mem. See
 * docs/nodes.md.
 */

/* Rows of an output batch. */
#define SORT_ROWS 64

/* The counters of the node. */
typedef struct SortCounters
{
	uint64		batches;
	uint64		rows;
	Size		memory;
} SortCounters;

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
} TessSortState;

static const CustomExecMethods sort_exec_methods;
static void reread_child(TessSortState *state);
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
static bool
sort_key_of(PathKey *pathkey, PathTarget *target, Relids relids, int *place,
			TessSortKey *key)
{
	EquivalenceClass *ec = pathkey->pk_eclass;

	if (pathkey->pk_opfamily != INTEGER_BTREE_FAM_OID || ec->ec_has_volatile ||
		(pathkey->pk_cmptype != COMPARE_LT && pathkey->pk_cmptype != COMPARE_GT))
		return false;
	foreach_ptr(Expr, expr, target->exprs)
	{
		Oid			type = exprType((Node *) expr);

		if ((type != INT4OID && type != INT8OID) ||
			find_ec_member_matching_expr(ec, expr, relids) == NULL)
			continue;
		*place = foreach_current_index(expr);
		key->kind = type == INT8OID ? TESS_TABLE_KEY_INT8 : TESS_TABLE_KEY_INT4;
		key->flags = (pathkey->pk_cmptype == COMPARE_GT ? TESS_SORT_DESCENDING : 0) |
			(pathkey->pk_nulls_first ? TESS_SORT_NULLS_FIRST : 0);
		return true;
	}
	return false;
}

/*
 * The node's path in place of the core's full sort: the same planner
 * properties over the batch child of the sort's input, with the key
 * expressions, which the sort's targets hold, and their kinds and flags. NULL when a key is not one the
 * kernels sort, the output has too many columns, the rows would not fit
 * work_mem, or the input cannot be read in batches.
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
	int			nkeys = list_length(sort->path.pathkeys);
	int			ncolumns = list_length(target->exprs);
	double		bytes;
	double		rows;
	Path	   *child;

	if (nkeys == 0 || nkeys > TESS_TABLE_MAX_KEYS ||
		ncolumns == 0 || ncolumns > TESS_ROWS_MAX_COLUMNS)
		return NULL;
	foreach_node(PathKey, pathkey, sort->path.pathkeys)
	{
		TessSortKey key;
		int			place;

		if (!sort_key_of(pathkey, target, input->parent->relids, &place, &key))
			return NULL;
		exprs = lappend(exprs, list_nth(target->exprs, place));
		kinds = lappend_int(kinds, (int) key.kind);
		flags = lappend_int(flags, (int) key.flags);
	}
	/*
	 * What the rows take: a record of its header, key slots and a word per
	 * column, the by-reference values at most the row's width, an item of
	 * up to two words per key and the reference.
	 */
	rows = input->rows;
	/* Under a limit, a top-N sort keeps its rows and a few times more. */
	if (root->limit_tuples >= 0)
		rows = Min(rows, Max(4.0 * root->limit_tuples, 65536.0));
	bytes = rows *
		(16.0 + 8.0 * nkeys + 8.0 * (1 + ncolumns) + target->width +
		 16.0 * nkeys + 8.0 + sizeof(uint32));
	if (bytes > (double) work_mem * 1024.0)
		return NULL;
	child = tess_batch_input_path(root, input);
	if (child == NULL)
		return NULL;
	config.template_path = &sort->path;
	config.methods = &sort_path_methods;
	config.node = &tess_sort_node;
	config.children = list_make1(child);
	config.expressions = exprs;
	config.node_data = (Node *) list_make2(kinds, flags);
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
	tess_plan_reader_finish(reader);
	state->ncolumns = list_length(columns);
	state->nkeys = list_length(keys);
	if (state->ncolumns != desc->natts || state->ncolumns == 0 ||
		state->ncolumns > TESS_ROWS_MAX_COLUMNS || state->nkeys == 0 ||
		state->nkeys > TESS_TABLE_MAX_KEYS ||
		list_length(key_kinds) != state->nkeys ||
		list_length(key_flags) != state->nkeys)
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
	foreach_int(key, keys)
	{
		int			index = foreach_current_index(key);

		if (key < 0 || key >= state->ncolumns)
			elog(ERROR, "TessSort received a foreign plan");
		state->key_columns[index] = key;
		kinds[index] = (TessTableKeyKind) list_nth_int(key_kinds, index);
		state->keys[index].kind = kinds[index];
		state->keys[index].flags = (uint32) list_nth_int(key_flags, index);
	}
	/* Whole batches: every column of the rows kept. */
	request.projection_columns = projection;
	request.output_mode = TESS_OUTPUT_BATCH;
	tess_input_set_request(state->input, &request);

	memcpy(state->kinds, kinds, sizeof(TessTableKeyKind) * state->nkeys);
	rows.parent_context = estate->es_query_cxt;
	rows.kernels = state->kernels;
	rows.nkeys = state->nkeys;
	rows.kinds = state->kinds;
	rows.ncolumns = state->ncolumns;
	rows.typlens = typlens;
	rows.typbyvals = typbyvals;
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

/* The key columns of the batch, as the table takes them. */
static void
batch_keys(TessSortState *state, TessBatch *batch)
{
	for (int key = 0; key < state->nkeys; key++)
	{
		batch_column(state, batch, state->key_columns[key]);
		state->table_keys[key].kind = state->keys[key].kind;
		state->table_keys[key].column = &state->columns[state->key_columns[key]];
		state->table_keys[key].prepared = NULL;
	}
}

/* Whether an output column is a key's, fetched with the keys. */
static bool
is_key_column(TessSortState *state, int column)
{
	for (int key = 0; key < state->nkeys; key++)
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
	for (int key = 0; key < state->nkeys; key++)
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
	Datum		values[TESS_ROWS_MAX_COLUMNS][SORT_ROWS];
	bool		nulls[TESS_ROWS_MAX_COLUMNS][SORT_ROWS];
	TessDatumColumn columns[TESS_ROWS_MAX_COLUMNS];
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
							 values[column], nulls[column]);
			columns[column] = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
			columns[column].values = values[column];
			columns[column].isnull = nulls[column];
			columns[column].nrows = n;
		}
		for (int key = 0; key < state->nkeys; key++)
		{
			keys[key].kind = state->keys[key].kind;
			keys[key].column = &columns[state->key_columns[key]];
			keys[key].prepared = NULL;
		}
		tess_rows_append(rows, keys, columns, &mask, new_refs);
		/* The items of the kept rows go in anew, by their new records. */
		tess_rows_top_push(rows, state->top_keys, new_refs, &mask, state->heap,
						   state->heap_capacity, &state->heap_len);
	}
	pfree(kept);
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

		check_kernel(state->kernels->sort_top_candidates(state->nkeys,
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
	for (int key = 0; key < state->nkeys; key++)
	{
		state->top_keys[key] = state->keys[key];
		state->top_keys[key].flags |= TESS_SORT_NULLABLE;
	}
	check_kernel(state->kernels->sort_item_words(state->nkeys, state->top_keys,
												 &state->words, &status),
				 &status);
	bytes = (double) state->bound * state->words * sizeof(uint64) +
		(double) Max(4 * (double) state->bound, 65536.0) *
		(16.0 + 8.0 * (state->nkeys + 1 + state->ncolumns));
	return bytes <= (double) work_mem * 1024.0;
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
		if (rows > 0 && state->topn)
			top_batch(state, batch);
		else if (rows > 0)
			append_batch(state, batch);
		tess_input_finish(state->input);
		CHECK_FOR_INTERRUPTS();
	}
	estate->es_direction = direction;
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
		note_memory(state, state->heap_capacity * state->words * sizeof(uint64));
		state->sorted = true;
		state->current = -1;
		return;
	}
	state->count = tess_rows_count(state->rows);
	/* A key takes the bit for NULL only when one of its rows held one. */
	for (int key = 0; key < state->nkeys; key++)
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

		if (state->kernels->sort_item_words(state->nkeys, keys, &words,
											&status) != TESS_OK)
			tess_status_report(&status);
		note_memory(state, mul_size(state->count,
									sizeof(uint32) + sizeof(uint64) * words));
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

	tess_output_end(state->output);
	ExecEndNode(state->child);
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
		return;
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

static void
sort_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	TessSortState *state = (TessSortState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	List	   *context = set_deparse_context_plan(es->deparse_cxt,
												   css->ss.ps.plan, ancestors);
	bool		useprefix = es->rtable_size > 1 || es->verbose;
	List	   *keys = NIL;

	foreach_ptr(Node, expr, cscan->custom_exprs)
	{
		char	   *key = deparse_expression(expr, context, useprefix, false);
		uint32		flags = state->keys[foreach_current_index(expr)].flags;
		bool		descending = (flags & TESS_SORT_DESCENDING) != 0;
		bool		nulls_first = (flags & TESS_SORT_NULLS_FIRST) != 0;

		/* As the core shows them: what is not the default is spelled out. */
		if (descending)
			key = psprintf("%s DESC", key);
		if (nulls_first != descending)
			key = psprintf("%s NULLS %s", key, nulls_first ? "FIRST" : "LAST");
		keys = lappend(keys, key);
	}
	ExplainPropertyList("Sort Key", keys, es);
	if (!es->analyze || !state->sorted)
		return;
	ExplainPropertyText("Sort Method", state->topn ? "top-N in memory" : "in memory", es);
	ExplainPropertyInteger("Memory Usage", "kB",
						   (state->counters.memory + 1023) / 1024, es);
	if (state->counters.memory > (Size) work_mem * 1024)
		ExplainPropertyInteger("Overrun", "kB",
							   (state->counters.memory - (Size) work_mem * 1024 + 1023) / 1024,
							   es);
	ExplainPropertyInteger("Input Batches", NULL, state->counters.batches, es);
	ExplainPropertyInteger("Input Rows", NULL, state->counters.rows, es);
	if (state->topn && state->compactions > 0)
		ExplainPropertyInteger("Rows Rebuilt", NULL, state->compactions, es);
}

static const CustomExecMethods sort_exec_methods = {
	.CustomName = "TessSort",
	.BeginCustomScan = sort_begin,
	.ExecCustomScan = sort_exec,
	.EndCustomScan = sort_end,
	.ReScanCustomScan = sort_rescan,
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

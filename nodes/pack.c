#include "postgres.h"

#include "catalog/pg_am_d.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "nodes/makefuncs.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/plancat.h"
#include "optimizer/tlist.h"
#if TESS_GREENGAGE
#include "access/tableam.h"
#include "cdb/cdbaocsam.h"
#include "cdb/cdbappendonlyam.h"
#endif

#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * TessPack turns the rows of an ordinary child into batches for a
 * batch-aware parent. The parent creates the path through
 * tess_batch_input_path, so the node never stands under a row-wise one;
 * a request for rows is an error at the first execution.
 *
 * Above a sequential scan of a plain table the pack plans the scan with
 * the relation's physical target list, so that it returns its buffer
 * tuple slot without projecting, and keeps the tuples in a heap batch
 * that deforms a column only when a consumer asks for it, for the rows
 * asked for. Above any other child, or a scan the executor projects, the
 * builder copies every column of every row. Above a subquery scan without
 * clauses whose subquery is planned as a batch path, the pack packs
 * nothing: it forwards the batches of the plan under the subquery scan,
 * which the planner keeps for the subquery's range table but the pack
 * never executes. See docs/nodes.md.
 */
#define PACK_BATCH_ROWS 64

/* The path's data: how the pack stands above its child. */
#define PACK_PHYSICAL_TARGETS 1
#define PACK_FORWARD 2

typedef struct PackState
{
	CustomScanState css;
	PlanState  *child;
	/* Forwarding: the unary helper reads the batches under the child. */
	TessUnary  *unary;
	TessOutput *output;
	TessLayout	layout;
	/* One of the two is created at the first execution, by the first slot. */
	TessBuilder *builder;
	TessHeapBatch *heap;
	const TessRequest *request;
	int			capacity;
	/* The child returned its last row. */
	bool		exhausted;
	/* Rows the parent needs at most in this scan, or -1; rows given so far. */
	int64		tuples_needed;
	int64		produced;
	uint64		batches;
#if TESS_GREENGAGE
	/* An append-only table the pack reads in blocks, or NULL. */
	struct PackAocs *aocs;
#endif
} PackState;

#if TESS_GREENGAGE
/*
 * Greengage's append-only table read by the pack itself, in place of the
 * core's sequential scan under it, with no tuple slot: for ao_column
 * aocs_getnext_batch gives the rows of the projected columns column by
 * column straight from their blocks, for ao_row appendonly_getnext_batch
 * deforms each row of a block and lays the columns asked for out the
 * same way. The batch's columns are those arrays; a by-reference value
 * points into its block, which the scan keeps until the next batch is
 * read, after the parent released this one. The child stays in the plan,
 * initialized and never executed.
 */
typedef struct PackAocs
{
	TessBatch	batch;
	Relation	relation;
	bool		columns;		/* ao_column, else ao_row */
	/* Begun at the first execution, with the columns of the child's targets. */
	AOCSScanDesc scan;
	TableScanDesc row_scan;
	/* A row of ao_row deformed, the relation's attributes. */
	Datum	   *row_values;
	bool	   *row_nulls;
	/* The zero-based attribute of each batch column. */
	int			ncolumns;
	AttrNumber *attnos;
	/* A row's values and NULL flags of each attribute, PACK_BATCH_ROWS rows. */
	Datum	  **values;
	bool	  **isnull;
	uint64		visible;
	uint64		rows_read;
} PackAocs;

static PackAocs *pack_aocs_create(PackState *state, EState *estate);
#endif

static Plan *pack_plan(PlannerInfo *root, RelOptInfo *rel,
					   CustomPath *best_path, List *tlist, List *clauses,
					   List *custom_plans);
static Node *pack_create_state(CustomScan *cscan);
static void pack_begin(CustomScanState *css, EState *estate, int eflags);
static TupleTableSlot *pack_exec(CustomScanState *css);
static void pack_end(CustomScanState *css);
static void pack_rescan(CustomScanState *css);
static void pack_explain(CustomScanState *css, List *ancestors,
						 ExplainState *es);

static const CustomPathMethods pack_path_methods = {
	.CustomName = "TessPack",
	.PlanCustomPath = pack_plan,
};

const CustomScanMethods tess_pack_scan_methods = {
	.CustomName = "TessPack",
	.CreateCustomScanState = pack_create_state,
};

static const CustomExecMethods pack_exec_methods = {
	.CustomName = "TessPack",
	.BeginCustomScan = pack_begin,
	.ExecCustomScan = pack_exec,
	.EndCustomScan = pack_end,
	.ReScanCustomScan = pack_rescan,
	.ExplainCustomScan = pack_explain,
};

/*
 * The scan's physical target list when the child is a sequential scan of
 * a plain table whose targets are all columns of it: planned with that
 * list the scan returns its buffer tuple slot without projecting. NIL for
 * any other child, or a table with dropped or missing columns.
 */
static List *
physical_targets(PlannerInfo *root, const Path *child)
{
	RelOptInfo *rel = child->parent;

	/* A stand-in path of a test has neither a relation nor a target. */
	if (root == NULL || rel == NULL || child->pathtarget == NULL ||
		child->pathtype != T_SeqScan || !IS_SIMPLE_REL(rel) ||
		rel->rtekind != RTE_RELATION)
		return NIL;
#if TESS_GREENGAGE
	/*
	 * Only a heap scan returns its buffer's tuples: an append-only scan
	 * would read every column for the physical list, and a column-oriented
	 * one reads just the columns its targets name.
	 */
	if (rel->relam != HEAP_TABLE_AM_OID)
		return NIL;
#endif
	foreach_ptr(Expr, expr, child->pathtarget->exprs)
	{
		Var		   *var = (Var *) expr;

		if (!IsA(expr, Var) || var->varno != rel->relid ||
			var->varattno <= 0 || var->varlevelsup != 0)
			return NIL;
	}
	return build_physical_tlist(root, rel);
}

/*
 * Whether the child is a subquery scan the pack sees through: no clauses
 * of its own, targets that are columns of the subquery, and a subquery
 * whose chosen path is a batch path, so that its batches can be forwarded.
 */
static bool
forwardable(const Path *child)
{
	const SubqueryScanPath *scan = (const SubqueryScanPath *) child;
	RelOptInfo *rel = child->parent;
	Path	   *subpath;

	if (!IsA(child, SubqueryScanPath) || rel == NULL ||
		rel->baserestrictinfo != NIL || child->pathtarget == NULL)
		return false;
	/*
	 * A projection over a batch path that projects, such as a constant
	 * among the subquery's targets, goes into that path's plan: the
	 * subplan is the batch node still.
	 */
	subpath = scan->subpath;
	if (IsA(subpath, ProjectionPath) && ((ProjectionPath *) subpath)->dummypp)
		subpath = ((ProjectionPath *) subpath)->subpath;
	if (tess_path_node(subpath) == NULL)
		return false;
	foreach_ptr(Expr, expr, child->pathtarget->exprs)
	{
		Var		   *var = (Var *) expr;

		if (!IsA(expr, Var) || var->varno != rel->relid ||
			var->varattno <= 0 || var->varlevelsup != 0)
			return false;
	}
	return true;
}

/* Whether a path is the pack node's forwarding the batches of a subquery. */
bool
tess_pack_forwards(const Path *path)
{
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);

	if (tess_path_node(path) != &tess_pack_node)
		return false;
	tess_path_get_info((const CustomPath *) path, &info);
	return info.node_data != NULL && intVal(info.node_data) == PACK_FORWARD;
}

/*
 * The path costs what its child costs and the copying of its rows:
 * tessera.pack_value_share (0.4) of cpu_operator_cost a value and one more
 * a row, as the builder took 7.8 ns a row of nine columns and 1.6 ns of
 * one where the core's aggregate above took 21 ns a row it costs 0.0225
 * (plan 4.28). A heap batch keeps one reference a row; forwarded batches
 * cost nothing.
 */
static CustomPath *
pack_wrap_rows(PlannerInfo *root, Path *child)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	List	   *physical = physical_targets(root, child);
	Path	   *scan = child;
	CustomPath *path;
	double		values = 1 + (child->pathtarget != NULL ? list_length(child->pathtarget->exprs) : 0);

	if (forwardable(child))
	{
		config.node_data = (Node *) makeInteger(PACK_FORWARD);
		values = 0;
	}
	else if (physical != NIL)
	{
		/* The caller's path keeps its target: the pack's own is that one. */
		scan = makeNode(Path);
		*scan = *child;
		scan->pathtarget = create_pathtarget(root, physical);
		config.node_data = (Node *) makeInteger(PACK_PHYSICAL_TARGETS);
		values = 1;
	}
	config.template_path = child;
	config.methods = &pack_path_methods;
	config.node = &tess_pack_node;
	config.children = list_make1(scan);
	path = tess_path_create(&config);
	path->path.total_cost += cpu_operator_cost * tess_pack_value_share * values *
		path->path.rows;
	/* Over a parallel scan, each participant packs its own rows: no shared state. */
	path->path.parallel_aware = false;
	return path;
}

/* The parent needs at most tuples_needed rows: pull no more, and tell
 * the child, so that a sort below stays a top-N sort. */
static void
pack_set_tuple_bound(CustomScanState *css, int64 tuples_needed)
{
	PackState  *state = (PackState *) css;

	state->tuples_needed = tuples_needed < 0 ? -1 : tuples_needed;
	if (state->unary != NULL)
		tess_unary_set_tuple_bound(state->unary, tuples_needed);
	else
		ExecSetTupleBound(tuples_needed, state->child);
}

const TessNode tess_pack_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_PACK_NODE_NAME,
	.wrap_rows = pack_wrap_rows,
	.set_tuple_bound = pack_set_tuple_bound,
};

static Plan *
pack_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
		  List *tlist, List *clauses, List *custom_plans)
{
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);
	Plan	   *child = linitial(custom_plans);

	tess_path_get_info(best_path, &info);
	config.methods = &tess_pack_scan_methods;
	config.scan_targetlist = child->targetlist;
	if (info.node_data != NULL && intVal(info.node_data) == PACK_FORWARD)
	{
		/*
		 * The batches are the subplan's: every one of its columns is a
		 * batch column, and a target, a column of the subquery, maps to the
		 * column of the subplan's target of that number.
		 */
		SubqueryScan *scan = castNode(SubqueryScan, child);
		Plan	   *subplan = scan->subplan;
		TessLayout	forwarded = TESS_STRUCT_INITIALIZER(TessLayout);
		int		   *map = palloc_array(int, list_length(tlist));
		int			target = 0;
		const char *kind = IsA(subplan, CustomScan) ?
			tess_plan_data_kind(((CustomScan *) subplan)->custom_private) : NULL;

		if (kind == NULL || strcmp(kind, "tessera.plan") != 0)
			elog(ERROR, "Tessera pack cannot forward the batches of a foreign plan");
		tess_plan_get_layout(subplan, &forwarded);
		foreach_ptr(TargetEntry, entry, tlist)
		{
			Var		   *var = (Var *) entry->expr;

			if (!IsA(var, Var) || var->varattno < 1 ||
				var->varattno > forwarded.ntargets)
				elog(ERROR, "Tessera pack target is not a column of the subquery");
			map[target++] = tess_layout_column(&forwarded, var->varattno - 1);
		}
		layout.ncolumns = forwarded.ncolumns;
		layout.ntargets = list_length(tlist);
		layout.target_columns = map;
		config.layout_policy = TESS_LAYOUT_EXPLICIT;
		config.explicit_layout = &layout;
		config.node_data = info.node_data;
	}
	else if (info.node_data != NULL)
	{
		/* Every relation column is a batch column; the targets map to them. */
		int		   *map = palloc_array(int, list_length(tlist));
		int			target = 0;

		foreach_ptr(TargetEntry, entry, tlist)
		{
			if (!IsA(entry->expr, Var))
				elog(ERROR, "Tessera pack target is not a column");
			map[target++] = ((Var *) entry->expr)->varattno - 1;
		}
		layout.ncolumns = list_length(child->targetlist);
		layout.ntargets = list_length(tlist);
		layout.target_columns = map;
		config.layout_policy = TESS_LAYOUT_EXPLICIT;
		config.explicit_layout = &layout;
	}
	else
	{
		/*
		 * The child was planned with its exact target list, from the same
		 * path target as this node's, so child attribute N is target N.
		 * The clauses are the relation's, which the child already evaluates.
		 */
		if (list_length(tlist) != list_length(child->targetlist))
			elog(ERROR, "Tessera pack target list does not match its child");
		config.layout_policy = TESS_LAYOUT_DENSE;
	}
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

static Node *
pack_create_state(CustomScan *cscan)
{
	PackState  *state = (PackState *)
		newNode(sizeof(PackState), T_CustomScanState);

	state->css.methods = &pack_exec_methods;
	return (Node *) state;
}

static void
pack_begin(CustomScanState *css, EState *estate, int eflags)
{
	PackState  *state = (PackState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);

	tess_node_require_forward(eflags, "Tessera pack");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_pack_node || info.nchildren != 1 ||
		info.child_names[0] != NULL)
		elog(ERROR, "Tessera pack received a foreign plan");
	state->child = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
	css->custom_ps = list_make1(state->child);
	state->layout = info.layout;
	state->tuples_needed = -1;
	if (info.node_data != NULL && intVal(info.node_data) == PACK_FORWARD)
	{
		/*
		 * The batch source is the plan under the subquery scan, or the child
		 * itself once the planner dropped a trivial subquery scan; the
		 * subquery scan, never executed, is still what a rescan goes
		 * through, so that changed parameters reach the subquery's plan.
		 */
		TessUnaryConfig config = TESS_STRUCT_INITIALIZER(TessUnaryConfig);
		PlanState  *source = state->child;

		if (IsA(source, SubqueryScanState))
			source = ((SubqueryScanState *) source)->subplan;
		config.parent_context = estate->es_query_cxt;
		config.node = css;
		config.child = source;
		config.rescan_child = state->child;
		config.layout = &info.layout;
		state->unary = tess_unary_create(&config);
		return;
	}
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   css->ss.ps.ps_ResultTupleSlot,
									   &info.layout);
#if TESS_GREENGAGE
	/* Only a dense layout maps batch column N to the child's target N. */
	if (info.node_data == NULL)
		state->aocs = pack_aocs_create(state, estate);
#endif
}

/* Freeze the parent's request; the batch size follows from it. */
static void
pack_freeze_request(PackState *state)
{
	state->request = tess_output_request(state->output);
	state->capacity = state->request->max_batch_rows > 0 ?
		Min(state->request->max_batch_rows, PACK_BATCH_ROWS) :
		PACK_BATCH_ROWS;
	if (state->request->output_mode != TESS_OUTPUT_BATCH)
		elog(ERROR, "Tessera pack requires a batch-aware parent");
}

/* The provider for this scan: kept tuples for a scan's buffer slot. */
static void
pack_create_provider(PackState *state, TupleTableSlot *slot)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;

	if (TTS_IS_BUFFERTUPLE(slot))
	{
		TessHeapBatchConfig config = TESS_STRUCT_INITIALIZER(TessHeapBatchConfig);

		config.parent_context = context;
		config.ncolumns = state->layout.ncolumns;
		config.capacity = state->capacity;
		state->heap = tess_heap_batch_create(&config);
	}
	else
	{
		TessBuilderConfig config = TESS_STRUCT_INITIALIZER(TessBuilderConfig);

		config.parent_context = context;
		config.tuple_desc = ExecGetResultType(state->child);
		config.ncolumns = state->layout.ncolumns;
		config.capacity = state->capacity;
		state->builder = tess_builder_create(&config);
	}
}

#if TESS_GREENGAGE
static void
aocs_get_datum_column(TessBatch *batch, int column, const TessRowMask *rows,
					  TessColumnPurpose purpose, TessDatumColumn *result)
{
	PackAocs   *aocs = (PackAocs *) batch->private_data;

	if (result == NULL || result->struct_size < TESS_DATUM_COLUMN_MIN_SIZE)
		elog(ERROR, "Tessera pack received an incompatible column request");
	if (column < 0 || column >= aocs->ncolumns)
		elog(ERROR, "Tessera pack column is out of range");
	result->values = aocs->values[aocs->attnos[column]];
	result->isnull = aocs->isnull[aocs->attnos[column]];
	result->nrows = batch->rows.nrows;
	if (result->struct_size >= TESS_DATUM_COLUMN_DECIMALS_SIZE)
		result->decimal_rows = NULL;
}

static const TessBatchOps aocs_batch_ops = {
	.abi_version = TESS_BATCH_OPS_ABI_VERSION,
	.struct_size = sizeof(TessBatchOps),
	.get_datum_column = aocs_get_datum_column,
};

/*
 * The block reader of the child, when it is a sequential scan of an
 * append-only table with no clauses of its own and the table's columns
 * for targets, which are the batch's columns in order; NULL otherwise.
 */
static PackAocs *
pack_aocs_create(PackState *state, EState *estate)
{
	Plan	   *plan = state->child->plan;
	Relation	relation;
	PackAocs   *aocs;
	int			natts;

	if (!IsA(plan, SeqScan) || plan->qual != NIL || plan->initPlan != NIL)
		return NULL;
	relation = ((SeqScanState *) state->child)->ss.ss_currentRelation;
	if (relation == NULL ||
		(relation->rd_rel->relam != AO_COLUMN_TABLE_AM_OID &&
		 relation->rd_rel->relam != AO_ROW_TABLE_AM_OID) ||
		list_length(plan->targetlist) != state->layout.ncolumns)
		return NULL;
	foreach_node(TargetEntry, entry, plan->targetlist)
	{
		Var		   *var = (Var *) entry->expr;

		if (!IsA(var, Var) || var->varno != ((Scan *) plan)->scanrelid ||
			var->varattno <= 0 || var->varlevelsup != 0)
			return NULL;
	}
	natts = RelationGetNumberOfAttributes(relation);
	aocs = MemoryContextAllocZero(estate->es_query_cxt, sizeof(PackAocs));
	aocs->relation = relation;
	aocs->columns = relation->rd_rel->relam == AO_COLUMN_TABLE_AM_OID;
	aocs->ncolumns = list_length(plan->targetlist);
	aocs->attnos = MemoryContextAlloc(estate->es_query_cxt,
									  sizeof(AttrNumber) * Max(aocs->ncolumns, 1));
	foreach_node(TargetEntry, entry, plan->targetlist)
		aocs->attnos[foreach_current_index(entry)] = ((Var *) entry->expr)->varattno - 1;
	/* Every attribute: with no column to read the scan picks one itself. */
	aocs->values = MemoryContextAlloc(estate->es_query_cxt, sizeof(Datum *) * natts);
	aocs->isnull = MemoryContextAlloc(estate->es_query_cxt, sizeof(bool *) * natts);
	for (int attr = 0; attr < natts; attr++)
	{
		aocs->values[attr] = MemoryContextAllocZero(estate->es_query_cxt,
													sizeof(Datum) * PACK_BATCH_ROWS);
		aocs->isnull[attr] = MemoryContextAllocZero(estate->es_query_cxt,
													sizeof(bool) * PACK_BATCH_ROWS);
	}
	if (!aocs->columns)
	{
		aocs->row_values = MemoryContextAlloc(estate->es_query_cxt, sizeof(Datum) * Max(natts, 1));
		aocs->row_nulls = MemoryContextAlloc(estate->es_query_cxt, sizeof(bool) * Max(natts, 1));
	}
	aocs->batch.abi_version = TESS_BATCH_ABI_VERSION;
	aocs->batch.struct_size = sizeof(TessBatch);
	aocs->batch.ops = &aocs_batch_ops;
	aocs->batch.private_data = aocs;
	aocs->batch.rows.bits = &aocs->visible;
	aocs->batch.table_oid = RelationGetRelid(relation);
	return aocs;
}

/* Begin the scan of the batch columns' attributes, as the core's would. */
static void
pack_aocs_begin(PackAocs *aocs, EState *estate)
{
	int			natts = RelationGetNumberOfAttributes(aocs->relation);
	bool	   *proj;

	if (!aocs->columns)
	{
		aocs->row_scan = appendonly_beginscan(aocs->relation, estate->es_snapshot, 0, NULL,
											  NULL, SO_TYPE_SEQSCAN | SO_ALLOW_STRAT |
											  SO_ALLOW_SYNC | SO_ALLOW_PAGEMODE);
		return;
	}
	proj = palloc0(sizeof(bool) * natts);

	for (int column = 0; column < aocs->ncolumns; column++)
		proj[aocs->attnos[column]] = true;
	aocs->scan = aocs_beginscan(aocs->relation, estate->es_snapshot,
								aocs->ncolumns > 0 ? proj : NULL,
								aocs->ncolumns > 0 ? AOCS_PROJ_SOME : AOCS_PROJ_ANY,
								SO_TYPE_SEQSCAN | SO_ALLOW_STRAT | SO_ALLOW_SYNC |
								SO_ALLOW_PAGEMODE);
	pfree(proj);
}

/* The next batch of at most limit rows of the table, NULL at its end. */
static TessBatch *
pack_aocs_next(PackState *state, int limit)
{
	PackAocs   *aocs = state->aocs;

	if (aocs->scan == NULL && aocs->row_scan == NULL)
		pack_aocs_begin(aocs, state->css.ss.ps.state);
	for (;;)
	{
		int			nrows;

		CHECK_FOR_INTERRUPTS();
		if (aocs->columns)
			nrows = aocs_getnext_batch(aocs->scan, RelationGetDescr(aocs->relation),
									   Min(limit, PACK_BATCH_ROWS),
									   aocs->values, aocs->isnull, &aocs->visible);
		else
			nrows = appendonly_getnext_batch(aocs->row_scan, RelationGetDescr(aocs->relation),
											 Min(limit, PACK_BATCH_ROWS), aocs->ncolumns,
											 aocs->attnos, aocs->row_values, aocs->row_nulls,
											 aocs->values, aocs->isnull, &aocs->visible);
		if (nrows == 0)
			return NULL;
		aocs->rows_read += nrows;
		/* Rows of a batch the snapshot does not see are deleted ones. */
		if (aocs->visible == 0)
			continue;
		aocs->batch.rows.nrows = nrows;
		return &aocs->batch;
	}
}
#endif

static TupleTableSlot *
pack_exec(CustomScanState *css)
{
	PackState  *state = (PackState *) css;
	TessBatch  *batch;
	int			limit;

	if (state->unary != NULL)
	{
		TupleTableSlot *slot = tess_unary_exec(state->unary);

		if (state->request == NULL)
		{
			state->request = tess_unary_request(state->unary);
			if (state->request->output_mode != TESS_OUTPUT_BATCH)
				elog(ERROR, "Tessera pack requires a batch-aware parent");
		}
		return slot;
	}
	if (state->request == NULL)
		pack_freeze_request(state);
	/* Refuses while the parent has not finished the previous batch. */
	tess_output_release(state->output);
	if (state->exhausted)
		return NULL;
	/* A bounded parent never gets more rows than it asked for. */
	limit = state->capacity;
	if (state->tuples_needed >= 0)
	{
		if (state->produced >= state->tuples_needed)
			return NULL;
		limit = (int) Min((int64) limit, state->tuples_needed - state->produced);
	}
#if TESS_GREENGAGE
	if (state->aocs != NULL)
	{
		batch = pack_aocs_next(state, limit);
		if (batch == NULL)
		{
			state->exhausted = true;
			return NULL;
		}
		state->produced += tess_row_mask_count(&batch->rows);
		state->batches++;
		return tess_output_publish(state->output, batch);
	}
#endif
	if (state->heap != NULL)
		tess_heap_batch_reset(state->heap);
	else if (state->builder != NULL)
		tess_builder_reset(state->builder);
	while (limit > 0)
	{
		TupleTableSlot *slot = ExecProcNode(state->child);

		if (TupIsNull(slot))
		{
			state->exhausted = true;
			break;
		}
		if (state->heap == NULL && state->builder == NULL)
			pack_create_provider(state, slot);
		if (state->heap != NULL)
			tess_heap_batch_append_slot(state->heap, slot);
		else
			tess_builder_append_slot(state->builder, slot);
		limit--;
	}
	if (state->heap != NULL)
		batch = tess_heap_batch_finish(state->heap, InvalidOid);
	else if (state->builder != NULL)
		batch = tess_builder_finish(state->builder, InvalidOid);
	else
		batch = NULL;
	if (batch == NULL)
		return NULL;
	state->produced += tess_row_mask_count(&batch->rows);
	state->batches++;
	return tess_output_publish(state->output, batch);
}

static void
pack_end(CustomScanState *css)
{
	PackState  *state = (PackState *) css;

	if (state->unary != NULL)
		tess_unary_end(state->unary);
	else
		tess_output_end(state->output);
#if TESS_GREENGAGE
	/* Before the child, which closes the relation the scan reads. */
	if (state->aocs != NULL && state->aocs->scan != NULL)
		aocs_endscan(state->aocs->scan);
	if (state->aocs != NULL && state->aocs->row_scan != NULL)
		appendonly_endscan(state->aocs->row_scan);
#endif
	ExecEndNode(state->child);
}

static void
pack_rescan(CustomScanState *css)
{
	PackState  *state = (PackState *) css;

	if (state->unary != NULL)
	{
		tess_unary_rescan(state->unary);
		return;
	}
	tess_output_clear(state->output);
#if TESS_GREENGAGE
	if (state->aocs != NULL && state->aocs->scan != NULL)
		aocs_rescan(state->aocs->scan);
	if (state->aocs != NULL && state->aocs->row_scan != NULL)
		appendonly_rescan(state->aocs->row_scan, NULL, false, false, false, false);
#endif
	/* Rows of a core plan: no batch input to start anew. */
	tess_rescan_child(&css->ss.ps, state->child, NULL);
	state->exhausted = false;
	/* The rows of this scan, for its bound; EXPLAIN's batches total every scan. */
	state->produced = 0;
}

static void
pack_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	PackState  *state = (PackState *) css;

	/* How the rows became batches: VERBOSE only. */
	if (!es->verbose)
		return;
	if (state->unary != NULL)
	{
		ExplainPropertyText("Rows Kept As", "forwarded batches", es);
		if (es->analyze)
			ExplainPropertyInteger("Batches", NULL,
								   tess_unary_stats(state->unary)->input_batches,
								   es);
		return;
	}
	/* The parent's request, and so the size, is known once executed. */
	if (state->request != NULL)
		ExplainPropertyInteger("Batch Size", NULL, state->capacity, es);
#if TESS_GREENGAGE
	if (state->aocs != NULL)
		ExplainPropertyText("Rows Kept As",
							state->aocs->columns ? "ao_column blocks" : "ao_row blocks", es);
#endif
	if (state->heap != NULL || state->builder != NULL)
		ExplainPropertyText("Rows Kept As",
							state->heap != NULL ? "heap tuples" : "copies", es);
	if (!es->analyze)
		return;
	ExplainPropertyInteger("Batches", NULL, state->batches, es);
	if (state->heap != NULL)
	{
		const TessHeapBatchStats *stats = tess_heap_batch_stats(state->heap);

		ExplainPropertyInteger("Deformed Datums", NULL, stats->deformed_datums, es);
		ExplainPropertyInteger("Restarted Datums", NULL, stats->restarted_datums, es);
		if (stats->copied_tuples > 0)
			ExplainPropertyInteger("Copied Tuples", NULL, stats->copied_tuples, es);
	}
}

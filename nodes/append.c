#include "postgres.h"

#include "access/htup_details.h"
#include "access/parallel.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/restrictinfo.h"
#include "storage/lwlock.h"

#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * TessAppend stands in for the core's Append under a batch parent
 * (docs/nodes.md): it reads its batch children in turn and gives its
 * parent each child's batches as they are, through a batch of its own
 * whose columns are the child's, renumbered by the child's layout. Under
 * the core's Append each child's batches became rows, which a pack node
 * made batches again: 13 ms of an aggregate's 31 over two filtered scans
 * of a million rows each. The node's path is built only where a batch
 * parent asks for a batch child over an Append (tess_batch_input_path),
 * so that a row-wise parent keeps the core's Append. In a parallel plan
 * the node shares the children out as the core's Parallel Append does: a
 * child that is not partial goes to one participant, a partial one to any
 * that comes while it has work, each dividing its pages with the others.
 */

/* The core's cost of a row through an Append (costsize.c), which the node saves. */
#define APPEND_CPU_COST_MULTIPLIER 0.5

/* The node's counter summed over the participants: the batches given out. */
#define APPEND_NCOUNTERS 1

/*
 * The children shared out in a parallel plan, after the counters in the
 * node's chunk, as the core's ParallelAppendState: the child a worker
 * looks at first, and the children that need no more participants.
 */
typedef struct AppendShared
{
	LWLock		lock;
	int			next_plan;
	bool		finished[FLEXIBLE_ARRAY_MEMBER];
} AppendShared;

typedef struct TessAppendState
{
	CustomScanState css;
	int			nchildren;
	PlanState **children;
	TessInput **inputs;
	/* Each child's layout: the column of each of its targets. */
	TessLayout *layouts;
	int			ncolumns;
	/* The children before this one are not partial. */
	int			first_partial;
	/* The child being read, -1 before the first, and its batch given out. */
	int			current;
	bool		done;
	TessBatch  *child_batch;
	/* In a parallel plan: the children shared out and the counters. */
	AppendShared *shared;
	TessSharedStats *stats;
	TessOutput *output;
	bool		requested;
	/* The batch given out: the child's rows, its columns renumbered. */
	TessBatch	batch;
	bool		published;
	int			served;
	uint64		batches;
} TessAppendState;

static const CustomExecMethods append_exec_methods;

static Plan *append_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
						 List *tlist, List *clauses, List *custom_plans);

static const CustomPathMethods append_path_methods = {
	.CustomName = "TessAppend",
	.PlanCustomPath = append_plan,
};

/* ---------------------------------------------------------------- planning */

static bool
contains_param(Node *node, void *context)
{
	if (node == NULL)
		return false;
	if (IsA(node, Param))
		return true;
	return expression_tree_walker(node, contains_param, context);
}

static bool
contains_expr(Node *node, void *context)
{
	if (node == NULL)
		return false;
	if (equal(node, context))
		return true;
	return expression_tree_walker(node, contains_expr, context);
}

/*
 * Whether the core might prune the partitions of rel while executing: a
 * clause over a partition key whose value is known only then, through a
 * parameter or a function that is not immutable. The node reads every
 * child, which is correct, as each child evaluates the clauses, but would
 * lose the pruning.
 */
static bool
prunes_at_execution(RelOptInfo *rel)
{
	if (!enable_partition_pruning || rel->part_scheme == NULL || rel->partexprs == NULL)
		return false;
	foreach_node(RestrictInfo, rinfo, rel->baserestrictinfo)
	{
		Node	   *clause = (Node *) rinfo->clause;

		if (!contains_param(clause, NULL) && !contain_mutable_functions(clause))
			continue;
		for (int key = 0; key < rel->part_scheme->partnatts; key++)
		{
			foreach_ptr(Node, expr, rel->partexprs[key])
			{
				if (contains_expr(clause, expr))
					return true;
			}
		}
	}
	return false;
}

/*
 * Whether a batch child only packs the rows of a core path: a pack over a
 * subquery planned as a batch path forwards its batches.
 */
static bool
packs_rows(Path *child)
{
	return tess_path_node(child) == &tess_pack_node && !tess_pack_forwards(child);
}

/*
 * The node's path in place of an Append of a base relation's children: a
 * partitioned table, an inheritance tree, a UNION ALL the planner made a
 * relation of; serial, partial, or parallel-aware as a Parallel Append,
 * whose children before first_partial_path are not partial. When every
 * child has a batch path and one of them at least does more than pack
 * rows; NULL otherwise. The Append of a set operation's own relation,
 * whose targets are Vars of no relation, stays the core's.
 */
static CustomPath *
append_wrap(PlannerInfo *root, Path *path)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	AppendPath *append = (AppendPath *) path;
	RelOptInfo *rel = path->parent;
	List	   *children = NIL;
	bool		batches = false;
	CustomPath *built;
	Cost		saved;

	if (!*tess_runtime_api()->settings->enable || !IsA(path, AppendPath) ||
		rel == NULL || !IS_SIMPLE_REL(rel) || root->parse->commandType != CMD_SELECT ||
		root->parse->rowMarks != NIL || path->param_info != NULL ||
		list_length(append->subpaths) < 2 || path->pathtarget == NULL ||
		list_length(path->pathtarget->exprs) > MaxTupleAttributeNumber ||
		prunes_at_execution(rel))
		return NULL;
	foreach_ptr(Path, subpath, append->subpaths)
	{
		Path	   *child = tess_batch_input_path(root, subpath);

		if (child == NULL)
			return NULL;
		if (!packs_rows(child))
			batches = true;
		children = lappend(children, child);
	}
	if (!batches)
		return NULL;
	config.template_path = path;
	config.methods = &append_path_methods;
	config.node = &tess_append_node;
	config.children = children;
	config.node_data = (Node *) makeInteger(path->parallel_aware ?
											append->first_partial_path : 0);
	built = tess_path_create(&config);
	saved = APPEND_CPU_COST_MULTIPLIER * cpu_tuple_cost * path->rows;
	built->path.total_cost = Max(built->path.startup_cost, built->path.total_cost - saved);
	return built;
}

/*
 * A column per target. The clauses are the parent relation's, which the
 * core translated to every child: the children evaluate them.
 */
static Plan *
append_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path, List *tlist,
			List *clauses, List *custom_plans)
{
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanWriter *writer;

	tess_path_get_info(best_path, &info);
	for (int index = 0; index < list_length(custom_plans); index++)
	{
		TessPlanChild child = TESS_STRUCT_INITIALIZER(TessPlanChild);

		if (!tess_plan_child(best_path, custom_plans, index, &child))
			elog(ERROR, "TessAppend expected batch children");
		if (child.layout.ntargets != list_length(tlist))
			elog(ERROR, "TessAppend child %d has %d targets, not %d", index,
				 child.layout.ntargets, list_length(tlist));
	}
	writer = tess_plan_writer_create(TESS_APPEND_DATA, TESS_APPEND_DATA_VERSION);
	tess_plan_write_int(writer, "children", list_length(custom_plans));
	tess_plan_write_int(writer, "first_partial", intVal(info.node_data));
	config.methods = &tess_append_scan_methods;
	config.layout_policy = TESS_LAYOUT_DENSE;
	config.scanrelid = 0;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

/* ---------------------------------------------------------------- execution */

static Node *
append_create_state(CustomScan *cscan)
{
	TessAppendState *state = (TessAppendState *) newNode(sizeof(TessAppendState),
														 T_CustomScanState);

	state->css.methods = &append_exec_methods;
	return (Node *) state;
}

/* A column of the batch given out: the child's column of that target. */
static void
append_get_column(TessBatch *batch, int column, const TessRowMask *rows,
				  TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessAppendState *state = (TessAppendState *) batch->private_data;
	TessBatch  *child = state->child_batch;

	if (column < 0 || column >= state->ncolumns || child == NULL)
		elog(ERROR, "TessAppend has no column %d", column);
	child->ops->get_datum_column(child,
								 tess_layout_column(&state->layouts[state->current], column),
								 rows, purpose, result);
}

static const TessBatchOps append_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = append_get_column,
};

static void
append_begin(CustomScanState *css, EState *estate, int eflags)
{
	TessAppendState *state = (TessAppendState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TessPlanReader *reader;
	int			index = 0;

	/* The planner puts Material above a batch subtree for these. */
	if (eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK))
		elog(ERROR, "TessAppend supports neither backward scan nor mark/restore");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_append_node)
		elog(ERROR, "TessAppend received a foreign plan");
	reader = tess_plan_reader_create((List *) info.node_data, TESS_APPEND_DATA,
									 TESS_APPEND_DATA_VERSION);
	state->nchildren = tess_plan_read_int(reader, "children");
	state->first_partial = tess_plan_read_int(reader, "first_partial");
	tess_plan_reader_finish(reader);
	state->current = -1;
	if (state->nchildren != info.nchildren || state->nchildren < 1 ||
		state->first_partial < 0 || state->first_partial > state->nchildren)
		elog(ERROR, "TessAppend received a foreign plan");
	state->ncolumns = css->ss.ps.ps_ResultTupleSlot->tts_tupleDescriptor->natts;
	state->children = palloc_array(PlanState *, state->nchildren);
	state->inputs = palloc_array(TessInput *, state->nchildren);
	state->layouts = palloc_array(TessLayout, state->nchildren);
	foreach_ptr(Plan, plan, cscan->custom_plans)
	{
		if (info.child_names[index] == NULL)
			elog(ERROR, "TessAppend expected batch children");
		state->children[index] = ExecInitNode(plan, estate, eflags);
		css->custom_ps = lappend(css->custom_ps, state->children[index]);
		state->inputs[index] = tess_input_create(estate->es_query_cxt,
												 state->children[index]);
		state->layouts[index] = *tess_input_layout(state->inputs[index]);
		if (state->layouts[index].ntargets != state->ncolumns)
			elog(ERROR, "TessAppend child %d has %d targets, not %d", index,
				 state->layouts[index].ntargets, state->ncolumns);
		index++;
	}
	state->batch.abi_version = TESS_BATCH_ABI_VERSION;
	state->batch.struct_size = sizeof(TessBatch);
	state->batch.ops = &append_batch_ops;
	state->batch.private_data = state;
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   css->ss.ps.ps_ResultTupleSlot, &info.layout);
}

/* The children's columns of the targets in columns. */
static Bitmapset *
child_columns(const TessLayout *layout, const Bitmapset *columns)
{
	Bitmapset  *result = NULL;
	int			column = -1;

	while ((column = bms_next_member(columns, column)) >= 0)
		result = bms_add_member(result, tess_layout_column(layout, column));
	return result;
}

/*
 * Every child gets the parent's request in its own columns; a row-wise
 * parent is served every column, as the output helper reads the targets.
 */
static void
send_requests(TessAppendState *state)
{
	const TessRequest *request = tess_output_request(state->output);
	Bitmapset  *all = NULL;

	if (request->output_mode == TESS_OUTPUT_ROWS)
		all = bms_add_range(NULL, 0, state->ncolumns - 1);
	for (int index = 0; index < state->nchildren; index++)
	{
		TessRequest child = TESS_STRUCT_INITIALIZER(TessRequest);
		const TessLayout *layout = &state->layouts[index];

		if (all != NULL)
			child.projection_columns = child_columns(layout, all);
		else
		{
			child.filter_columns = child_columns(layout, request->filter_columns);
			child.projection_columns = child_columns(layout, request->projection_columns);
		}
		child.output_mode = TESS_OUTPUT_BATCH;
		child.max_batch_rows = request->max_batch_rows;
		tess_input_set_request(state->inputs[index], &child);
	}
	state->requested = true;
}

/*
 * The leader's next child in a parallel plan, as the core's
 * choose_next_subplan_for_leader: from the last child down, so that the
 * workers, which start from the first, take the costly children that are
 * not partial and the leader the partial ones, and can stop gathering
 * early. A child that is not partial is finished once chosen.
 */
static bool
choose_for_leader(TessAppendState *state)
{
	AppendShared *shared = state->shared;

	LWLockAcquire(&shared->lock, LW_EXCLUSIVE);
	if (state->current >= 0)
		shared->finished[state->current] = true;
	else
		state->current = state->nchildren - 1;
	while (shared->finished[state->current])
	{
		if (state->current == 0)
		{
			shared->next_plan = -1;
			state->current = -1;
			LWLockRelease(&shared->lock);
			return false;
		}
		state->current--;
	}
	if (state->current < state->first_partial)
		shared->finished[state->current] = true;
	LWLockRelease(&shared->lock);
	return true;
}

/* The child after index, or -1: every child is valid, none pruned. */
static int
next_child(TessAppendState *state, int index)
{
	return index + 1 < state->nchildren ? index + 1 : -1;
}

/*
 * A worker's next child in a parallel plan, as the core's
 * choose_next_subplan_for_worker: the first that is not finished from
 * next_plan on, going round to the first partial child; next_plan moves
 * past it.
 */
static bool
choose_for_worker(TessAppendState *state)
{
	AppendShared *shared = state->shared;
	int			start;

	LWLockAcquire(&shared->lock, LW_EXCLUSIVE);
	if (state->current >= 0)
		shared->finished[state->current] = true;
	if (shared->next_plan < 0)
	{
		LWLockRelease(&shared->lock);
		return false;
	}
	start = shared->next_plan;
	while (shared->finished[shared->next_plan])
	{
		int			next = next_child(state, shared->next_plan);

		if (next >= 0)
			shared->next_plan = next;
		else if (start > state->first_partial)
		{
			next = next_child(state, state->first_partial - 1);
			shared->next_plan = next < 0 ? start : next;
		}
		else
			shared->next_plan = start;
		if (shared->next_plan == start)
		{
			shared->next_plan = -1;
			LWLockRelease(&shared->lock);
			return false;
		}
	}
	state->current = shared->next_plan;
	shared->next_plan = next_child(state, shared->next_plan);
	if (shared->next_plan < 0)
		shared->next_plan = next_child(state, state->first_partial - 1);
	if (state->current < state->first_partial)
		shared->finished[state->current] = true;
	LWLockRelease(&shared->lock);
	return true;
}

/* The next child to read, the children in turn without shared memory. */
static bool
choose_next(TessAppendState *state)
{
	if (state->shared != NULL)
		return IsParallelWorker() ? choose_for_worker(state) : choose_for_leader(state);
	state->current++;
	return state->current < state->nchildren;
}

/* The next child batch with rows; false at the end. */
static bool
append_next(TessAppendState *state)
{
	/* The batch given out was consumed: its child may go on. */
	if (state->child_batch != NULL)
	{
		tess_input_finish(state->inputs[state->current]);
		state->child_batch = NULL;
	}
	if (state->done)
		return false;
	if (state->current < 0 && !choose_next(state))
	{
		state->done = true;
		return false;
	}
	for (;;)
	{
		TessInput  *input = state->inputs[state->current];
		TessBatch  *batch = tess_input_next(input);

		if (batch == NULL)
		{
			if (!choose_next(state))
			{
				state->done = true;
				return false;
			}
			continue;
		}
		if (tess_row_mask_count(&batch->rows) == 0)
		{
			tess_input_finish(input);
			continue;
		}
		state->child_batch = batch;
		state->batch.rows = batch->rows;
		state->batch.table_oid = batch->table_oid;
		state->batches++;
		return true;
	}
}

static TupleTableSlot *
append_exec(CustomScanState *css)
{
	TessAppendState *state = (TessAppendState *) css;
	bool		rows;

	CHECK_FOR_INTERRUPTS();
	if (!state->requested)
		send_requests(state);
	rows = tess_output_request(state->output)->output_mode == TESS_OUTPUT_ROWS;
	if (rows && state->published)
	{
		int			next = tess_row_mask_next(&state->batch.rows, state->served);

		if (next >= 0)
		{
			state->served = next;
			return tess_output_select(state->output, next);
		}
		tess_output_finish(state->output);
	}
	tess_output_release(state->output);
	state->published = false;
	if (!append_next(state))
		return NULL;
	state->published = true;
	state->served = tess_row_mask_next(&state->batch.rows, -1);
	return tess_output_publish(state->output, &state->batch);
}

static void
append_end(CustomScanState *css)
{
	TessAppendState *state = (TessAppendState *) css;

	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	tess_output_end(state->output);
	for (int index = 0; index < state->nchildren; index++)
		ExecEndNode(state->children[index]);
}

static void
append_rescan(CustomScanState *css)
{
	TessAppendState *state = (TessAppendState *) css;

	tess_output_clear(state->output);
	state->published = false;
	state->child_batch = NULL;
	for (int index = 0; index < state->nchildren; index++)
	{
		/* The core passes changed parameters to outer and inner plans only. */
		if (css->ss.ps.chgParam != NULL)
			UpdateChangedParamSet(state->children[index], css->ss.ps.chgParam);
		ExecReScan(state->children[index]);
		tess_input_rescan(state->inputs[index]);
	}
	state->current = -1;
	state->done = false;
	state->batches = 0;
}

/* The batches given out: every participant's in a parallel plan. */
static void
append_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	TessAppendState *state = (TessAppendState *) css;
	const uint64 *totals = NULL;

	if (!es->analyze)
		return;
	if (state->stats != NULL)
		totals = tess_shared_stats_totals(state->stats);
	ExplainPropertyInteger("Batches", NULL, totals != NULL ? totals[0] : state->batches, es);
}

/*
 * A parallel plan: the node's chunk holds the counters' rows, then the
 * children shared out. The leader lays both out, a worker attaches.
 */
static Size
shared_size(TessAppendState *state)
{
	return MAXALIGN(offsetof(AppendShared, finished) + sizeof(bool) * state->nchildren);
}

static Size
append_estimate_dsm(CustomScanState *css, ParallelContext *pcxt)
{
	TessAppendState *state = (TessAppendState *) css;

	return add_size(tess_shared_stats_estimate(APPEND_NCOUNTERS, pcxt->nworkers),
					shared_size(state));
}

static void
reset_shared(TessAppendState *state)
{
	state->shared->next_plan = 0;
	memset(state->shared->finished, 0, sizeof(bool) * state->nchildren);
}

static void
append_initialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate)
{
	TessAppendState *state = (TessAppendState *) css;

	/* A Gather a limit above shut down sets up anew when rescanned. */
	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	state->stats = tess_shared_stats_init(css->ss.ps.state->es_query_cxt, coordinate,
										  APPEND_NCOUNTERS, pcxt->nworkers, pcxt->seg);
	state->shared = (AppendShared *) ((char *) coordinate +
									  tess_shared_stats_size(coordinate));
	LWLockInitialize(&state->shared->lock, LWTRANCHE_PARALLEL_APPEND);
	reset_shared(state);
}

static void
append_reinitialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate)
{
	TessAppendState *state = (TessAppendState *) css;

	tess_shared_stats_reset(state->stats);
	reset_shared(state);
}

static void
append_initialize_worker(CustomScanState *css, shm_toc *toc, void *coordinate)
{
	TessAppendState *state = (TessAppendState *) css;

	state->stats = tess_shared_stats_attach(css->ss.ps.state->es_query_cxt, coordinate,
											ParallelWorkerNumber + 1);
	state->shared = (AppendShared *) ((char *) coordinate +
									  tess_shared_stats_size(coordinate));
}

static void
append_shutdown(CustomScanState *css)
{
	TessAppendState *state = (TessAppendState *) css;
	uint64		values[APPEND_NCOUNTERS] = {state->batches};

	if (state->stats != NULL)
		tess_shared_stats_store(state->stats, values);
}

/*
 * As ExecSetTupleBound for an Append: any child may give all the rows the
 * parent needs, so every child gets the bound.
 */
static void
append_set_tuple_bound(CustomScanState *css, int64 tuples_needed)
{
	TessAppendState *state = (TessAppendState *) css;

	for (int index = 0; index < state->nchildren; index++)
	{
		PlanState  *child = state->children[index];
		const TessNode *node = tess_batch_node_of(child);

		if (node != NULL && TESS_ABI_HAS_FIELD(node, TessNode, set_tuple_bound) &&
			node->set_tuple_bound != NULL)
			node->set_tuple_bound((CustomScanState *) child, tuples_needed);
		else
			ExecSetTupleBound(tuples_needed, child);
	}
}

static const CustomExecMethods append_exec_methods = {
	.CustomName = "TessAppend",
	.BeginCustomScan = append_begin,
	.ExecCustomScan = append_exec,
	.EndCustomScan = append_end,
	.ReScanCustomScan = append_rescan,
	.ExplainCustomScan = append_explain,
	.EstimateDSMCustomScan = append_estimate_dsm,
	.InitializeDSMCustomScan = append_initialize_dsm,
	.ReInitializeDSMCustomScan = append_reinitialize_dsm,
	.InitializeWorkerCustomScan = append_initialize_worker,
	.ShutdownCustomScan = append_shutdown,
};

const CustomScanMethods tess_append_scan_methods = {
	.CustomName = "TessAppend",
	.CreateCustomScanState = append_create_state,
};

const TessNode tess_append_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_APPEND_NODE_NAME,
	.set_tuple_bound = append_set_tuple_bound,
	.wrap_append = append_wrap,
};

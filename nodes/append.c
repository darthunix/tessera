#include "postgres.h"

#include "access/htup_details.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/restrictinfo.h"

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
 * so that a row-wise parent keeps the core's Append.
 */

/* The core's cost of a row through an Append (costsize.c), which the node saves. */
#define APPEND_CPU_COST_MULTIPLIER 0.5

typedef struct TessAppendState
{
	CustomScanState css;
	int			nchildren;
	PlanState **children;
	TessInput **inputs;
	/* Each child's layout: the column of each of its targets. */
	TessLayout *layouts;
	int			ncolumns;
	/* The child being read, and its batch given out. */
	int			current;
	TessBatch  *child_batch;
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

/*
 * Whether the core would prune the partitions of rel while executing, by a
 * clause of a parameter or of a function whose value is known only then:
 * the node reads every child, which is correct, as each child evaluates
 * the clauses, but loses the pruning.
 */
static bool
prunes_at_execution(RelOptInfo *rel)
{
	List	   *clauses;

	if (!enable_partition_pruning || rel->part_scheme == NULL)
		return false;
	clauses = extract_actual_clauses(rel->baserestrictinfo, false);
	return contains_param((Node *) clauses, NULL) ||
		contain_mutable_functions((Node *) clauses);
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
 * relation of. Serial or partial without parallel awareness, when every
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
		root->parse->rowMarks != NIL || path->param_info != NULL || path->parallel_aware ||
		list_length(append->subpaths) < 2 || path->pathtarget == NULL ||
		list_length(path->pathtarget->exprs) == 0 ||
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
	TessPlanWriter *writer;

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
	tess_plan_reader_finish(reader);
	if (state->nchildren != info.nchildren || state->nchildren < 1)
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

/* The next child batch with rows, the children in turn; false at the end. */
static bool
append_next(TessAppendState *state)
{
	/* The batch given out was consumed: its child may go on. */
	if (state->child_batch != NULL)
	{
		tess_input_finish(state->inputs[state->current]);
		state->child_batch = NULL;
	}
	while (state->current < state->nchildren)
	{
		TessInput  *input = state->inputs[state->current];
		TessBatch  *batch = tess_input_next(input);

		if (batch == NULL)
		{
			state->current++;
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
	return false;
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
	state->current = 0;
	state->batches = 0;
}

static void
append_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	TessAppendState *state = (TessAppendState *) css;

	if (es->analyze)
		ExplainPropertyInteger("Batches", NULL, state->batches, es);
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

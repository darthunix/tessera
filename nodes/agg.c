#include "postgres.h"

#include "catalog/pg_aggregate.h"
#include "catalog/pg_type_d.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "common/int.h"
#include "executor/executor.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/planner.h"
#include "optimizer/tlist.h"
#include "utils/fmgroids.h"
#include "utils/regproc.h"

#include "tessera/expr.h"
#include "tessera/function.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * TessAgg computes the aggregates of a query without GROUP BY over the
 * batches of a batch child and returns the one result row: it stands in
 * for the core's plain Aggregate above TessFilter, TessHeapScan or
 * TessPack, so that no row is handed up one at a time. Each aggregate is
 * computed per batch by the registered batch function of its aggregate
 * (tessera/function.h, kind TESS_FUNCTION_AGGREGATE) and the partials are
 * combined here, with the overflow check the core's transition would
 * make. See docs/nodes.md.
 */
#define AGG_COST_FACTOR 0.9

/* How the partials of an aggregate combine, and what an empty input gives. */
typedef enum AggKind
{
	AGG_COUNT,					/* int8 sum of the partials, 0 without any */
	AGG_SUM,					/* int8 sum of the partials, NULL without any */
	AGG_MIN,					/* the least int4 partial, NULL without any */
	AGG_MAX						/* the greatest int4 partial, NULL without any */
} AggKind;

typedef struct AggValue
{
	AggKind		kind;
	const TessFunction *function;
	/* The argument over the child's batch, or NULL for count(*). */
	TessExpr   *expr;
	int64		total;
	int32		extreme;
	bool		has_value;
} AggValue;

typedef struct TessAggState
{
	CustomScanState css;
	PlanState  *child;
	TessInput  *input;
	TessOutput *output;
	TessBuilder *builder;
	AggValue   *values;
	int			nvalues;
	/* Written by a batch function on failure only. */
	TessStatus	status;
	/* The row was returned; the next call ends the scan. */
	bool		done;
	uint64		batches;
	uint64		rows;
} TessAggState;

static const CustomExecMethods agg_exec_methods;
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
			return AGG_MIN;
		case F_MAX_INT4:
			return AGG_MAX;
		default:
			return -1;
	}
}

/* The aggregated argument, or NULL for count(*). */
static Node *
aggregate_argument(const Aggref *agg)
{
	return agg->args == NIL ? NULL :
		(Node *) ((TargetEntry *) linitial(agg->args))->expr;
}

/*
 * Whether the node computes this aggregate: a plain call of an aggregate
 * the node combines and the registry implements over batches, with no
 * argument for count(*) or one int4 batch expression, a column or a
 * chain of registered calls over it.
 */
static bool
aggregate_supported(const Aggref *agg)
{
	const TessFunction *function;
	Node	   *argument;

	if (agg->agglevelsup != 0 || agg->aggkind != AGGKIND_NORMAL ||
		agg->aggsplit != AGGSPLIT_SIMPLE || agg->aggorder != NIL ||
		agg->aggdistinct != NIL || agg->aggfilter != NULL ||
		agg->aggdirectargs != NIL || agg->aggvariadic ||
		aggregate_kind(agg->aggfnoid) < 0)
		return false;
	function = tess_runtime_api()->functions->find(agg->aggfnoid);
	if (function == NULL || function->kind != TESS_FUNCTION_AGGREGATE)
		return false;
	if (agg->aggfnoid == F_COUNT_)
		return agg->aggstar && agg->args == NIL;
	argument = aggregate_argument(agg);
	return list_length(agg->args) == 1 && exprType(argument) == INT4OID &&
		tess_expr_supports_value(argument, 0);
}

/* Whether the child's target has every column the arguments read. */
static bool
arguments_available(const List *aggregates, const Path *child)
{
	foreach_ptr(TargetEntry, entry, aggregates)
	{
		Node	   *argument = aggregate_argument((Aggref *) entry->expr);
		List	   *vars = pull_var_clause(argument, 0);

		foreach_ptr(Var, var, vars)
		{
			if (!list_member(child->pathtarget->exprs, var))
				return false;
		}
	}
	return true;
}

/*
 * The distinct aggregates of the query's target and HAVING as a flat
 * target list, the scan tuple of the node, when every one is supported;
 * NIL otherwise. Expressions above the aggregates are left to the plan's
 * projection and qual over that tuple.
 */
static List *
collect_aggregates(PlannerInfo *root, RelOptInfo *output_rel)
{
	List	   *tlist = NIL;
	List	   *found;

	found = pull_var_clause((Node *) list_make2(output_rel->reltarget->exprs,
												root->parse->havingQual),
							PVC_INCLUDE_AGGREGATES);
	foreach_ptr(Node, node, found)
	{
		if (!IsA(node, Aggref) || !aggregate_supported((Aggref *) node))
			return NIL;
		tlist = add_to_flat_tlist(tlist, list_make1(node));
	}
	return tlist;
}

/* The queries the node handles: plain aggregation of a single result row. */
static bool
query_supported(PlannerInfo *root, RelOptInfo *input_rel, RelOptInfo *output_rel)
{
	Query	   *parse = root->parse;

	return parse->hasAggs && parse->groupClause == NIL &&
		parse->groupingSets == NIL && !parse->hasWindowFuncs &&
		output_rel->reloptkind == RELOPT_UPPER_REL && !IS_DUMMY_REL(input_rel);
}

/*
 * The node's path in place of each of the core's plain aggregate paths
 * whose input can be read in batches: the same planner properties and
 * rows, a lower cost, the batch child and the aggregates it computes.
 */
static void
create_upper_paths(PlannerInfo *root, UpperRelationKind stage,
				   RelOptInfo *input_rel, RelOptInfo *output_rel, void *extra)
{
	List	   *aggregates;
	List	   *templates = NIL;

	if (previous_create_upper_paths_hook != NULL)
		previous_create_upper_paths_hook(root, stage, input_rel, output_rel,
										 extra);
	if (!*tess_runtime_api()->settings->enable ||
		stage != UPPERREL_GROUP_AGG ||
		!query_supported(root, input_rel, output_rel))
		return;
	aggregates = collect_aggregates(root, output_rel);
	if (aggregates == NIL)
		return;
	/* add_path changes the list: the candidates are taken first. */
	foreach_ptr(Path, path, output_rel->pathlist)
	{
		if (IsA(path, AggPath) &&
			((AggPath *) path)->aggstrategy == AGG_PLAIN &&
			((AggPath *) path)->aggsplit == AGGSPLIT_SIMPLE)
			templates = lappend(templates, path);
	}
	foreach_ptr(AggPath, agg, templates)
	{
		TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
		Path	   *child = tess_batch_input_path(root, agg->subpath);
		Path		template;

		if (child == NULL || !arguments_available(aggregates, child))
			continue;
		template = agg->path;
		template.total_cost *= AGG_COST_FACTOR;
		config.template_path = &template;
		config.methods = &agg_path_methods;
		config.node = &tess_agg_node;
		config.children = list_make1(child);
		config.expressions = aggregates;
		add_path(output_rel, (Path *) tess_path_create(&config));
	}
}

/*
 * An argument's column of the child's batch as a Var of INDEX_VAR: the
 * arguments cannot go through custom_exprs, which the planner would fix
 * against the scan tuple of aggregates, so they travel in the private
 * data with their column resolved here.
 */
static Node *
resolve_argument(Node *node, TessPlanChild *child)
{
	if (node == NULL)
		return NULL;
	if (IsA(node, Var))
	{
		Var		   *var = (Var *) node;
		TargetEntry *found = tlist_member((Expr *) var, child->plan->targetlist);
		int			column = found == NULL ? -1 :
			tess_layout_column(&child->layout, found->resno - 1);

		if (column < 0)
			elog(ERROR, "TessAgg argument is missing from its child");
		return (Node *) makeVar(INDEX_VAR, column + 1, var->vartype,
								var->vartypmod, var->varcollid, 0);
	}
	return expression_tree_mutator(node, resolve_argument, child);
}

/*
 * The scan tuple is the aggregates themselves, so that the planner turns
 * the targets and HAVING into references to it; the child's columns stay
 * hidden. HAVING is the plan's qual, as it is the core aggregate's. The
 * private data carries one argument per aggregate, a NULL constant for
 * count(*).
 */
static Plan *
agg_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
		 List *tlist, List *clauses, List *custom_plans)
{
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPlanChild child = TESS_STRUCT_INITIALIZER(TessPlanChild);
	List	   *arguments = NIL;

	tess_path_get_info(best_path, &info);
	if (!tess_plan_child(best_path, custom_plans, 0, &child))
		elog(ERROR, "TessAgg expected a batch child");
	foreach_ptr(TargetEntry, entry, info.expressions)
	{
		Node	   *argument = aggregate_argument((Aggref *) entry->expr);

		arguments = lappend(arguments, argument == NULL ?
							(Node *) makeNullConst(INT4OID, -1, InvalidOid) :
							resolve_argument(copyObject(argument), &child));
	}
	config.methods = &tess_agg_scan_methods;
	config.layout_policy = TESS_LAYOUT_DENSE;
	config.qual = (List *) root->parse->havingQual;
	config.scan_targetlist = info.expressions;
	config.scanrelid = 0;
	config.node_data = (Node *) arguments;
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

/* An argument's Var names its batch column directly. */
static int
resolve_column(const Var *var, void *context)
{
	return var->varno == INDEX_VAR ? var->varattno - 1 : -1;
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
	Bitmapset  *projection = NULL;
	List	   *arguments;
	int			index = 0;

	/* The planner puts Material above a batch subtree for these. */
	if (eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK))
		elog(ERROR, "TessAgg supports neither backward scan nor mark/restore");
	tess_plan_get_info(cscan, &info);
	arguments = (List *) info.node_data;
	if (info.node != &tess_agg_node || info.nchildren != 1 ||
		info.child_names[0] == NULL || cscan->custom_scan_tlist == NIL ||
		arguments == NIL ||
		list_length(arguments) != list_length(cscan->custom_scan_tlist))
		elog(ERROR, "TessAgg received a foreign plan");
	state->child = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
	css->custom_ps = list_make1(state->child);
	state->input = tess_input_create(estate->es_query_cxt, state->child);
	state->nvalues = list_length(cscan->custom_scan_tlist);
	state->values = palloc0_array(AggValue, state->nvalues);
	state->status = (TessStatus) TESS_STRUCT_INITIALIZER(TessStatus);
	foreach_ptr(TargetEntry, entry, cscan->custom_scan_tlist)
	{
		Aggref	   *agg = castNode(Aggref, entry->expr);
		AggValue   *value = &state->values[index++];

		value->kind = aggregate_kind(agg->aggfnoid);
		value->function = tess_runtime_api()->functions->find(agg->aggfnoid);
		if (value->kind < 0 || value->function == NULL ||
			value->function->kind != TESS_FUNCTION_AGGREGATE)
			elog(ERROR, "TessAgg has no batch implementation of %s",
				 format_procedure(agg->aggfnoid));
		if (agg->args != NIL)
		{
			value->expr = tess_expr_compile_value(list_nth(arguments, index - 1),
												  &css->ss.ps, resolve_column,
												  NULL);
			if (tess_expr_input_column(value->expr) >= 0)
				projection = bms_add_member(projection,
											tess_expr_input_column(value->expr));
		}
	}
	/* Whole batches; the arguments' columns only for the surviving rows. */
	request.projection_columns = projection;
	request.output_mode = TESS_OUTPUT_BATCH;
	tess_input_set_request(state->input, &request);
	builder.parent_context = estate->es_query_cxt;
	builder.tuple_desc = result->tts_tupleDescriptor;
	builder.ncolumns = result->tts_tupleDescriptor->natts;
	builder.capacity = 1;
	state->builder = tess_builder_create(&builder);
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   result, &info.layout);
}

/* Raise a batch function's failure as the error it names. */
static void
report(const TessStatus *status)
{
	const char *sqlstate = status->sqlstate;

	ereport(ERROR,
			(errcode(MAKE_SQLSTATE(sqlstate[0], sqlstate[1], sqlstate[2],
								   sqlstate[3], sqlstate[4])),
			 errmsg("%s", status->message)));
}

/* Add one batch's partial of the aggregate to its running value. */
static void
accumulate(TessAggState *state, AggValue *value, TessBatch *batch)
{
	TessFunctionCall call = TESS_STRUCT_INITIALIZER(TessFunctionCall);
	TessFunctionArg arg = TESS_STRUCT_INITIALIZER(TessFunctionArg);
	uint64		word = 0;
	TessRowMask present = {1, &word};
	Datum		partial = (Datum) 0;

	call.function = value->function;
	if (value->expr != NULL)
	{
		/*
		 * No readiness mask: the column's rows are all initialized memory
		 * (tessera/batch.h), and a mask would keep the kernel off its vector
		 * path for every word the selection does not fill.
		 */
		tess_expr_bind(value->expr, batch, state->css.ss.ps.ps_ExprContext,
					   TESS_COLUMN_FOR_PROJECTION);
		arg.column = tess_expr_get_column(value->expr);
		call.nargs = 1;
		call.args = &arg;
	}
	call.rows = &batch->rows;
	call.values = &partial;
	call.non_nulls = &present;
	call.context = CurrentMemoryContext;
	call.status = &state->status;
	if (value->function->evaluate(&call) != TESS_OK)
		report(&state->status);
	if ((word & 1) == 0)
		return;
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
			if (!value->has_value || DatumGetInt32(partial) < value->extreme)
				value->extreme = DatumGetInt32(partial);
			break;
		case AGG_MAX:
			if (!value->has_value || DatumGetInt32(partial) > value->extreme)
				value->extreme = DatumGetInt32(partial);
			break;
	}
	value->has_value = true;
}

/* Read every batch of the child into the running values. */
static void
drain(TessAggState *state)
{
	for (;;)
	{
		TessBatch  *batch = tess_input_next(state->input);
		int			rows;

		if (batch == NULL)
			return;
		rows = tess_row_mask_count(&batch->rows);
		state->batches++;
		state->rows += rows;
		if (rows > 0)
		{
			ResetExprContext(state->css.ss.ps.ps_ExprContext);
			for (int index = 0; index < state->nvalues; index++)
				accumulate(state, &state->values[index], batch);
		}
		tess_input_finish(state->input);
	}
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
				scan->tts_values[index] = Int32GetDatum(value->extreme);
				break;
		}
	}
	return ExecStoreVirtualTuple(scan);
}

/*
 * The result row, once: the aggregates in the scan slot, HAVING over
 * them, and the plan's projection when the targets are not the bare
 * aggregates, as the executor set it up for the scan tuple.
 */
static TupleTableSlot *
agg_exec(CustomScanState *css)
{
	TessAggState *state = (TessAggState *) css;
	ExprContext *econtext = css->ss.ps.ps_ExprContext;
	TupleTableSlot *row;
	TessBatch  *batch;

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

	tess_output_end(state->output);
	ExecEndNode(state->child);
}

static void
agg_rescan(CustomScanState *css)
{
	TessAggState *state = (TessAggState *) css;

	tess_output_clear(state->output);
	/* The core passes changed parameters to outer and inner plans only. */
	if (css->ss.ps.chgParam != NULL)
		UpdateChangedParamSet(state->child, css->ss.ps.chgParam);
	ExecReScan(state->child);
	tess_input_rescan(state->input);
	for (int index = 0; index < state->nvalues; index++)
	{
		state->values[index].total = 0;
		state->values[index].has_value = false;
	}
	state->done = false;
	state->batches = 0;
	state->rows = 0;
}

static void
agg_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	TessAggState *state = (TessAggState *) css;

	if (!es->analyze)
		return;
	ExplainPropertyInteger("Input Batches", NULL, state->batches, es);
	ExplainPropertyInteger("Input Rows", NULL, state->rows, es);
}

static const CustomExecMethods agg_exec_methods = {
	.CustomName = "TessAgg",
	.BeginCustomScan = agg_begin,
	.ExecCustomScan = agg_exec,
	.EndCustomScan = agg_end,
	.ReScanCustomScan = agg_rescan,
	.ExplainCustomScan = agg_explain,
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

#include "postgres.h"

#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "nodes/makefuncs.h"
#include "optimizer/optimizer.h"
#include "utils/ruleutils.h"

#include "tessera/expr.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * TessFilter stands on the unary helper: it applies the relation's
 * clauses to each batch of its child, the leading ones compiled as batch
 * filters and the rest row by row over the rows those kept, and passes
 * the batch on with the rows that remain. See docs/nodes.md.
 */
typedef struct FilterState
{
	CustomScanState css;
	TessUnary  *unary;
	/* The child's layout, which the clauses' columns refer to. */
	TessLayout	child_layout;
	/* The batch clauses, in the planner's order. */
	TessExpr  **filters;
	int			nfilters;
	/* The scan tuple attributes the row-wise clauses read, and their columns. */
	int		   *residual_atts;
	int			nresidual;
	TessDatumColumn *columns;
	/* The targets PostgreSQL asks the node to compute, or NULL. */
	TessProjection *projection;
	uint64		batch_removed;
	uint64		residual_removed;
} FilterState;

static void filter_begin(CustomScanState *css, EState *estate, int eflags);
static TupleTableSlot *filter_exec(CustomScanState *css);
static void filter_end(CustomScanState *css);
static void filter_rescan(CustomScanState *css);
static void filter_explain(CustomScanState *css, List *ancestors,
						   ExplainState *es);

static const CustomExecMethods filter_exec_methods = {
	.CustomName = "TessFilter",
	.BeginCustomScan = filter_begin,
	.ExecCustomScan = filter_exec,
	.EndCustomScan = filter_end,
	.ReScanCustomScan = filter_rescan,
	.ExplainCustomScan = filter_explain,
};

const TessNode tess_filter_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_FILTER_NODE_NAME,
};

Node *
tess_filter_create_state(CustomScan *cscan)
{
	FilterState *state = (FilterState *)
		newNode(sizeof(FilterState), T_CustomScanState);

	state->css.methods = &filter_exec_methods;
	return (Node *) state;
}

/* A Var of the scan tuple is a position in the child's target list. */
static int
resolve_column(const Var *var, void *context)
{
	return tess_layout_column((const TessLayout *) context, var->varattno - 1);
}

/*
 * The row-wise clauses over the rows the batch clauses kept: each row is
 * shown to ExecQual through the scan tuple slot, whose attributes the
 * clauses read come from the batch's columns; nothing is allocated.
 */
static int
apply_residual(FilterState *state, TessBatch *batch, int kept)
{
	ExprContext *econtext = state->css.ss.ps.ps_ExprContext;
	TupleTableSlot *slot = state->css.ss.ss_ScanTupleSlot;
	int			row = -1;

	for (int index = 0; index < state->nresidual; index++)
	{
		int			column = tess_layout_column(&state->child_layout,
												state->residual_atts[index]);

		batch->ops->get_datum_column(batch, column, &batch->rows,
									 TESS_COLUMN_FOR_FILTER,
									 &state->columns[index]);
	}
	ExecClearTuple(slot);
	ExecStoreVirtualTuple(slot);
	econtext->ecxt_scantuple = slot;
	while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
	{
		for (int index = 0; index < state->nresidual; index++)
		{
			int			att = state->residual_atts[index];

			slot->tts_values[att] = state->columns[index].values[row];
			slot->tts_isnull[att] = state->columns[index].isnull[row];
		}
		if (!ExecQual(state->css.ss.ps.qual, econtext))
		{
			tess_row_mask_clear(&batch->rows, row);
			kept--;
		}
	}
	return kept;
}

/* Apply the clauses in order, each over the rows the previous ones left. */
static int
filter_batch(void *private_data, TessBatch *batch, int rows)
{
	FilterState *state = private_data;
	ExprContext *econtext = state->css.ss.ps.ps_ExprContext;
	int			kept = rows;
	int			batch_kept;

	ResetExprContext(econtext);
	for (int index = 0; index < state->nfilters && kept > 0; index++)
	{
		tess_expr_bind(state->filters[index], batch, econtext,
					   TESS_COLUMN_FOR_FILTER);
		tess_expr_apply_filter(state->filters[index]);
		kept = tess_row_mask_count(&batch->rows);
	}
	batch_kept = kept;
	state->batch_removed += rows - batch_kept;
	if (state->css.ss.ps.qual != NULL && kept > 0)
		kept = apply_residual(state, batch, kept);
	state->residual_removed += batch_kept - kept;
	return kept;
}

/* The scan tuple attributes the row-wise clauses read, each once. */
static void
prepare_residual(FilterState *state, CustomScan *cscan,
				 Bitmapset **filter_columns)
{
	TupleTableSlot *slot = state->css.ss.ss_ScanTupleSlot;
	Bitmapset  *atts = NULL;
	int			att = -1;
	int			index = 0;

	foreach_ptr(Var, var, pull_var_clause((Node *) cscan->scan.plan.qual, 0))
		atts = bms_add_member(atts, var->varattno - 1);
	state->nresidual = bms_num_members(atts);
	state->residual_atts = palloc_array(int, state->nresidual);
	state->columns = palloc_array(TessDatumColumn, state->nresidual);
	while ((att = bms_next_member(atts, att)) >= 0)
	{
		state->residual_atts[index] = att;
		state->columns[index++] = (TessDatumColumn)
			TESS_STRUCT_INITIALIZER(TessDatumColumn);
		*filter_columns = bms_add_member(*filter_columns,
										 tess_layout_column(&state->child_layout, att));
	}
	/* The attributes the clauses do not read are never looked at. */
	memset(slot->tts_isnull, true, slot->tts_tupleDescriptor->natts);
}

static void
filter_begin(CustomScanState *css, EState *estate, int eflags)
{
	FilterState *state = (FilterState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TessUnaryConfig config = TESS_STRUCT_INITIALIZER(TessUnaryConfig);
	Bitmapset  *filter_columns = NULL;
	PlanState  *child;
	int			index = 0;

	/* The planner puts Material above a batch subtree for these. */
	if (eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK))
		elog(ERROR, "TessFilter supports neither backward scan nor mark/restore");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_filter_node || info.nchildren != 1 ||
		info.child_names[0] == NULL || cscan->custom_exprs == NIL)
		elog(ERROR, "TessFilter received a foreign plan");
	child = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
	css->custom_ps = list_make1(child);
	state->child_layout = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	tess_plan_get_layout(child->plan, &state->child_layout);
	state->nfilters = list_length(cscan->custom_exprs);
	state->filters = palloc_array(TessExpr *, state->nfilters);
	foreach_ptr(Node, clause, cscan->custom_exprs)
	{
		TessExpr   *expr = tess_expr_compile_filter(clause, &css->ss.ps,
													resolve_column,
													&state->child_layout);

		state->filters[index++] = expr;
		filter_columns = bms_add_member(filter_columns,
										tess_expr_input_column(expr));
	}
	if (css->ss.ps.qual != NULL)
		prepare_residual(state, cscan, &filter_columns);
	if (info.computed != NIL)
	{
		TessProjectionConfig projection = TESS_STRUCT_INITIALIZER(TessProjectionConfig);

		/* The scan tuple is the child's target list, as the child maps it. */
		projection.parent_context = estate->es_query_cxt;
		projection.parent = &css->ss.ps;
		projection.econtext = css->ss.ps.ps_ExprContext;
		projection.scan_slot = css->ss.ss_ScanTupleSlot;
		projection.scan_tuple = &state->child_layout;
		projection.base_columns = state->child_layout.ncolumns;
		projection.computed = info.computed;
		state->projection = tess_projection_create(&projection);
	}
	config.parent_context = estate->es_query_cxt;
	config.node = css;
	config.child = child;
	config.layout = &info.layout;
	config.filter_columns = filter_columns;
	config.process = filter_batch;
	config.private_data = state;
	config.projection = state->projection;
	state->unary = tess_unary_create(&config);
}

static TupleTableSlot *
filter_exec(CustomScanState *css)
{
	FilterState *state = (FilterState *) css;

	return tess_unary_exec(state->unary);
}

static void
filter_end(CustomScanState *css)
{
	FilterState *state = (FilterState *) css;

	tess_unary_end(state->unary);
	ExecEndNode(linitial(css->custom_ps));
}

static void
filter_rescan(CustomScanState *css)
{
	FilterState *state = (FilterState *) css;

	tess_unary_rescan(state->unary);
}

/* Per loop, as the core shows the rows its qualifiers removed. */
static void
show_removed(const char *label, uint64 removed, CustomScanState *css,
			 ExplainState *es)
{
	if (css->ss.ps.instrument != NULL && css->ss.ps.instrument->nloops > 0)
		ExplainPropertyFloat(label, NULL,
							 removed / css->ss.ps.instrument->nloops, 0, es);
}

/* The batch clauses, as the core shows a scan's qualifiers. */
static void
filter_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	FilterState *state = (FilterState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	List	   *context;
	bool		useprefix = es->rtable_size > 1 || es->verbose;
	const TessUnaryStats *stats;

	context = set_deparse_context_plan(es->deparse_cxt, css->ss.ps.plan,
									   ancestors);
	ExplainPropertyText("Batch Filter",
						deparse_expression((Node *) make_ands_explicit(cscan->custom_exprs),
										   context, useprefix, false), es);
	if (!es->analyze)
		return;
	show_removed("Rows Removed by Batch Filter", state->batch_removed, css, es);
	if (cscan->scan.plan.qual != NIL)
		show_removed("Rows Removed by Residual Filter", state->residual_removed,
					 css, es);
	stats = tess_unary_stats(state->unary);
	ExplainPropertyInteger("Input Batches", NULL, stats->input_batches, es);
	ExplainPropertyInteger("Input Rows", NULL, stats->input_rows, es);
	ExplainPropertyInteger("Output Rows", NULL, stats->output_rows, es);
	if (state->projection != NULL)
	{
		const TessProjectionStats *computed = tess_projection_stats(state->projection);

		ExplainPropertyInteger("Computed Datums", NULL,
							   computed->chain_datums + computed->row_datums, es);
	}
}

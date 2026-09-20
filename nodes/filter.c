#include "postgres.h"

#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "nodes/makefuncs.h"
#include "utils/ruleutils.h"

#include "tessera/expr.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * TessFilter stands on the unary helper: it applies the relation's
 * clauses, compiled as batch filters, to each batch of its child and
 * passes the batch on with the rows that remain. See docs/nodes.md.
 */
typedef struct FilterState
{
	CustomScanState css;
	TessUnary  *unary;
	/* The child's layout, which the clauses' columns refer to. */
	TessLayout	child_layout;
	/* The clauses, in the planner's order. */
	TessExpr  **filters;
	int			nfilters;
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

/* Apply the clauses in order, each over the rows the previous ones left. */
static int
filter_batch(void *private_data, TessBatch *batch, int rows)
{
	FilterState *state = private_data;
	ExprContext *econtext = state->css.ss.ps.ps_ExprContext;
	int			kept = rows;

	ResetExprContext(econtext);
	for (int index = 0; index < state->nfilters && kept > 0; index++)
	{
		tess_expr_bind(state->filters[index], batch, econtext,
					   TESS_COLUMN_FOR_FILTER);
		tess_expr_apply_filter(state->filters[index]);
		kept = tess_row_mask_count(&batch->rows);
	}
	return kept;
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
	config.parent_context = estate->es_query_cxt;
	config.node = css;
	config.child = child;
	config.layout = &info.layout;
	config.filter_columns = filter_columns;
	config.process = filter_batch;
	config.private_data = state;
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

/* The clauses, as the core shows a scan's qualifiers. */
static void
filter_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	List	   *context;
	bool		useprefix = es->rtable_size > 1 || es->verbose;

	context = set_deparse_context_plan(es->deparse_cxt, css->ss.ps.plan,
									   ancestors);
	ExplainPropertyText("Batch Filter",
						deparse_expression((Node *) make_ands_explicit(cscan->custom_exprs),
										   context, useprefix, false), es);
}

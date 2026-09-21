#include "postgres.h"

#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"

#include "tessera/runtime.h"

#include "internal.h"

/*
 * TessLimit stands on the unary helper: it removes the offset's rows and
 * the rows past the count from each batch of its child, stops the input
 * once the count is reached, and tells the child how many rows it needs.
 */
typedef struct TessLimitState
{
	CustomScanState css;
	TessUnary  *unary;
	ExprState  *offset_expr;
	ExprState  *count_expr;
	int64		offset_remaining;
	int64		count_remaining;
	bool		no_count;
	/* The expressions are evaluated at the first execution after a rescan. */
	bool		limits_ready;
} TessLimitState;

static void limit_begin(CustomScanState *css, EState *estate, int eflags);
static TupleTableSlot *limit_exec(CustomScanState *css);
static void limit_end(CustomScanState *css);
static void limit_rescan(CustomScanState *css);
static void limit_explain(CustomScanState *css, List *ancestors,
						  ExplainState *es);

static const CustomExecMethods limit_exec_methods = {
	.CustomName = "TessLimit",
	.BeginCustomScan = limit_begin,
	.ExecCustomScan = limit_exec,
	.EndCustomScan = limit_end,
	.ReScanCustomScan = limit_rescan,
	.ExplainCustomScan = limit_explain,
};

Node *
tess_limit_create_state(CustomScan *cscan)
{
	TessLimitState *state = (TessLimitState *)
		newNode(sizeof(TessLimitState), T_CustomScanState);

	state->css.methods = &limit_exec_methods;
	return (Node *) state;
}

/* Drop the offset's rows, keep the count, stop once it is reached. */
static int
trim_batch(void *private_data, TessBatch *batch, int rows)
{
	TessLimitState *state = private_data;
	TessRowMask *mask = &batch->rows;
	int			nwords = tess_row_mask_word_count(mask->nrows);
	int			kept = 0;

	/*
	 * Whole words at a time: a word the offset still covers is cleared, a
	 * word past the count is cleared, a word the count still covers is
	 * kept, and only a word the offset or the count ends in is walked row
	 * by row.
	 */
	for (int word = 0; word < nwords; word++)
	{
		uint64		bits = mask->bits[word];
		int			present;

		if (bits == 0)
			continue;
		present = pg_popcount64(bits);
		if (state->offset_remaining >= present)
		{
			mask->bits[word] = 0;
			state->offset_remaining -= present;
			continue;
		}
		if (!state->no_count && state->count_remaining == 0)
		{
			mask->bits[word] = 0;
			continue;
		}
		if (state->offset_remaining == 0 &&
			(state->no_count || state->count_remaining >= present))
		{
			if (!state->no_count)
				state->count_remaining -= present;
			kept += present;
			continue;
		}
		while (bits != 0)
		{
			uint64		bit = bits & (~bits + 1);

			if (state->offset_remaining > 0)
			{
				mask->bits[word] &= ~bit;
				state->offset_remaining--;
			}
			else if (!state->no_count && state->count_remaining == 0)
				mask->bits[word] &= ~bit;
			else
			{
				if (!state->no_count)
					state->count_remaining--;
				kept++;
			}
			bits &= bits - 1;
		}
	}
	if (!state->no_count && state->count_remaining == 0)
		tess_unary_stop(state->unary);
	return kept;
}

static void
limit_begin(CustomScanState *css, EState *estate, int eflags)
{
	TessLimitState *state = (TessLimitState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TessUnaryConfig config = TESS_STRUCT_INITIALIZER(TessUnaryConfig);
	PlanState  *child;

	/* The planner puts Material above a batch subtree for these. */
	if (eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK))
		elog(ERROR, "TessLimit supports neither backward scan nor mark/restore");
	tess_plan_get_info(cscan, &info);
	if (info.nchildren != 1 || info.child_names[0] == NULL ||
		list_length(cscan->custom_exprs) != 2)
		elog(ERROR, "TessLimit expected one batch child and two expressions");
	child = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
	css->custom_ps = list_make1(child);
	config.parent_context = estate->es_query_cxt;
	config.node = css;
	config.child = child;
	config.layout = &info.layout;
	config.process = trim_batch;
	config.private_data = state;
	state->unary = tess_unary_create(&config);
	if (linitial(cscan->custom_exprs) != NULL)
		state->offset_expr = ExecInitExpr(linitial(cscan->custom_exprs),
										  &css->ss.ps);
	if (lsecond(cscan->custom_exprs) != NULL)
		state->count_expr = ExecInitExpr(lsecond(cscan->custom_exprs),
										 &css->ss.ps);
}

/* Evaluate the expressions, as the core node does, and bound the child. */
static void
compute_limits(TessLimitState *state)
{
	ExprContext *econtext = state->css.ss.ps.ps_ExprContext;
	Datum		value;
	bool		isnull;

	state->offset_remaining = 0;
	if (state->offset_expr != NULL)
	{
		value = ExecEvalExprSwitchContext(state->offset_expr, econtext, &isnull);
		state->offset_remaining = isnull ? 0 : DatumGetInt64(value);
		if (state->offset_remaining < 0)
			ereport(ERROR,
					(errcode(ERRCODE_INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE),
					 errmsg("OFFSET must not be negative")));
	}
	state->no_count = true;
	state->count_remaining = 0;
	if (state->count_expr != NULL)
	{
		value = ExecEvalExprSwitchContext(state->count_expr, econtext, &isnull);
		if (!isnull)
		{
			state->no_count = false;
			state->count_remaining = DatumGetInt64(value);
			if (state->count_remaining < 0)
				ereport(ERROR,
						(errcode(ERRCODE_INVALID_ROW_COUNT_IN_LIMIT_CLAUSE),
						 errmsg("LIMIT must not be negative")));
		}
	}
	if (!state->no_count && state->count_remaining == 0)
		tess_unary_stop(state->unary);
	if (state->no_count ||
		state->count_remaining > PG_INT64_MAX - state->offset_remaining)
		tess_unary_set_tuple_bound(state->unary, -1);
	else
		tess_unary_set_tuple_bound(state->unary,
								   state->offset_remaining + state->count_remaining);
	state->limits_ready = true;
}

static TupleTableSlot *
limit_exec(CustomScanState *css)
{
	TessLimitState *state = (TessLimitState *) css;

	if (!state->limits_ready)
		compute_limits(state);
	return tess_unary_exec(state->unary);
}

static void
limit_end(CustomScanState *css)
{
	TessLimitState *state = (TessLimitState *) css;

	tess_unary_end(state->unary);
	ExecEndNode(linitial(css->custom_ps));
}

static void
limit_rescan(CustomScanState *css)
{
	TessLimitState *state = (TessLimitState *) css;

	tess_unary_rescan(state->unary);
	state->limits_ready = false;
}

static void
limit_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	TessLimitState *state = (TessLimitState *) css;
	const TessUnaryStats *stats;

	if (!es->analyze)
		return;
	stats = tess_unary_stats(state->unary);
	ExplainPropertyInteger("Input Batches", NULL, stats->input_batches, es);
	ExplainPropertyInteger("Input Rows", NULL, stats->input_rows, es);
	ExplainPropertyInteger("Output Rows", NULL, stats->output_rows, es);
}

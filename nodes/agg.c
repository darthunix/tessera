#include "postgres.h"

#include "commands/explain.h"
#include "commands/explain_format.h"
#include "common/int.h"
#include "executor/executor.h"
#include "utils/fmgroids.h"
#include "utils/regproc.h"

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
 * make. The planner side follows. See docs/nodes.md.
 */

/* How the partials of an aggregate combine, and what an empty input gives. */
typedef enum AggKind
{
	AGG_COUNT,					/* int8 sum of the partials, 0 without any */
} AggKind;

typedef struct AggValue
{
	AggKind		kind;
	const TessFunction *function;
	int64		total;
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
	/* The row was returned; the next call ends the scan. */
	bool		done;
	uint64		batches;
	uint64		rows;
} TessAggState;

static const CustomExecMethods agg_exec_methods;

/* The kind of a supported aggregate, or -1: the node knows how to combine these. */
static int
aggregate_kind(Oid aggfnoid)
{
	switch (aggfnoid)
	{
		case F_COUNT_:
			return AGG_COUNT;
		default:
			return -1;
	}
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
	int			index = 0;

	/* The planner puts Material above a batch subtree for these. */
	if (eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK))
		elog(ERROR, "TessAgg supports neither backward scan nor mark/restore");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_agg_node || info.nchildren != 1 ||
		info.child_names[0] == NULL || cscan->custom_scan_tlist == NIL)
		elog(ERROR, "TessAgg received a foreign plan");
	state->child = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
	css->custom_ps = list_make1(state->child);
	state->input = tess_input_create(estate->es_query_cxt, state->child);
	state->nvalues = list_length(cscan->custom_scan_tlist);
	state->values = palloc0_array(AggValue, state->nvalues);
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
	}
	/* Whole batches; no column is needed for count(*). */
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
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	uint64		word = 0;
	TessRowMask present = {1, &word};
	Datum		partial = (Datum) 0;

	call.function = value->function;
	call.nargs = 0;
	call.rows = &batch->rows;
	call.values = &partial;
	call.non_nulls = &present;
	call.context = CurrentMemoryContext;
	call.status = &status;
	if (value->function->evaluate(&call) != TESS_OK)
		report(&status);
	if ((word & 1) == 0)
		return;
	switch (value->kind)
	{
		case AGG_COUNT:
			if (pg_add_s64_overflow(value->total, DatumGetInt64(partial),
									&value->total))
				ereport(ERROR,
						(errcode(ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE),
						 errmsg("bigint out of range")));
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
			for (int index = 0; index < state->nvalues; index++)
				accumulate(state, &state->values[index], batch);
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

		switch (value->kind)
		{
			case AGG_COUNT:
				scan->tts_values[index] = Int64GetDatum(value->total);
				scan->tts_isnull[index] = false;
				break;
		}
	}
	return ExecStoreVirtualTuple(scan);
}

static TupleTableSlot *
agg_exec(CustomScanState *css)
{
	TessAggState *state = (TessAggState *) css;
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
	row = ExecCopySlot(css->ss.ps.ps_ResultTupleSlot, result_row(state));
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

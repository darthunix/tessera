#include "postgres.h"

#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"

#include "tessera/runtime.h"

#include "internal.h"

/*
 * TessPack turns the rows of an ordinary child into batches for a
 * batch-aware parent. The parent creates the path through
 * tess_batch_input_path, so the node never stands under a row-wise one;
 * a request for rows is an error at the first execution. Every column is
 * materialized: the request's column masks are not used yet. See
 * docs/nodes.md.
 */
#define PACK_BATCH_ROWS 64

typedef struct PackState
{
	CustomScanState css;
	PlanState  *child;
	TessOutput *output;
	/* Created at the first execution, once the parent's request is frozen. */
	TessBuilder *builder;
	const TessRequest *request;
	int			capacity;
	/* The child returned its last row. */
	bool		exhausted;
	uint64		batches;
} PackState;

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

/* The path costs what its child costs: there is no cost model yet. */
static CustomPath *
pack_wrap_rows(PlannerInfo *root, Path *child)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);

	config.template_path = child;
	config.methods = &pack_path_methods;
	config.node = &tess_pack_node;
	config.children = list_make1(child);
	return tess_path_create(&config);
}

const TessNode tess_pack_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_PACK_NODE_NAME,
	.wrap_rows = pack_wrap_rows,
};

static Plan *
pack_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
		  List *tlist, List *clauses, List *custom_plans)
{
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	Plan	   *child = linitial(custom_plans);

	/*
	 * The child was planned with its exact target list, from the same path
	 * target as this node's, so child attribute N is target N. The clauses
	 * are the relation's, which the child already evaluates.
	 */
	if (list_length(tlist) != list_length(child->targetlist))
		elog(ERROR, "Tessera pack target list does not match its child");
	config.methods = &tess_pack_scan_methods;
	config.layout_policy = TESS_LAYOUT_DENSE;
	config.scan_targetlist = child->targetlist;
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

	/* The planner puts Material above a batch subtree for these. */
	if (eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK))
		elog(ERROR, "Tessera pack supports neither backward scan nor mark/restore");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_pack_node || info.nchildren != 1 ||
		info.child_names[0] != NULL)
		elog(ERROR, "Tessera pack received a foreign plan");
	state->child = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
	css->custom_ps = list_make1(state->child);
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   css->ss.ps.ps_ResultTupleSlot,
									   &info.layout);
}

/* Freeze the parent's request; the batch size follows from it. */
static void
pack_freeze_request(PackState *state)
{
	state->request = tess_output_request(state->output);
	state->capacity = state->request->max_batch_rows > 0 ?
		Min(state->request->max_batch_rows, PACK_BATCH_ROWS) :
		PACK_BATCH_ROWS;
}

static TupleTableSlot *
pack_exec(CustomScanState *css)
{
	PackState  *state = (PackState *) css;
	TessBatch  *batch;

	if (state->builder == NULL)
	{
		TessBuilderConfig config = TESS_STRUCT_INITIALIZER(TessBuilderConfig);

		config.parent_context = css->ss.ps.state->es_query_cxt;
		config.tuple_desc = ExecGetResultType(state->child);
		config.ncolumns = css->ss.ps.ps_ResultTupleSlot->tts_tupleDescriptor->natts;
		pack_freeze_request(state);
		config.capacity = state->capacity;
		if (state->request->output_mode != TESS_OUTPUT_BATCH)
			elog(ERROR, "Tessera pack requires a batch-aware parent");
		state->builder = tess_builder_create(&config);
	}
	/* Refuses while the parent has not finished the previous batch. */
	tess_output_release(state->output);
	if (state->exhausted)
		return NULL;
	tess_builder_reset(state->builder);
	while (!tess_builder_is_full(state->builder))
	{
		TupleTableSlot *slot = ExecProcNode(state->child);

		if (TupIsNull(slot))
		{
			state->exhausted = true;
			break;
		}
		tess_builder_append_slot(state->builder, slot);
	}
	batch = tess_builder_finish(state->builder, InvalidOid);
	if (batch == NULL)
		return NULL;
	state->batches++;
	return tess_output_publish(state->output, batch);
}

static void
pack_end(CustomScanState *css)
{
	PackState  *state = (PackState *) css;

	tess_output_end(state->output);
	ExecEndNode(state->child);
}

static void
pack_rescan(CustomScanState *css)
{
	PackState  *state = (PackState *) css;

	tess_output_clear(state->output);
	ExecReScan(state->child);
	state->exhausted = false;
	state->batches = 0;
}

static void
pack_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	PackState  *state = (PackState *) css;

	/* The parent's request, and so the size, is known once executed. */
	if (state->request != NULL)
		ExplainPropertyInteger("Batch Size", NULL, state->capacity, es);
	if (es->analyze)
		ExplainPropertyInteger("Batches", NULL, state->batches, es);
}

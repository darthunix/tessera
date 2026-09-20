#include "postgres.h"

#include <string.h>

#include "catalog/pg_class.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "fmgr.h"
#include "nodes/makefuncs.h"
#include "optimizer/pathnode.h"
#include "optimizer/paths.h"
#include "utils/guc.h"
#include "utils/lsyscache.h"

#include "tessera/planner.h"
#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

PG_FUNCTION_INFO_V1(tessera_test_pack_paths);

/*
 * A stand-in for a batch-aware parent: the sink wraps the sequential scan
 * of every table named pack_* in the pack node through the batch-input
 * helper, requests batches through the input helper and returns their
 * rows one by one, as the unary node helper will.
 */
typedef struct SinkState
{
	CustomScanState css;
	PlanState  *child;
	TessInput  *input;
	const TessLayout *layout;
	TessBatch  *batch;
	/* One per result attribute, for the active batch. */
	TessDatumColumn *columns;
	int			row;
	uint64		batches;
} SinkState;

static int	batch_rows = 0;
static bool rows_mode = false;
static set_rel_pathlist_hook_type previous_set_rel_pathlist_hook = NULL;

static const TessNode sink_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = "tessera.pack_sink",
};

static Node *sink_create_state(CustomScan *cscan);

static const CustomScanMethods sink_scan_methods = {
	.CustomName = "tessera_pack_sink",
	.CreateCustomScanState = sink_create_state,
};

static Plan *
sink_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
		  List *tlist, List *clauses, List *custom_plans)
{
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	Plan	   *child = linitial(custom_plans);

	config.methods = &sink_scan_methods;
	config.layout_policy = TESS_LAYOUT_DENSE;
	config.scan_targetlist = child->targetlist;
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

static const CustomPathMethods sink_path_methods = {
	.CustomName = "tessera_pack_sink",
	.PlanCustomPath = sink_plan,
};

static void
sink_begin(CustomScanState *css, EState *estate, int eflags)
{
	SinkState  *state = (SinkState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
	int			natts = css->ss.ps.ps_ResultTupleSlot->tts_tupleDescriptor->natts;

	tess_plan_get_info(cscan, &info);
	if (info.nchildren != 1 || info.child_names[0] == NULL ||
		strcmp(info.child_names[0], TESS_PACK_NODE_NAME) != 0)
		elog(ERROR, "Tessera pack sink expected a pack child");
	state->child = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
	css->custom_ps = list_make1(state->child);
	state->input = tess_input_create(estate->es_query_cxt, state->child);
	request.output_mode = rows_mode ? TESS_OUTPUT_ROWS : TESS_OUTPUT_BATCH;
	request.max_batch_rows = batch_rows;
	tess_input_set_request(state->input, &request);
	state->layout = tess_input_layout(state->input);
	if (state->layout->ntargets != natts)
		elog(ERROR, "Tessera pack sink layout does not match its slot");
	state->columns = palloc0_array(TessDatumColumn, natts);
	state->row = -1;
}

static TupleTableSlot *
sink_exec(CustomScanState *css)
{
	SinkState  *state = (SinkState *) css;
	TupleTableSlot *slot = css->ss.ps.ps_ResultTupleSlot;
	int			natts = slot->tts_tupleDescriptor->natts;

	for (;;)
	{
		int			row;

		if (state->batch == NULL)
		{
			state->batch = tess_input_next(state->input);
			if (state->batch == NULL)
				return NULL;
			state->batches++;
			state->row = -1;
			for (int i = 0; i < natts; i++)
			{
				state->columns[i] = (TessDatumColumn)
					TESS_STRUCT_INITIALIZER(TessDatumColumn);
				state->batch->ops->get_datum_column(state->batch,
													tess_layout_column(state->layout, i),
													&state->batch->rows,
													TESS_COLUMN_FOR_PROJECTION,
													&state->columns[i]);
			}
		}
		row = tess_row_mask_next(&state->batch->rows, state->row);
		if (row < 0)
		{
			tess_input_finish(state->input);
			state->batch = NULL;
			continue;
		}
		state->row = row;
		ExecClearTuple(slot);
		for (int i = 0; i < natts; i++)
		{
			slot->tts_values[i] = state->columns[i].values[row];
			slot->tts_isnull[i] = state->columns[i].isnull[row];
		}
		return ExecStoreVirtualTuple(slot);
	}
}

static void
sink_end(CustomScanState *css)
{
	ExecEndNode(((SinkState *) css)->child);
}

static void
sink_rescan(CustomScanState *css)
{
	SinkState  *state = (SinkState *) css;

	if (state->batch != NULL)
	{
		tess_input_finish(state->input);
		state->batch = NULL;
	}
	ExecReScan(state->child);
	tess_input_rescan(state->input);
	state->batches = 0;
}

static void
sink_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	SinkState  *state = (SinkState *) css;

	ExplainPropertyInteger("Requested Rows", NULL, batch_rows, es);
	if (es->analyze)
		ExplainPropertyInteger("Batches Received", NULL, state->batches, es);
}

static const CustomExecMethods sink_exec_methods = {
	.CustomName = "tessera_pack_sink",
	.BeginCustomScan = sink_begin,
	.ExecCustomScan = sink_exec,
	.EndCustomScan = sink_end,
	.ReScanCustomScan = sink_rescan,
	.ExplainCustomScan = sink_explain,
};

static Node *
sink_create_state(CustomScan *cscan)
{
	SinkState  *state = (SinkState *)
		newNode(sizeof(SinkState), T_CustomScanState);

	state->css.methods = &sink_exec_methods;
	return (Node *) state;
}

static void
sink_set_rel_pathlist(PlannerInfo *root, RelOptInfo *rel, Index rti,
					  RangeTblEntry *rte)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	const char *name;
	Path	   *seqscan = NULL;
	Path	   *copy;
	Path	   *child;
	Path		template;

	if (previous_set_rel_pathlist_hook != NULL)
		previous_set_rel_pathlist_hook(root, rel, rti, rte);
	if (!*tess_runtime_api()->settings->enable)
		return;
	if (rte->rtekind != RTE_RELATION || rte->relkind != RELKIND_RELATION)
		return;
	name = get_rel_name(rte->relid);
	if (name == NULL || strncmp(name, "pack_", 5) != 0)
		return;
	foreach_ptr(Path, path, rel->pathlist)
	{
		if (path->pathtype == T_SeqScan && path->param_info == NULL)
		{
			seqscan = path;
			break;
		}
	}
	if (seqscan == NULL)
		return;
	/* add_path frees the dominated core path; the wrapped child is a copy. */
	copy = makeNode(Path);
	*copy = *seqscan;
	child = tess_batch_input_path(root, copy);
	if (child == NULL)
		return;
	template = *seqscan;
	template.total_cost *= 0.5;
	config.template_path = &template;
	config.methods = &sink_path_methods;
	config.node = &sink_node;
	config.children = list_make1(child);
	add_path(rel, (Path *) tess_path_create(&config));
}

void
_PG_init(void)
{
	const TessApi *api = tess_runtime_api();

	api->nodes->add(&sink_node);
	RegisterCustomScanMethods(&sink_scan_methods);
	DefineCustomIntVariable("pack_test.batch_rows",
							"Rows the sink requests per batch; 0 leaves it to the pack node.",
							NULL, &batch_rows, 0, 0, 1000, PGC_USERSET, 0,
							NULL, NULL, NULL);
	DefineCustomBoolVariable("pack_test.rows_mode",
							 "Request rows instead of batches.",
							 NULL, &rows_mode, false, PGC_USERSET, 0,
							 NULL, NULL, NULL);
	previous_set_rel_pathlist_hook = set_rel_pathlist_hook;
	set_rel_pathlist_hook = sink_set_rel_pathlist;
}

/* The batch-input helper with the pack node loaded. */
Datum
tessera_test_pack_paths(PG_FUNCTION_ARGS)
{
	const TessApi *api = tess_runtime_api();
	const TessNode *pack = api->nodes->find(TESS_PACK_NODE_NAME);
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	Path	   *rows = makeNode(Path);
	Path	   *wrapped;
	CustomPath *batch;
	bool		result;

	rows->pathtype = T_SeqScan;
	rows->rows = 10;
	rows->startup_cost = 1;
	rows->total_cost = 20;
	rows->parallel_safe = true;
	wrapped = tess_batch_input_path(NULL, rows);
	result = pack != NULL && wrapped != NULL && IsA(wrapped, CustomPath) &&
		tess_path_node(wrapped) == pack && wrapped->rows == 10 &&
		wrapped->total_cost == 20 && wrapped->parallel_safe &&
		list_length(((CustomPath *) wrapped)->custom_paths) == 1 &&
		linitial(((CustomPath *) wrapped)->custom_paths) == rows;
	/* A batch path is its own batch input. */
	config.template_path = rows;
	config.methods = &sink_path_methods;
	config.node = &sink_node;
	batch = tess_path_create(&config);
	result &= tess_batch_input_path(NULL, &batch->path) == &batch->path;
	/* A parameterized path has no batch input. */
	rows->param_info = makeNode(ParamPathInfo);
	result &= tess_batch_input_path(NULL, rows) == NULL;
	PG_RETURN_BOOL(result);
}

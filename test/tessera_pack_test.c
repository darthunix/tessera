#include "postgres.h"

#include <string.h>

#include "catalog/pg_class.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "fmgr.h"
#include "lib/stringinfo.h"
#include "nodes/makefuncs.h"
#include "nodes/value.h"
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
 * A stand-in for a batch-aware parent on the unary helper. The hook wraps
 * the sequential scan of every table named pack_* in the pack node through
 * the batch-input helper and puts the sink above it, which serves the rows
 * of the batches to the executor. With pack_test.trim set, a trim node of
 * the same kind stands between them: it keeps the rows whose first column
 * is at most the setting, and shows the request the helper forwarded.
 */
#define ROLE_SINK 0
#define ROLE_TRIM 1
#define TRIM_BATCH_ROWS 32

typedef struct SinkState
{
	CustomScanState css;
	int			role;
	TessUnary  *unary;
	/* With pack_test.rows_mode, the sink asks the child for rows itself. */
	TessInput  *rows_input;
} SinkState;

static int	batch_rows = 0;
static bool rows_mode = false;
static int	trim_rows = -1;
static bool stop_after_first = false;
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
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);

	/* A pass-through keeps its child's layout, whatever its columns. */
	tess_path_get_info(best_path, &info);
	config.methods = &sink_scan_methods;
	config.layout_policy = TESS_LAYOUT_PRESERVE_CHILD;
	config.layout_child = 0;
	config.node_data = info.node_data;
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

static const CustomPathMethods sink_path_methods = {
	.CustomName = "tessera_pack_sink",
	.PlanCustomPath = sink_plan,
};

static CustomPath *
make_sink_path(const Path *template, Path *child, int role)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);

	config.template_path = template;
	config.methods = &sink_path_methods;
	config.node = &sink_node;
	config.children = list_make1(child);
	config.node_data = (Node *) makeInteger(role);
	return tess_path_create(&config);
}

/* Keep the rows whose first column is at most the trim setting. */
static int
trim_batch(void *private_data, TessBatch *batch, int rows)
{
	SinkState  *state = private_data;
	TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	int			row = -1;
	int			kept = 0;

	batch->ops->get_datum_column(batch, 0, &batch->rows,
								 TESS_COLUMN_FOR_FILTER, &column);
	while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
	{
		if (column.isnull[row] || DatumGetInt32(column.values[row]) > trim_rows)
			tess_row_mask_clear(&batch->rows, row);
		else
			kept++;
	}
	if (kept > 0 && stop_after_first)
		tess_unary_stop(state->unary);
	return kept;
}

static void
sink_begin(CustomScanState *css, EState *estate, int eflags)
{
	SinkState  *state = (SinkState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TessUnaryConfig config = TESS_STRUCT_INITIALIZER(TessUnaryConfig);
	PlanState  *child;

	tess_plan_get_info(cscan, &info);
	if (info.nchildren != 1 || info.child_names[0] == NULL ||
		info.node_data == NULL || !IsA(info.node_data, Integer))
		elog(ERROR, "Tessera pack sink expected a batch child and a role");
	state->role = intVal(info.node_data);
	child = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
	css->custom_ps = list_make1(child);
	if (state->role == ROLE_SINK && rows_mode)
	{
		TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);

		state->rows_input = tess_input_create(estate->es_query_cxt, child);
		request.output_mode = TESS_OUTPUT_ROWS;
		tess_input_set_request(state->rows_input, &request);
		return;
	}
	config.parent_context = estate->es_query_cxt;
	config.node = css;
	config.child = child;
	config.layout = &info.layout;
	if (state->role == ROLE_TRIM)
	{
		config.filter_columns = bms_make_singleton(0);
		config.max_rows = TRIM_BATCH_ROWS;
		config.process = trim_batch;
		config.private_data = state;
	}
	else
		config.max_rows = batch_rows;
	state->unary = tess_unary_create(&config);
}

static TupleTableSlot *
sink_exec(CustomScanState *css)
{
	SinkState  *state = (SinkState *) css;

	if (state->rows_input != NULL)
	{
		tess_input_next(state->rows_input);
		return NULL;
	}
	return tess_unary_exec(state->unary);
}

static void
sink_end(CustomScanState *css)
{
	SinkState  *state = (SinkState *) css;

	if (state->unary != NULL)
		tess_unary_end(state->unary);
	ExecEndNode(linitial(css->custom_ps));
}

static void
sink_rescan(CustomScanState *css)
{
	SinkState  *state = (SinkState *) css;

	if (state->unary != NULL)
		tess_unary_rescan(state->unary);
	else
		ExecReScan(linitial(css->custom_ps));
}

static void
append_mask(StringInfo text, const char *label, const Bitmapset *mask)
{
	int			member = -1;

	appendStringInfo(text, "%s {", label);
	while ((member = bms_next_member(mask, member)) >= 0)
		appendStringInfo(text, "%s%d", text->data[text->len - 1] == '{' ? "" : " ",
						 member);
	appendStringInfoString(text, "} ");
}

static void
sink_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	SinkState  *state = (SinkState *) css;
	const TessUnaryStats *stats;
	const TessRequest *request;
	StringInfoData text;

	ExplainPropertyText("Role", state->role == ROLE_TRIM ? "trim" : "sink", es);
	if (state->role == ROLE_SINK)
		ExplainPropertyInteger("Requested Rows", NULL, batch_rows, es);
	if (!es->analyze || state->unary == NULL)
		return;
	stats = tess_unary_stats(state->unary);
	ExplainPropertyInteger("Input Batches", NULL, stats->input_batches, es);
	ExplainPropertyInteger("Input Rows", NULL, stats->input_rows, es);
	ExplainPropertyInteger("Output Rows", NULL, stats->output_rows, es);
	request = tess_unary_child_request(state->unary);
	if (request == NULL)
		return;
	initStringInfo(&text);
	append_mask(&text, "filter", request->filter_columns);
	append_mask(&text, "projection", request->projection_columns);
	appendStringInfo(&text, "max %d", request->max_batch_rows);
	ExplainPropertyText("Child Request", text.data, es);
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
	const char *name;
	const TessNode *pack;
	CustomPath *wrapped;
	Path	   *seqscan;
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
	/*
	 * A sequential scan of its own: the one in the path list may have given
	 * way to the native scan, and add_path frees the paths the sink's
	 * dominates.
	 */
	seqscan = create_seqscan_path(root, rel, NULL, 0);
	/* The pack node itself: a batch input may prefer a native scan. */
	pack = tess_runtime_api()->nodes->find(TESS_PACK_NODE_NAME);
	if (pack == NULL || !TESS_ABI_HAS_FIELD(pack, TessNode, wrap_rows) ||
		pack->wrap_rows == NULL || (wrapped = pack->wrap_rows(root, seqscan)) == NULL)
		return;
	child = &wrapped->path;
	template = *seqscan;
	template.total_cost *= 0.5;
	if (trim_rows >= 0)
		child = (Path *) make_sink_path(&template, child, ROLE_TRIM);
	add_path(rel, (Path *) make_sink_path(&template, child, ROLE_SINK));
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
	DefineCustomIntVariable("pack_test.trim",
							"Keep rows whose first column is at most this; -1 adds no trim node.",
							NULL, &trim_rows, -1, -1, 1000, PGC_USERSET, 0,
							NULL, NULL, NULL);
	DefineCustomBoolVariable("pack_test.stop",
							 "Stop the trim node after its first batch with rows.",
							 NULL, &stop_after_first, false, PGC_USERSET, 0,
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
	/* The pack costs its child and the copying of the child's rows. */
	result = pack != NULL && wrapped != NULL && IsA(wrapped, CustomPath) &&
		tess_path_node(wrapped) == pack && wrapped->rows == 10 &&
		wrapped->startup_cost == 1 && wrapped->total_cost > 20 &&
		wrapped->total_cost < 21 && wrapped->parallel_safe &&
		list_length(((CustomPath *) wrapped)->custom_paths) == 1 &&
		linitial(((CustomPath *) wrapped)->custom_paths) == rows;
	/* A batch path is its own batch input. */
	batch = make_sink_path(rows, rows, ROLE_SINK);
	result &= tess_batch_input_path(NULL, &batch->path) == &batch->path;
	/* A parameterized path has no batch input. */
	rows->param_info = makeNode(ParamPathInfo);
	result &= tess_batch_input_path(NULL, rows) == NULL;
	PG_RETURN_BOOL(result);
}

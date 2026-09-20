#include "postgres.h"

#include "access/heapam.h"
#include "access/relscan.h"
#include "access/table.h"
#include "access/tableam.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "optimizer/optimizer.h"
#include "parser/parsetree.h"
#include "pgstat.h"
#include "storage/bufmgr.h"
#include "storage/bufpage.h"
#include "utils/rel.h"

#include "tessera/runtime.h"

#include "internal.h"

/*
 * TessHeapScan reads a heap relation in batches for a batch-aware parent.
 * It lets the core's scan bring one page at a time into memory, prune it
 * and decide which tuples are visible, then publishes the page's visible
 * tuples as heap batches, up to 64 rows each, that pin the page and
 * deform a column only when a consumer asks for it. The node evaluates no
 * clause: a filter above takes the relation's clauses, or the relation
 * has none. See docs/nodes.md.
 */
#define HEAP_SCAN_BATCH_ROWS 64

typedef struct HeapScanState
{
	CustomScanState css;
	TessOutput *output;
	TessLayout	layout;
	/* The slot the core's scan returns its page's first tuple in. */
	TupleTableSlot *landing;
	/* Begun at the first execution. */
	TableScanDesc scan;
	TessHeapBatch *heap;
	const TessRequest *request;
	int			capacity;
	/* The scan collects a page's visible tuples; otherwise one at a time. */
	bool		pagemode;
	bool		page_active;
	/* The next entry of the page's visible tuples to take. */
	int			page_cursor;
	bool		exhausted;
	/* Rows the parent needs at most in this scan, or -1; rows given so far. */
	int64		tuples_needed;
	int64		produced;
	uint64		batches;
	uint64		pages;
} HeapScanState;

static CustomPath *heap_scan_rows(PlannerInfo *root, Path *path);
static Plan *heap_scan_plan(PlannerInfo *root, RelOptInfo *rel,
							CustomPath *best_path, List *tlist, List *clauses,
							List *custom_plans);
static Node *heap_scan_create_state(CustomScan *cscan);
static void heap_scan_begin(CustomScanState *css, EState *estate, int eflags);
static TupleTableSlot *heap_scan_exec(CustomScanState *css);
static void heap_scan_end(CustomScanState *css);
static void heap_scan_rescan(CustomScanState *css);
static void heap_scan_explain(CustomScanState *css, List *ancestors,
							  ExplainState *es);

static const CustomPathMethods heap_scan_path_methods = {
	.CustomName = "TessHeapScan",
	.PlanCustomPath = heap_scan_plan,
};

const CustomScanMethods tess_heap_scan_scan_methods = {
	.CustomName = "TessHeapScan",
	.CreateCustomScanState = heap_scan_create_state,
};

static const CustomExecMethods heap_scan_exec_methods = {
	.CustomName = "TessHeapScan",
	.BeginCustomScan = heap_scan_begin,
	.ExecCustomScan = heap_scan_exec,
	.EndCustomScan = heap_scan_end,
	.ReScanCustomScan = heap_scan_rescan,
	.ExplainCustomScan = heap_scan_explain,
};

/* The parent needs at most tuples_needed rows: read no more than that. */
static void
heap_scan_set_tuple_bound(CustomScanState *css, int64 tuples_needed)
{
	HeapScanState *state = (HeapScanState *) css;

	state->tuples_needed = tuples_needed < 0 ? -1 : tuples_needed;
}

const TessNode tess_heap_scan_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_HEAP_SCAN_NODE_NAME,
	.set_tuple_bound = heap_scan_set_tuple_bound,
	.scan_rows = heap_scan_rows,
};

/*
 * Whether the path is a sequential scan of a plain heap table whose
 * targets are all columns of it. A stand-in path of a test has neither a
 * relation nor a target.
 */
static bool
plain_heap_scan(PlannerInfo *root, const Path *path)
{
	RelOptInfo *rel = path->parent;
	RangeTblEntry *rte;
	Relation	relation;
	bool		heap;

	if (root == NULL || rel == NULL || path->pathtarget == NULL ||
		path->pathtype != T_SeqScan || rel->reloptkind != RELOPT_BASEREL ||
		rel->rtekind != RTE_RELATION)
		return false;
	rte = planner_rt_fetch(rel->relid, root);
	if (rte->relkind != RELKIND_RELATION || rte->inh ||
		rte->tablesample != NULL)
		return false;
	foreach_ptr(Expr, expr, path->pathtarget->exprs)
	{
		Var		   *var = (Var *) expr;

		if (!IsA(expr, Var) || var->varno != rel->relid ||
			var->varattno <= 0 || var->varlevelsup != 0)
			return false;
	}
	/* The planner holds the lock; the executor reads pages as the heap AM. */
	relation = table_open(rte->relid, NoLock);
	heap = relation->rd_tableam == GetHeapamTableAmRoutine();
	table_close(relation, NoLock);
	return heap;
}

/* The path costs what the scan costs: there is no cost model yet. */
static CustomPath *
heap_scan_rows(PlannerInfo *root, Path *path)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	CustomPath *scan;

	if (!plain_heap_scan(root, path))
		return NULL;
	config.template_path = path;
	config.methods = &heap_scan_path_methods;
	config.node = &tess_heap_scan_node;
	scan = tess_path_create(&config);
	/* Every row of the relation comes out: the node evaluates no clause. */
	scan->path.rows = clamp_row_est(path->parent->tuples);
	return scan;
}

/* Every relation column is a batch column; the targets map to them. */
static Plan *
heap_scan_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
			   List *tlist, List *clauses, List *custom_plans)
{
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);
	int		   *map = NULL;
	int			target = 0;

	if (tlist != NIL)
		map = palloc_array(int, list_length(tlist));
	foreach_ptr(TargetEntry, entry, tlist)
	{
		if (!IsA(entry->expr, Var))
			elog(ERROR, "TessHeapScan target is not a column");
		map[target++] = ((Var *) entry->expr)->varattno - 1;
	}
	layout.ncolumns = rel->max_attr;
	layout.ntargets = list_length(tlist);
	layout.target_columns = map;
	config.methods = &tess_heap_scan_scan_methods;
	config.layout_policy = TESS_LAYOUT_EXPLICIT;
	config.explicit_layout = &layout;
	config.scanrelid = rel->relid;
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

static Node *
heap_scan_create_state(CustomScan *cscan)
{
	HeapScanState *state = (HeapScanState *)
		newNode(sizeof(HeapScanState), T_CustomScanState);

	state->css.methods = &heap_scan_exec_methods;
	return (Node *) state;
}

static void
heap_scan_begin(CustomScanState *css, EState *estate, int eflags)
{
	HeapScanState *state = (HeapScanState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	Relation	rel = css->ss.ss_currentRelation;

	/* The planner puts Material above a batch subtree for these. */
	if (eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK))
		elog(ERROR, "TessHeapScan supports neither backward scan nor mark/restore");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_heap_scan_node || info.nchildren != 0 ||
		rel == NULL || info.layout.ncolumns != RelationGetDescr(rel)->natts)
		elog(ERROR, "TessHeapScan received a foreign plan");
	state->layout = info.layout;
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   css->ss.ps.ps_ResultTupleSlot,
									   &info.layout);
	state->landing = table_slot_create(rel, &estate->es_tupleTable);
	state->tuples_needed = -1;
}

/* Freeze the parent's request, begin the scan and create the provider. */
static void
heap_scan_start(HeapScanState *state)
{
	EState	   *estate = state->css.ss.ps.state;
	Relation	rel = state->css.ss.ss_currentRelation;
	TessHeapBatchConfig config = TESS_STRUCT_INITIALIZER(TessHeapBatchConfig);
	uint32		flags = SO_NONE;

	state->request = tess_output_request(state->output);
	state->capacity = state->request->max_batch_rows > 0 ?
		Min(state->request->max_batch_rows, HEAP_SCAN_BATCH_ROWS) :
		HEAP_SCAN_BATCH_ROWS;
	if (state->request->output_mode != TESS_OUTPUT_BATCH)
		elog(ERROR, "TessHeapScan requires a batch-aware parent");
	if (ScanRelIsReadOnly(&state->css.ss))
		flags |= SO_HINT_REL_READ_ONLY;
	state->scan = table_beginscan(rel, estate->es_snapshot, 0, NULL, flags);
	/* Cleared for a non-MVCC snapshot: then one tuple at a time. */
	state->pagemode = (state->scan->rs_flags & SO_ALLOW_PAGEMODE) != 0;
	config.parent_context = estate->es_query_cxt;
	config.ncolumns = state->layout.ncolumns;
	config.capacity = state->capacity;
	config.tuple_desc = RelationGetDescr(rel);
	config.first_non_guaranteed_attr = RelationGetDescr(rel)->firstNonGuaranteedAttr;
	state->heap = tess_heap_batch_create(&config);
}

/*
 * Move the core's scan to the next page with visible tuples: it brings
 * the page in, prunes it, checks visibility and returns the first visible
 * tuple, which is left in the page's list for the batch. False at the end.
 */
static bool
next_page(HeapScanState *state)
{
	HeapScanDesc hscan = (HeapScanDesc) state->scan;

	/* Skip the rest of the current page: the next call fetches another. */
	if (state->page_active)
		hscan->rs_cindex = hscan->rs_ntuples - 1;
	if (!heap_getnextslot(state->scan, ForwardScanDirection, state->landing))
		return false;
	/* The scan keeps its own pin on the page; the slot's is not needed. */
	ExecClearTuple(state->landing);
	state->page_active = true;
	state->page_cursor = hscan->rs_cindex;
	state->pages++;
	return true;
}

/* Take up to limit visible tuples of the current page into the batch. */
static void
fill_from_page(HeapScanState *state, int limit)
{
	HeapScanDesc hscan = (HeapScanDesc) state->scan;
	Relation	rel = state->css.ss.ss_currentRelation;
	Page		page = BufferGetPage(hscan->rs_cbuf);
	int			nrows = Min(limit, hscan->rs_ntuples - state->page_cursor);

	for (int index = 0; index < nrows; index++)
	{
		int			entry = state->page_cursor + index;
		OffsetNumber offset = hscan->rs_vistuples[entry];
		ItemId		item = PageGetItemId(page, offset);
		HeapTupleData tuple;

		tuple.t_len = ItemIdGetLength(item);
		tuple.t_data = (HeapTupleHeader) PageGetItem(page, item);
		ItemPointerSet(&tuple.t_self, hscan->rs_cblock, offset);
		tuple.t_tableOid = RelationGetRelid(rel);
		tess_heap_batch_append_tuple(state->heap, &tuple, hscan->rs_cbuf);
		/* The core counted the page's first visible tuple itself. */
		if (entry > 0)
			pgstat_count_heap_getnext(rel);
	}
	state->page_cursor += nrows;
}

static TupleTableSlot *
heap_scan_exec(CustomScanState *css)
{
	HeapScanState *state = (HeapScanState *) css;
	HeapScanDesc hscan;
	TessBatch  *batch;
	int			limit;

	if (state->request == NULL)
		heap_scan_start(state);
	if (!ScanDirectionIsForward(css->ss.ps.state->es_direction))
		elog(ERROR, "TessHeapScan supports only forward scans");
	/* Refuses while the parent has not finished the previous batch. */
	tess_output_release(state->output);
	if (state->exhausted)
		return NULL;
	/* A bounded parent never gets more rows than it asked for. */
	limit = state->capacity;
	if (state->tuples_needed >= 0)
	{
		if (state->produced >= state->tuples_needed)
			return NULL;
		limit = (int) Min((int64) limit, state->tuples_needed - state->produced);
	}
	tess_heap_batch_reset(state->heap);
	hscan = (HeapScanDesc) state->scan;
	if (state->pagemode)
	{
		/* A batch takes the rows of one page, never of two. */
		if (!state->page_active || state->page_cursor >= hscan->rs_ntuples)
		{
			if (!next_page(state))
			{
				state->exhausted = true;
				return NULL;
			}
		}
		fill_from_page(state, limit);
	}
	else
	{
		while (limit > 0 && !tess_heap_batch_is_full(state->heap))
		{
			if (!heap_getnextslot(state->scan, ForwardScanDirection,
								  state->landing))
			{
				state->exhausted = true;
				break;
			}
			tess_heap_batch_append_slot(state->heap, state->landing);
			limit--;
		}
	}
	batch = tess_heap_batch_finish(state->heap,
								   RelationGetRelid(css->ss.ss_currentRelation));
	if (batch == NULL)
		return NULL;
	state->produced += tess_row_mask_count(&batch->rows);
	state->batches++;
	return tess_output_publish(state->output, batch);
}

static void
heap_scan_end(CustomScanState *css)
{
	HeapScanState *state = (HeapScanState *) css;

	tess_output_end(state->output);
	ExecClearTuple(state->landing);
	if (state->scan != NULL)
		table_endscan(state->scan);
	/* The relation is closed by the executor. */
}

static void
heap_scan_rescan(CustomScanState *css)
{
	HeapScanState *state = (HeapScanState *) css;

	tess_output_clear(state->output);
	ExecClearTuple(state->landing);
	if (state->scan != NULL)
		table_rescan(state->scan, NULL);
	state->page_active = false;
	state->page_cursor = 0;
	state->exhausted = false;
	state->produced = 0;
	state->batches = 0;
	state->pages = 0;
}

static void
heap_scan_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	HeapScanState *state = (HeapScanState *) css;

	/* The parent's request, and so the size, is known once executed. */
	if (state->request != NULL)
		ExplainPropertyInteger("Batch Size", NULL, state->capacity, es);
	if (!es->analyze)
		return;
	ExplainPropertyInteger("Batches", NULL, state->batches, es);
	ExplainPropertyInteger("Pages", NULL, state->pages, es);
	if (state->heap != NULL)
	{
		const TessHeapBatchStats *stats = tess_heap_batch_stats(state->heap);

		ExplainPropertyInteger("Deformed Datums", NULL, stats->deformed_datums, es);
		ExplainPropertyInteger("Restarted Datums", NULL, stats->restarted_datums, es);
	}
}

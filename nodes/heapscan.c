#include "postgres.h"

#include "access/heapam.h"
#include "access/parallel.h"
#include "access/relscan.h"
#include "access/table.h"
#include "access/tableam.h"
#include "catalog/pg_am_d.h"
#include "catalog/pg_statistic.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "nodes/tidbitmap.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/plancat.h"
#include "parser/parsetree.h"
#include "pgstat.h"
#include "storage/bufmgr.h"
#include "storage/bufpage.h"
#include "storage/shm_toc.h"
#include "utils/lsyscache.h"
#include "utils/rel.h"
#include "utils/syscache.h"

#include "tessera/runtime.h"

#include "internal.h"

/*
 * TessHeapScan reads a heap relation in batches, for a batch-aware parent
 * or, row by row from each batch, for any other.
 * It lets the core's scan bring one page at a time into memory, prune it
 * and decide which tuples are visible, then publishes the page's visible
 * tuples as heap batches, up to 64 rows each, that pin the page and
 * deform a column only when a consumer asks for it. The node evaluates no
 * clause: a filter above takes the relation's clauses, or the relation
 * has none. A row-wise parent gets each batch's rows from the columns of
 * its targets, taken once per batch: the page is still pinned once and a
 * column deformed only when a target reads it, which makes the node
 * faster than the core's scan there too (docs/nodes.md). Under a Gather,
 * the participants share the core's parallel
 * scan descriptor, which hands each of them its own pages, and the
 * leader reports the counters of all of them. With one child, the core's
 * plan of a bitmap of an index (Bitmap Index Scan, BitmapAnd, BitmapOr),
 * the node reads the pages of that bitmap instead of all of them: the
 * core's bitmap scan brings each in, finds its visible tuples (those the
 * bitmap names, or all of a lossy page's) and leaves them in the page's
 * list, as its page-at-a-time scan does, and the batches are taken from
 * it the same way; a filter above evaluates every clause of the
 * relation, the index's recheck among them. With the core's index scan
 * as its child, stripped of clauses and projection, the node takes the
 * rows that scan returns, in the index's order, into batches of rows of
 * any pages, each pinned by the batch: the core keeps the index's keys,
 * parameters and rechecks, the node saves the slot's rows their copies,
 * deforms the columns asked for and lets the filter above run in batches.
 * See docs/nodes.md.
 */
#define HEAP_SCAN_BATCH_ROWS 64

/* The counters every participant of a parallel scan shares. */
enum
{
	HEAP_SCAN_RAN,
	HEAP_SCAN_CAPACITY,
	HEAP_SCAN_BATCHES,
	HEAP_SCAN_PAGES,
	HEAP_SCAN_DEFORMED,
	HEAP_SCAN_RESTARTED,
	HEAP_SCAN_COMPUTED,
	HEAP_SCAN_EXACT,
	HEAP_SCAN_LOSSY,
	HEAP_SCAN_NCOUNTERS
};

typedef struct HeapScanState
{
	CustomScanState css;
	TessOutput *output;
	TessLayout	layout;
	/* The slot the core's scan returns its page's first tuple in. */
	TupleTableSlot *landing;
	/* Begun in the shared memory callbacks, or at the first execution. */
	TableScanDesc scan;
	/* The counters of every participant, in a parallel plan. */
	TessSharedStats *stats;
	TessHeapBatch *heap;
	/* The targets PostgreSQL asks the node to compute, or NULL. */
	TessProjection *projection;
	/* The relation's row as the scan tuple: attribute n is column n. */
	TessLayout	relation;
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
	/* A row-wise parent: the batch being served, its next row, its targets' columns. */
	bool		rows;
	TessBatch  *active;
	int			next_row;
	TessDatumColumn *columns;
	/*
	 * Bitmap mode: the plan of the bitmap, the bitmap once built, and the
	 * pages read whole (lossy) and by their tuples (exact).
	 */
	PlanState  *bitmap_plan;
	TIDBitmap  *tbm;
	/* Index mode: the core's index scan, whose rows the node takes. */
	PlanState  *index_plan;
	uint64		exact_pages;
	uint64		lossy_pages;
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
static Size heap_scan_estimate_dsm(CustomScanState *css, ParallelContext *pcxt);
static void heap_scan_initialize_dsm(CustomScanState *css, ParallelContext *pcxt,
									 void *coordinate);
static void heap_scan_reinitialize_dsm(CustomScanState *css,
									   ParallelContext *pcxt, void *coordinate);
static void heap_scan_initialize_worker(CustomScanState *css, shm_toc *toc,
										void *coordinate);
static void heap_scan_shutdown(CustomScanState *css);

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
	.EstimateDSMCustomScan = heap_scan_estimate_dsm,
	.InitializeDSMCustomScan = heap_scan_initialize_dsm,
	.ReInitializeDSMCustomScan = heap_scan_reinitialize_dsm,
	.InitializeWorkerCustomScan = heap_scan_initialize_worker,
	.ShutdownCustomScan = heap_scan_shutdown,
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
 * Whether the relation is a plain heap table and the target columns of it
 * or expressions over them, which the node computes.
 */
static bool
plain_heap_relation(PlannerInfo *root, RelOptInfo *rel, const PathTarget *target)
{
	RangeTblEntry *rte;
	Relation	relation;
	bool		heap;

	if (root == NULL || rel == NULL || target == NULL || !IS_SIMPLE_REL(rel) ||
		rel->rtekind != RTE_RELATION)
		return false;
	rte = planner_rt_fetch(rel->relid, root);
	if (rte->relkind != RELKIND_RELATION || rte->inh ||
		rte->tablesample != NULL)
		return false;
	foreach_ptr(Var, var, pull_var_clause((Node *) target->exprs,
										  PVC_RECURSE_AGGREGATES |
										  PVC_RECURSE_WINDOWFUNCS |
										  PVC_RECURSE_PLACEHOLDERS))
	{
		if (var->varno != rel->relid || var->varattno <= 0 ||
			var->varlevelsup != 0)
			return false;
	}
	/* The planner holds the lock; the executor reads pages as the heap AM. */
	relation = table_open(rte->relid, NoLock);
	heap = relation->rd_tableam == GetHeapamTableAmRoutine();
	table_close(relation, NoLock);
	return heap;
}

/*
 * Whether the path is a sequential scan of a plain heap table. A stand-in
 * path of a test has neither a relation nor a target.
 */
static bool
plain_heap_scan(PlannerInfo *root, const Path *path)
{
	return path->pathtype == T_SeqScan &&
		plain_heap_relation(root, path->parent, path->pathtarget);
}

/* The core's get_parallel_divisor: the share of one participant. */
double
tess_parallel_divisor(const Path *path)
{
	double		divisor = path->parallel_workers;

	if (parallel_leader_participation)
	{
		double		leader_contribution = 1.0 - 0.3 * path->parallel_workers;

		if (leader_contribution > 0)
			divisor += leader_contribution;
	}
	return divisor;
}

/*
 * The path costs what the scan costs: there is no cost model yet. A
 * partial scan's path is parallel-aware, as the template is, and gives
 * each participant its share of the rows.
 */
static CustomPath *
heap_scan_rows(PlannerInfo *root, Path *path)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	CustomPath *scan;
	double		rows = path->parent->tuples;

	if (!plain_heap_scan(root, path))
		return NULL;
	config.template_path = path;
	config.methods = &heap_scan_path_methods;
	config.node = &tess_heap_scan_node;
	/* Expressions in the targets are computed over the batches. */
	config.flags = CUSTOMPATH_SUPPORT_PROJECTION;
	scan = tess_path_create(&config);
	/* Every row of the relation comes out: the node evaluates no clause. */
	if (path->parallel_workers > 0)
		rows /= tess_parallel_divisor(path);
	scan->path.rows = clamp_row_est(rows);
	return scan;
}

/*
 * The node's scan of the pages of a bitmap in place of the core's bitmap
 * heap scan, with the target given (the relation's columns and the
 * clauses'): the core's path, which the caller copied (add_path frees a
 * path another dominates), is the child, whose plan's bitmap the node
 * takes; its rows are the tuples the bitmap's pages give, before any
 * clause. NULL for a relation the node does not read, or for a bitmap
 * whose pages the planner expects to give fewer than two tuples each:
 * then a page costs the node a pin of its own besides the one the core's
 * scan takes, which its batches do not make up for (a bitmap of 1.4
 * tuples a page, 14 500 pages, took 9 % more than the core's scan). The
 * bound is tessera.bitmap_page_rows, 2 by default.
 */

Path *
tess_heap_bitmap_path(PlannerInfo *root, BitmapHeapPath *bitmap, PathTarget *target)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	RelOptInfo *rel = bitmap->path.parent;
	Path		template = bitmap->path;
	CustomPath *scan;
	double		tuples;

	double		pages;

	if (!plain_heap_relation(root, rel, target))
		return NULL;
	pages = compute_bitmap_pages(root, rel, bitmap->bitmapqual, 1.0, NULL, &tuples);
	if (pages <= 0 || tuples / pages < tess_bitmap_page_rows)
		return NULL;
	template.pathtarget = target;
	template.pathkeys = NIL;
	config.template_path = &template;
	config.methods = &heap_scan_path_methods;
	config.node = &tess_heap_scan_node;
	config.flags = CUSTOMPATH_SUPPORT_PROJECTION;
	config.children = list_make1(bitmap);
	scan = tess_path_create(&config);
	scan->path.rows = clamp_row_est(tuples);
	return &scan->path;
}

/*
 * The correlation of a btree index's order with the table's, as the
 * core's btcost_correlation takes it: its first column's, from the
 * statistics, three quarters of it for several columns; 0 when unknown.
 */
static double
index_correlation(PlannerInfo *root, IndexOptInfo *index)
{
	RangeTblEntry *rte = planner_rt_fetch(index->rel->relid, root);
	HeapTuple	stats;
	AttStatsSlot slot;
	Oid			sortop;
	double		correlation = 0;

	if (index->relam != BTREE_AM_OID || index->nkeycolumns < 1 ||
		index->indexkeys[0] <= 0)
		return 0;
	stats = SearchSysCache3(STATRELATTINH, ObjectIdGetDatum(rte->relid),
							Int16GetDatum(index->indexkeys[0]), BoolGetDatum(rte->inh));
	if (!HeapTupleIsValid(stats))
		return 0;
	sortop = get_opfamily_member(index->opfamily[0], index->opcintype[0],
								 index->opcintype[0], BTLessStrategyNumber);
	if (OidIsValid(sortop) &&
		get_attstatsslot(&slot, stats, STATISTIC_KIND_CORRELATION, sortop,
						 ATTSTATSSLOT_NUMBERS))
	{
		correlation = fabs(slot.numbers[0]) * (index->nkeycolumns > 1 ? 0.75 : 1.0);
		free_attstatsslot(&slot);
	}
	ReleaseSysCache(stats);
	return correlation;
}

/*
 * An index scan the node takes: one whose rows come mostly in runs of a
 * page, where the batch pins a page once for its run (an index of a
 * scattered column pinned a page a row besides the core scan's pin: 21 %
 * more than the core's), and many of them, since the node's setup costs
 * each query a few microseconds more (one row of an index took 11 against
 * 6, the first 10 of an order 12 against 8): at least
 * tessera.index_min_correlation (0.8) and tessera.index_min_rows (1000).
 */

/*
 * The node over the core's index scan, in its order, with the target
 * given: the core's path, which the caller copied, is the child, whose
 * plan the node keeps without clauses and projection; its rows are the
 * tuples the index's conditions select, before any other clause. NULL for
 * a relation the node does not read, an ordering by distance, or a scan
 * the node does not take (above).
 */
Path *
tess_heap_index_path(PlannerInfo *root, IndexPath *index, PathTarget *target)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	RelOptInfo *rel = index->path.parent;
	Path		template = index->path;
	CustomPath *scan;

	double		rows = index->indexselectivity * rel->tuples;

	if (root->limit_tuples >= 0)
		rows = Min(rows, root->limit_tuples);
	if (index->path.pathtype != T_IndexScan || index->indexorderbys != NIL ||
		!plain_heap_relation(root, rel, target) || rows < tess_index_min_rows ||
		(tess_index_min_correlation > 0 &&
		 index_correlation(root, index->indexinfo) < tess_index_min_correlation))
		return NULL;
	template.pathtarget = target;
	config.template_path = &template;
	config.methods = &heap_scan_path_methods;
	config.node = &tess_heap_scan_node;
	config.flags = CUSTOMPATH_SUPPORT_PROJECTION;
	config.children = list_make1(index);
	scan = tess_path_create(&config);
	scan->path.rows = clamp_row_est(index->indexselectivity * rel->tuples);
	return &scan->path;
}

/*
 * Every relation column is a batch column and the relation's row is the
 * scan tuple; the targets, PostgreSQL's projection among them, are derived
 * from it when the plan is read. A bitmap's scan keeps the bitmap's plan
 * of the core's bitmap heap scan as its child, and none of the rest; an
 * index scan below is kept whole but for its clauses, which the filter
 * above evaluates, and its projection: its targets are the relation's
 * columns, so that it returns the tuple as the page holds it.
 */
static Plan *
heap_scan_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
			   List *tlist, List *clauses, List *custom_plans)
{
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);

	if (custom_plans != NIL)
	{
		Plan	   *bitmap = linitial(custom_plans);

		if (list_length(custom_plans) == 1 && IsA(bitmap, IndexScan))
		{
			bitmap->qual = NIL;
			bitmap->targetlist = build_physical_tlist(root, rel);
		}
		else if (list_length(custom_plans) != 1 || !IsA(bitmap, BitmapHeapScan) ||
				 outerPlan(bitmap) == NULL)
			elog(ERROR, "TessHeapScan expected the core's bitmap heap scan below");
		else
			custom_plans = list_make1(outerPlan(bitmap));
	}

	layout.ncolumns = rel->max_attr;
	layout.ntargets = rel->max_attr;
	config.methods = &tess_heap_scan_scan_methods;
	config.layout_policy = TESS_LAYOUT_PROJECTED;
	config.explicit_layout = &layout;
	config.scanrelid = rel->relid;
	config.scan_tuple_is_relation = true;
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
	if (info.node != &tess_heap_scan_node || info.nchildren > 1 ||
		rel == NULL || cscan->custom_scan_tlist != NIL ||
		info.layout.ncolumns < RelationGetDescr(rel)->natts)
		elog(ERROR, "TessHeapScan received a foreign plan");
	/*
	 * The bitmap's plan (an index's bitmap scan, or their AND or OR), or
	 * the core's index scan.
	 */
	if (info.nchildren == 1)
	{
		Plan	   *child = linitial(cscan->custom_plans);

		if (IsA(child, IndexScan))
		{
			state->index_plan = ExecInitNode(child, estate, eflags);
			css->custom_ps = list_make1(state->index_plan);
		}
		else
		{
			state->bitmap_plan = ExecInitNode(child, estate, eflags);
			css->custom_ps = list_make1(state->bitmap_plan);
		}
	}
	state->layout = info.layout;
	state->relation = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	state->relation.ncolumns = RelationGetDescr(rel)->natts;
	state->relation.ntargets = RelationGetDescr(rel)->natts;
	if (info.computed != NIL)
	{
		TessProjectionConfig projection = TESS_STRUCT_INITIALIZER(TessProjectionConfig);

		projection.parent_context = estate->es_query_cxt;
		projection.parent = &css->ss.ps;
		projection.econtext = css->ss.ps.ps_ExprContext;
		projection.scan_slot = css->ss.ss_ScanTupleSlot;
		projection.scan_tuple = &state->relation;
		projection.base_columns = state->relation.ncolumns;
		projection.computed = info.computed;
		state->projection = tess_projection_create(&projection);
	}
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   css->ss.ps.ps_ResultTupleSlot,
									   &info.layout);
	state->landing = table_slot_create(rel, &estate->es_tupleTable);
	state->tuples_needed = -1;
}

/* Freeze the parent's request and create the provider. */
static void
heap_scan_start(HeapScanState *state)
{
	EState	   *estate = state->css.ss.ps.state;
	Relation	rel = state->css.ss.ss_currentRelation;
	TessHeapBatchConfig config = TESS_STRUCT_INITIALIZER(TessHeapBatchConfig);

	state->request = tess_output_request(state->output);
	state->capacity = state->request->max_batch_rows > 0 ?
		Min(state->request->max_batch_rows, HEAP_SCAN_BATCH_ROWS) :
		HEAP_SCAN_BATCH_ROWS;
	state->rows = state->request->output_mode == TESS_OUTPUT_ROWS;
	if (state->rows)
		state->columns = palloc0_array(TessDatumColumn,
									   Max(state->layout.ntargets, 1));
	config.parent_context = estate->es_query_cxt;
	config.ncolumns = state->relation.ncolumns;
	config.capacity = state->capacity;
	config.tuple_desc = RelationGetDescr(rel);
	config.first_non_guaranteed_attr = RelationGetDescr(rel)->firstNonGuaranteedAttr;
	state->heap = tess_heap_batch_create(&config);
}

static uint32
heap_scan_flags(HeapScanState *state)
{
	return ScanRelIsReadOnly(&state->css.ss) ? SO_HINT_REL_READ_ONLY : SO_NONE;
}

/* Take the scan the core began: serial, or a participant's parallel one. */
static void
begin_scan(HeapScanState *state, TableScanDesc scan)
{
	state->scan = scan;
	/* Cleared for a non-MVCC snapshot: then one tuple at a time. */
	state->pagemode = (scan->rs_flags & SO_ALLOW_PAGEMODE) != 0;
}

/*
 * Bitmap mode: build the bitmap from the child's plan and begin the
 * core's bitmap scan over its pages, which always collects a page's
 * visible tuples.
 */
static void
begin_bitmap_scan(HeapScanState *state)
{
	EState	   *estate = state->css.ss.ps.state;
	TableScanDesc scan;

	state->tbm = (TIDBitmap *) MultiExecProcNode(state->bitmap_plan);
	if (state->tbm == NULL || !IsA(state->tbm, TIDBitmap))
		elog(ERROR, "TessHeapScan bitmap child returned an invalid result");
	scan = table_beginscan_bm(state->css.ss.ss_currentRelation, estate->es_snapshot,
							  0, NULL, heap_scan_flags(state));
	scan->st.rs_tbmiterator = tbm_begin_iterate(state->tbm, estate->es_query_dsa,
												InvalidDsaPointer);
	begin_scan(state, scan);
	state->pagemode = true;
}

/* Drop the batch's pins and the landing slot's, then the scan and its bitmap. */
static void
end_scan(HeapScanState *state)
{
	if (state->heap != NULL)
		tess_heap_batch_reset(state->heap);
	ExecClearTuple(state->landing);
	if (state->scan != NULL)
	{
		if (state->bitmap_plan != NULL &&
			!tbm_exhausted(&state->scan->st.rs_tbmiterator))
			tbm_end_iterate(&state->scan->st.rs_tbmiterator);
		table_endscan(state->scan);
	}
	state->scan = NULL;
	if (state->tbm != NULL)
		tbm_free(state->tbm);
	state->tbm = NULL;
	state->page_active = false;
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

	if (state->bitmap_plan != NULL)
	{
		bool		recheck;

		/*
		 * The next page of the bitmap with a visible tuple; the scan steps
		 * past the one it returns. A filter above rechecks every row.
		 */
		if (state->page_active)
			hscan->rs_cindex = hscan->rs_ntuples;
		if (!table_scan_bitmap_next_tuple(state->scan, state->landing, &recheck,
										  &state->lossy_pages, &state->exact_pages))
			return false;
		ExecClearTuple(state->landing);
		state->page_active = true;
		state->page_cursor = hscan->rs_cindex - 1;
		state->pages++;
		return true;
	}
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

/* Take up to limit visible tuples of the current page into the batch; how many. */
static int
fill_from_page(HeapScanState *state, int limit)
{
	HeapScanDesc hscan = (HeapScanDesc) state->scan;
	Relation	rel = state->css.ss.ss_currentRelation;
	int			nrows = Min(limit, hscan->rs_ntuples - state->page_cursor);

	tess_heap_batch_append_page(state->heap, hscan->rs_cbuf, hscan->rs_cblock,
								hscan->rs_vistuples + state->page_cursor, nrows,
								RelationGetRelid(rel));
	/*
	 * As pgstat_count_heap_getnext, or pgstat_count_heap_fetch for a
	 * bitmap's tuples; the core counted the page's first tuple.
	 */
	if (pgstat_should_count_relation(rel))
	{
		int			counted = state->page_cursor > 0 ? nrows : nrows - 1;

		Assert(rel->pgstat_info->kind == PGSTAT_KIND_RELATION);
		if (state->bitmap_plan != NULL)
			rel->pgstat_info->tab.counts.tuples_fetched += counted;
		else
			rel->pgstat_info->tab.counts.tuples_returned += counted;
	}
	state->page_cursor += nrows;
	return nrows;
}

/* The next batch of the relation's rows, or NULL at the end. */
static TessBatch *
next_batch(HeapScanState *state)
{
	CustomScanState *css = &state->css;
	HeapScanDesc hscan;
	TessBatch  *batch;
	int			limit;

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
	if (state->index_plan != NULL)
	{
		/*
		 * The index scan's rows, in its order, from any pages: the batch
		 * keeps each row's tuple where it lies, its page pinned once for
		 * the rows of it that come in a run.
		 */
		while (limit > 0)
		{
			TupleTableSlot *slot = ExecProcNode(state->index_plan);

			if (TupIsNull(slot))
			{
				state->exhausted = true;
				break;
			}
			tess_heap_batch_append_slot(state->heap, slot);
			limit--;
		}
	}
	else if (state->bitmap_plan != NULL)
	{
		/*
		 * A bitmap's pages may give a few rows each: a batch takes them
		 * from page after page, each page pinned by the batch, up to its
		 * capacity; one page a batch made 1.6 rows a batch of a sparse
		 * bitmap, where every batch's cost fell on its few rows.
		 */
		while (limit > 0)
		{
			if (!state->page_active || state->page_cursor >= hscan->rs_ntuples)
			{
				if (!next_page(state))
				{
					state->exhausted = true;
					break;
				}
			}
			limit -= fill_from_page(state, limit);
		}
	}
	else if (state->pagemode)
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
	if (state->projection != NULL)
		batch = tess_projection_wrap(state->projection, batch);
	return batch;
}

/* The column of every target, for the batch's rows: deformed or computed once. */
static void
fetch_columns(HeapScanState *state, TessBatch *batch)
{
	for (int target = 0; target < state->layout.ntargets; target++)
	{
		TessDatumColumn *column = &state->columns[target];

		*column = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
		batch->ops->get_datum_column(batch, tess_layout_column(&state->layout, target),
									 &batch->rows, TESS_COLUMN_FOR_PROJECTION, column);
		if (column->values == NULL || column->isnull == NULL ||
			column->nrows != batch->rows.nrows)
			elog(ERROR, "TessHeapScan batch returned an invalid column");
	}
}

/* Forget the batch being served: the wrapper's values go with it. */
static void
drop_active(HeapScanState *state)
{
	if (state->active != NULL && state->projection != NULL)
		state->active->ops->release(state->active);
	state->active = NULL;
}

/*
 * A row-wise parent: the next row of the batch being served, in the
 * node's slot, which the parent reads before it asks for another; the
 * batch's pins go when the next batch is read.
 */
static TupleTableSlot *
exec_rows(HeapScanState *state)
{
	TupleTableSlot *slot = state->css.ss.ps.ps_ResultTupleSlot;

	for (;;)
	{
		int			row;

		if (state->active == NULL)
		{
			TessBatch  *batch = next_batch(state);

			if (batch == NULL)
				return NULL;
			fetch_columns(state, batch);
			state->active = batch;
			state->next_row = tess_row_mask_next(&batch->rows, -1);
		}
		if (state->next_row < 0)
		{
			drop_active(state);
			continue;
		}
		row = state->next_row;
		state->next_row = tess_row_mask_next(&state->active->rows, row);
		ExecClearTuple(slot);
		for (int target = 0; target < state->layout.ntargets; target++)
		{
			slot->tts_values[target] = state->columns[target].values[row];
			slot->tts_isnull[target] = state->columns[target].isnull[row];
		}
		slot->tts_tableOid = state->active->table_oid;
		return ExecStoreVirtualTuple(slot);
	}
}

static TupleTableSlot *
heap_scan_exec(CustomScanState *css)
{
	HeapScanState *state = (HeapScanState *) css;
	TessBatch  *batch;

	if (state->request == NULL)
		heap_scan_start(state);
	/* Without a parallel scan from the callbacks, a serial one. */
	if (state->scan == NULL && state->bitmap_plan != NULL)
		begin_bitmap_scan(state);
	else if (state->scan == NULL && state->index_plan == NULL)
	{
		EState	   *estate = css->ss.ps.state;

		begin_scan(state, table_beginscan(css->ss.ss_currentRelation,
										  estate->es_snapshot, 0, NULL,
										  heap_scan_flags(state)));
	}
	if (!ScanDirectionIsForward(css->ss.ps.state->es_direction))
		elog(ERROR, "TessHeapScan supports only forward scans");
	if (state->rows)
		return exec_rows(state);
	/* Refuses while the parent has not finished the previous batch. */
	tess_output_release(state->output);
	batch = next_batch(state);
	if (batch == NULL)
		return NULL;
	return tess_output_publish(state->output, batch);
}

static void
heap_scan_end(CustomScanState *css)
{
	HeapScanState *state = (HeapScanState *) css;

	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	drop_active(state);
	tess_output_end(state->output);
	/* The pins of a batch a projection wrapped are the node's to drop. */
	end_scan(state);
	if (state->bitmap_plan != NULL)
		ExecEndNode(state->bitmap_plan);
	if (state->index_plan != NULL)
		ExecEndNode(state->index_plan);
	/* The relation is closed by the executor. */
}

static void
heap_scan_rescan(CustomScanState *css)
{
	HeapScanState *state = (HeapScanState *) css;

	tess_output_clear(state->output);
	drop_active(state);
	if (state->projection != NULL)
		tess_projection_reset(state->projection);
	if (state->heap != NULL)
		tess_heap_batch_reset(state->heap);
	ExecClearTuple(state->landing);
	/*
	 * Bitmap mode: the bitmap is built again at the next execution, from
	 * the child rescanned, now or by its first execution when a parameter
	 * of it changed (the core passes those to outer and inner plans only).
	 */
	if (state->bitmap_plan != NULL)
	{
		end_scan(state);
		if (css->ss.ps.chgParam != NULL)
			UpdateChangedParamSet(state->bitmap_plan, css->ss.ps.chgParam);
		if (state->bitmap_plan->chgParam == NULL)
			ExecReScan(state->bitmap_plan);
		state->exact_pages = 0;
		state->lossy_pages = 0;
	}
	else if (state->index_plan != NULL)
	{
		/* The index scan computes its keys anew from changed parameters. */
		if (css->ss.ps.chgParam != NULL)
			UpdateChangedParamSet(state->index_plan, css->ss.ps.chgParam);
		if (state->index_plan->chgParam == NULL)
			ExecReScan(state->index_plan);
	}
	else if (state->scan != NULL)
		table_rescan(state->scan, NULL);
	state->page_active = false;
	state->page_cursor = 0;
	state->exhausted = false;
	state->produced = 0;
	state->batches = 0;
	state->pages = 0;
}

/* This participant's counters; a node that never ran counts nothing. */
static void
heap_scan_counters(HeapScanState *state, uint64 *values)
{
	memset(values, 0, HEAP_SCAN_NCOUNTERS * sizeof(uint64));
	values[HEAP_SCAN_RAN] = state->request != NULL;
	values[HEAP_SCAN_CAPACITY] = state->request != NULL ? state->capacity : 0;
	values[HEAP_SCAN_BATCHES] = state->batches;
	values[HEAP_SCAN_PAGES] = state->pages;
	if (state->heap != NULL)
	{
		const TessHeapBatchStats *stats = tess_heap_batch_stats(state->heap);

		values[HEAP_SCAN_DEFORMED] = stats->deformed_datums;
		values[HEAP_SCAN_RESTARTED] = stats->restarted_datums;
	}
	if (state->projection != NULL)
	{
		const TessProjectionStats *computed = tess_projection_stats(state->projection);

		values[HEAP_SCAN_COMPUTED] = computed->chain_datums + computed->row_datums;
	}
	values[HEAP_SCAN_EXACT] = state->exact_pages;
	values[HEAP_SCAN_LOSSY] = state->lossy_pages;
}

/* The totals of every participant in a parallel plan, else the node's own. */
static void
heap_scan_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	HeapScanState *state = (HeapScanState *) css;
	const uint64 *totals = NULL;
	uint64		own[HEAP_SCAN_NCOUNTERS];

	if (state->stats != NULL)
		totals = tess_shared_stats_totals(state->stats);
	if (totals == NULL)
	{
		heap_scan_counters(state, own);
		totals = own;
	}
	/* The parent's request, and so the size, is known once executed. */
	if (totals[HEAP_SCAN_RAN] > 0)
		ExplainPropertyInteger("Batch Size", NULL,
							   totals[HEAP_SCAN_CAPACITY] / totals[HEAP_SCAN_RAN], es);
	if (!es->analyze)
		return;
	ExplainPropertyInteger("Batches", NULL, totals[HEAP_SCAN_BATCHES], es);
	if (state->index_plan == NULL)
		ExplainPropertyInteger("Pages", NULL, totals[HEAP_SCAN_PAGES], es);
	if (state->bitmap_plan != NULL)
	{
		ExplainPropertyInteger("Exact Heap Blocks", NULL, totals[HEAP_SCAN_EXACT], es);
		ExplainPropertyInteger("Lossy Heap Blocks", NULL, totals[HEAP_SCAN_LOSSY], es);
	}
	if (totals[HEAP_SCAN_RAN] > 0)
	{
		ExplainPropertyInteger("Deformed Datums", NULL, totals[HEAP_SCAN_DEFORMED], es);
		ExplainPropertyInteger("Restarted Datums", NULL, totals[HEAP_SCAN_RESTARTED], es);
	}
	if (state->projection != NULL)
		ExplainPropertyInteger("Computed Datums", NULL, totals[HEAP_SCAN_COMPUTED], es);
}

/*
 * A parallel plan: the participants share the core's parallel scan
 * descriptor, which hands each of them its own pages, followed by the
 * rows of their counters, in the node's chunk of the query's shared
 * memory. The leader lays the chunk out and begins its scan before its
 * first execution; a worker attaches to it. The node's own rescan leaves
 * the shared descriptor to the leader's reinitialization, which the
 * Gather runs before it launches the workers again.
 */
static ParallelTableScanDesc
shared_scan(void *coordinate)
{
	return (ParallelTableScanDesc)
		((char *) coordinate + tess_shared_stats_size(coordinate));
}

static Size
heap_scan_estimate_dsm(CustomScanState *css, ParallelContext *pcxt)
{
	EState	   *estate = css->ss.ps.state;

	return add_size(tess_shared_stats_estimate(HEAP_SCAN_NCOUNTERS, pcxt->nworkers),
					table_parallelscan_estimate(css->ss.ss_currentRelation,
												estate->es_snapshot));
}

static void
heap_scan_initialize_dsm(CustomScanState *css, ParallelContext *pcxt,
						 void *coordinate)
{
	HeapScanState *state = (HeapScanState *) css;
	EState	   *estate = css->ss.ps.state;
	Relation	rel = css->ss.ss_currentRelation;
	ParallelTableScanDesc pscan;

	/* A Gather a limit above shut down sets up anew when rescanned. */
	end_scan(state);
	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	state->stats = tess_shared_stats_init(estate->es_query_cxt, coordinate,
										  HEAP_SCAN_NCOUNTERS, pcxt->nworkers,
										  pcxt->seg);
	pscan = shared_scan(coordinate);
	table_parallelscan_initialize(rel, pscan, estate->es_snapshot);
	begin_scan(state, table_beginscan_parallel(rel, pscan, heap_scan_flags(state)));
}

static void
heap_scan_reinitialize_dsm(CustomScanState *css, ParallelContext *pcxt,
						   void *coordinate)
{
	HeapScanState *state = (HeapScanState *) css;

	table_parallelscan_reinitialize(css->ss.ss_currentRelation,
									shared_scan(coordinate));
	tess_shared_stats_reset(state->stats);
}

static void
heap_scan_initialize_worker(CustomScanState *css, shm_toc *toc,
							void *coordinate)
{
	HeapScanState *state = (HeapScanState *) css;
	EState	   *estate = css->ss.ps.state;
	Relation	rel = css->ss.ss_currentRelation;

	state->stats = tess_shared_stats_attach(estate->es_query_cxt, coordinate,
											ParallelWorkerNumber + 1);
	begin_scan(state, table_beginscan_parallel(rel, shared_scan(coordinate),
											   heap_scan_flags(state)));
}

/*
 * After the plan's last row, in every participant: the counters into the
 * shared rows, and the parallel scan's descriptor, which refers to the
 * shared memory, ended while that is mapped. A serial scan stays: the
 * executor shuts a plan down after every partial run of it too.
 */
static void
heap_scan_shutdown(CustomScanState *css)
{
	HeapScanState *state = (HeapScanState *) css;
	uint64		values[HEAP_SCAN_NCOUNTERS];

	if (state->stats != NULL)
	{
		heap_scan_counters(state, values);
		tess_shared_stats_store(state->stats, values);
	}
	if (state->scan != NULL && state->scan->rs_parallel != NULL)
		end_scan(state);
}

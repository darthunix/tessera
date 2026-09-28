#include "postgres.h"

#include "access/heapam.h"
#include "access/parallel.h"
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
#include "storage/shm_toc.h"
#include "utils/rel.h"

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
 * leader reports the counters of all of them. See docs/nodes.md.
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
 * Whether the path is a sequential scan of a plain heap table whose
 * targets are columns of it or expressions over them, which the node
 * computes. A stand-in path of a test has neither a relation nor a target.
 */
static bool
plain_heap_scan(PlannerInfo *root, const Path *path)
{
	RelOptInfo *rel = path->parent;
	RangeTblEntry *rte;
	Relation	relation;
	bool		heap;

	if (root == NULL || rel == NULL || path->pathtarget == NULL ||
		path->pathtype != T_SeqScan || !IS_SIMPLE_REL(rel) ||
		rel->rtekind != RTE_RELATION)
		return false;
	rte = planner_rt_fetch(rel->relid, root);
	if (rte->relkind != RELKIND_RELATION || rte->inh ||
		rte->tablesample != NULL)
		return false;
	foreach_ptr(Var, var, pull_var_clause((Node *) path->pathtarget->exprs,
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
 * Every relation column is a batch column and the relation's row is the
 * scan tuple; the targets, PostgreSQL's projection among them, are derived
 * from it when the plan is read.
 */
static Plan *
heap_scan_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
			   List *tlist, List *clauses, List *custom_plans)
{
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);

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
	if (info.node != &tess_heap_scan_node || info.nchildren != 0 ||
		rel == NULL || cscan->custom_scan_tlist != NIL ||
		info.layout.ncolumns < RelationGetDescr(rel)->natts)
		elog(ERROR, "TessHeapScan received a foreign plan");
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

/* Drop the batch's pins and the landing slot's, then the scan. */
static void
end_scan(HeapScanState *state)
{
	if (state->heap != NULL)
		tess_heap_batch_reset(state->heap);
	ExecClearTuple(state->landing);
	if (state->scan != NULL)
		table_endscan(state->scan);
	state->scan = NULL;
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
	int			nrows = Min(limit, hscan->rs_ntuples - state->page_cursor);

	tess_heap_batch_append_page(state->heap, hscan->rs_cbuf, hscan->rs_cblock,
								hscan->rs_vistuples + state->page_cursor, nrows,
								RelationGetRelid(rel));
	/* As pgstat_count_heap_getnext; the core counted the page's first tuple. */
	if (pgstat_should_count_relation(rel))
	{
		Assert(rel->pgstat_info->kind == PGSTAT_KIND_RELATION);
		rel->pgstat_info->tab.counts.tuples_returned +=
			state->page_cursor > 0 ? nrows : nrows - 1;
	}
	state->page_cursor += nrows;
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
	if (state->scan == NULL)
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
	if (state->scan != NULL)
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
	ExplainPropertyInteger("Pages", NULL, totals[HEAP_SCAN_PAGES], es);
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

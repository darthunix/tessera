#include "postgres.h"

#include "access/parallel.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "nodes/makefuncs.h"
#include "optimizer/optimizer.h"
#include "storage/shm_toc.h"
#include "utils/ruleutils.h"

#include "tessera/kernel_ops.h"
#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * TessFilter stands on the unary helper: it applies the relation's
 * clauses to each batch of its child in the planner's order, those the
 * compiler takes as batch filters and the others row by row, each over
 * the rows the ones before it kept, and passes
 * the batch on with the rows that remain. A hash join above may hand it
 * the Bloom filter of its build side's keys, which it then checks rows
 * against before its first row-wise clause. Under a Gather, the leader
 * reports the counters of every participant. See docs/nodes.md.
 */

/* The counters every participant of a parallel plan shares. */
enum
{
	FILTER_BATCH_REMOVED,
	FILTER_RESIDUAL_REMOVED,
	FILTER_INPUT_BATCHES,
	FILTER_INPUT_ROWS,
	FILTER_OUTPUT_ROWS,
	FILTER_COMPUTED,
	/* Rows with a key that a parent's key filter rejected. */
	FILTER_KEY_REMOVED,
	/* Rows the key filter's check removed for a NULL key. */
	FILTER_NULL_REMOVED,
	FILTER_NCOUNTERS
};

typedef struct FilterState
{
	CustomScanState css;
	TessUnary  *unary;
	/* The child's layout, which the clauses' columns refer to. */
	TessLayout	child_layout;
	/* The batch clauses, then the row-wise ones. */
	TessQual   *qual;
	/* The targets PostgreSQL asks the node to compute, or NULL. */
	TessProjection *projection;
	/* The counters of every participant, in a parallel plan. */
	TessSharedStats *stats;

	/*
	 * A parent's key filter, applied before the first row-wise clause: its
	 * keys and their kinds, whether a shared filter was seen ready, and the
	 * buffers of a batch for capacity rows.
	 */
	const TessKernelOps *kernels;
	bool		key_filter_set;
	TessKeyFilter key_filter;
	int			key_columns[TESS_TABLE_MAX_KEYS];
	TessTableKeyKind key_kinds[TESS_TABLE_MAX_KEYS];
	bool		key_filter_ready;
	int			capacity;
	uint32	   *hashes;
	uint64	   *valid_bits;
	uint64	   *passed_bits;
	uint64		key_removed;
	uint64		null_removed;
	/* Written by a kernel on failure only. */
	TessStatus	status;
} FilterState;

static void filter_begin(CustomScanState *css, EState *estate, int eflags);
static TupleTableSlot *filter_exec(CustomScanState *css);
static void filter_end(CustomScanState *css);
static void filter_rescan(CustomScanState *css);
static void filter_explain(CustomScanState *css, List *ancestors,
						   ExplainState *es);
static void filter_counters(FilterState *state, uint64 *values);

/*
 * A parallel plan: the node shares only its counters, in the rows of its
 * chunk; the child divides the work. The leader lays the rows out, a
 * worker attaches to its own, and each stores its counters when the
 * executor shuts the node down after the plan's last row.
 */
TESS_NODE_STATS_CALLBACKS(filter, FilterState, FILTER_NCOUNTERS, filter_counters)

static const CustomExecMethods filter_exec_methods = {
	.CustomName = "TessFilter",
	.BeginCustomScan = filter_begin,
	.ExecCustomScan = filter_exec,
	.EndCustomScan = filter_end,
	.ReScanCustomScan = filter_rescan,
	.ExplainCustomScan = filter_explain,
	TESS_NODE_STATS_METHODS(filter),
};

static bool filter_set_key_filter(CustomScanState *css, const TessKeyFilter *filter);

const TessNode tess_filter_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_FILTER_NODE_NAME,
	.set_key_filter = filter_set_key_filter,
};

/* Raise the error a kernel stored, if the call failed. */
#define check(state, code) tess_status_check((code), &(state)->status)

/*
 * The parent's key filter, right before the first row-wise clause, over
 * the rows the batch clauses before it kept: the keys hashed as the join
 * hashes them, the rows with a NULL key and those whose hash the filter
 * rejects removed, each counted apart. A shared filter is used once it is
 * seen ready; until then every row passes.
 */
static int
apply_key_filter(void *private_data, TessBatch *batch, int rows)
{
	FilterState *state = private_data;
	const TessKernelOps *kernels = state->kernels;
	TessKeyFilter *filter = &state->key_filter;
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask valid;
	TessRowMask passed;
	int			kept;
	int			keyed;

	if (filter->shared && !state->key_filter_ready)
	{
		check(state, kernels->bloom_shared_ready(filter->words, filter->nwords,
												 &state->key_filter_ready,
												 &state->status));
		if (!state->key_filter_ready)
			return rows;
	}
	if (nrows > state->capacity)
	{
		MemoryContext context = state->css.ss.ps.state->es_query_cxt;
		int			capacity = Max(nrows, 64);

		if (state->hashes != NULL)
		{
			pfree(state->hashes);
			pfree(state->valid_bits);
			pfree(state->passed_bits);
		}
		state->hashes = MemoryContextAlloc(context, sizeof(uint32) * capacity);
		state->valid_bits = MemoryContextAlloc(context, sizeof(uint64) *
											   tess_row_mask_word_count(capacity));
		state->passed_bits = MemoryContextAlloc(context, sizeof(uint64) *
												tess_row_mask_word_count(capacity));
		state->capacity = capacity;
	}
	/* The kernels fill the masks whole, but check they are masks of nrows. */
	memset(state->valid_bits, 0, sizeof(uint64) * nwords);
	memset(state->passed_bits, 0, sizeof(uint64) * nwords);
	valid = (TessRowMask) {nrows, state->valid_bits};
	passed = (TessRowMask) {nrows, state->passed_bits};
	for (int key = 0; key < filter->nkeys; key++)
	{
		TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
		bool		int8 = filter->kinds[key] == TESS_TABLE_KEY_INT8;

		batch->ops->get_datum_column(batch, filter->columns[key],
									 key == 0 ? &batch->rows : &valid,
									 TESS_COLUMN_FOR_FILTER, &column);
		if (column.values == NULL || column.isnull == NULL || column.nrows != nrows)
			elog(ERROR, "Tessera batch returned an invalid column");
		if (key == 0)
			check(state, (int8 ? kernels->int8_hash : kernels->int4_hash)
				  (&column, NULL, &batch->rows, TESS_NULL_KEYS_REJECT,
				   state->hashes, &valid, &state->status));
		else
			check(state, (int8 ? kernels->int8_hash_next : kernels->int4_hash_next)
				  (&column, NULL, TESS_NULL_KEYS_REJECT, state->hashes, &valid,
				   &state->status));
	}
	if (filter->shared)
		check(state, kernels->bloom_shared_probe(filter->words, filter->nwords,
												 state->hashes, &valid, &passed,
												 &state->status));
	else
		check(state, kernels->bloom_probe(filter->words, filter->nwords,
										  state->hashes, &valid, &passed,
										  &state->status));
	/* The rows passed are among the batch's: only rows are removed. */
	memcpy(batch->rows.bits, state->passed_bits, sizeof(uint64) * nwords);
	kept = tess_row_mask_count(&passed);
	keyed = tess_row_mask_count(&valid);
	/* As the join counts its filter's rows: those with a key, apart. */
	state->null_removed += rows - keyed;
	state->key_removed += keyed - kept;
	return kept;
}

/*
 * Take a parent's key filter, when the node has row-wise clauses for it
 * to save and its keys are columns of the child; NULL takes it back.
 */
static bool
filter_set_key_filter(CustomScanState *css, const TessKeyFilter *filter)
{
	FilterState *state = (FilterState *) css;

	if (filter == NULL)
	{
		state->key_filter_set = false;
		tess_qual_set_row_prefilter(state->qual, NULL, NULL);
		return true;
	}
	if (!tess_qual_has_row_clauses(state->qual) ||
		filter->nkeys < 1 || filter->nkeys > TESS_TABLE_MAX_KEYS ||
		filter->words == NULL || filter->nwords == 0)
		return false;
	for (int key = 0; key < filter->nkeys; key++)
	{
		/* A computed column exists only after the clauses ran. */
		if (filter->columns[key] < 0 ||
			filter->columns[key] >= state->child_layout.ncolumns ||
			(filter->kinds[key] != TESS_TABLE_KEY_INT4 &&
			 filter->kinds[key] != TESS_TABLE_KEY_INT8))
			return false;
	}
	if (state->kernels == NULL)
		state->kernels = tess_runtime_kernels();
	if (state->kernels == NULL)
		return false;
	/* Taken: only now do the keys of a filter held before give way. */
	for (int key = 0; key < filter->nkeys; key++)
	{
		state->key_columns[key] = filter->columns[key];
		state->key_kinds[key] = filter->kinds[key];
	}
	state->key_filter = *filter;
	state->key_filter.columns = state->key_columns;
	state->key_filter.kinds = state->key_kinds;
	state->key_filter_ready = false;
	state->key_filter_set = true;
	tess_qual_set_row_prefilter(state->qual, apply_key_filter, state);
	return true;
}

Node *
tess_filter_create_state(CustomScan *cscan)
{
	FilterState *state = (FilterState *)
		newNode(sizeof(FilterState), T_CustomScanState);

	state->css.methods = &filter_exec_methods;
	return (Node *) state;
}

/* Apply the clauses in the planner's order, each over the rows the previous ones left. */
static int
filter_batch(void *private_data, TessBatch *batch, int rows)
{
	FilterState *state = private_data;

	ResetExprContext(state->css.ss.ps.ps_ExprContext);
	return tess_qual_apply(state->qual, batch, state->css.ss.ps.ps_ExprContext,
						   rows);
}

static void
filter_begin(CustomScanState *css, EState *estate, int eflags)
{
	FilterState *state = (FilterState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TessUnaryConfig config = TESS_STRUCT_INITIALIZER(TessUnaryConfig);
	TessQualConfig qual = TESS_STRUCT_INITIALIZER(TessQualConfig);
	TessPlanReader *reader;
	PlanState  *child;

	tess_node_require_forward(eflags, "TessFilter");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_filter_node || info.nchildren != 1 ||
		info.child_names[0] == NULL ||
		(cscan->custom_exprs == NIL && cscan->scan.plan.qual == NIL))
		elog(ERROR, "TessFilter received a foreign plan");
	child = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
	css->custom_ps = list_make1(child);
	state->child_layout = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	tess_plan_get_layout(child->plan, &state->child_layout);
	reader = tess_plan_reader_create((List *) info.node_data, TESS_FILTER_DATA,
									 TESS_FILTER_DATA_VERSION);
	qual.order = tess_plan_read_int_list(reader, "order");
	tess_plan_reader_finish(reader);
	/* The scan tuple is the child's target list, as the child maps it. */
	qual.parent_context = estate->es_query_cxt;
	qual.parent = &css->ss.ps;
	qual.batch_clauses = cscan->custom_exprs;
	qual.row_clauses = cscan->scan.plan.qual;
	qual.scan_slot = css->ss.ss_ScanTupleSlot;
	qual.scan_tuple = &state->child_layout;
	state->qual = tess_qual_create(&qual);
	if (info.computed != NIL)
		state->projection = tess_node_projection(css, css->ss.ss_ScanTupleSlot,
												 &state->child_layout,
												 state->child_layout.ncolumns, info.computed);
	config.parent_context = estate->es_query_cxt;
	config.node = css;
	config.child = child;
	config.layout = &info.layout;
	config.filter_columns = tess_qual_columns(state->qual);
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

	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
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

/* This participant's counters. */
static void
filter_counters(FilterState *state, uint64 *values)
{
	const TessUnaryStats *stats = tess_unary_stats(state->unary);
	const TessQualStats *removed = tess_qual_stats(state->qual);

	memset(values, 0, FILTER_NCOUNTERS * sizeof(uint64));
	values[FILTER_BATCH_REMOVED] = removed->batch_removed;
	values[FILTER_RESIDUAL_REMOVED] = removed->row_removed;
	values[FILTER_INPUT_BATCHES] = stats->input_batches;
	values[FILTER_INPUT_ROWS] = stats->input_rows;
	values[FILTER_OUTPUT_ROWS] = stats->output_rows;
	if (state->projection != NULL)
	{
		const TessProjectionStats *computed = tess_projection_stats(state->projection);

		values[FILTER_COMPUTED] = computed->chain_datums + computed->row_datums;
	}
	values[FILTER_KEY_REMOVED] = state->key_removed;
	values[FILTER_NULL_REMOVED] = state->null_removed;
}

/*
 * The batch clauses, as the core shows a scan's qualifiers; the totals of
 * every participant in a parallel plan, else the node's own.
 */
static void
filter_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	FilterState *state = (FilterState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	List	   *context;
	bool		useprefix = es->rtable_size > 1 || es->verbose;
	const uint64 *totals;
	uint64		own[FILTER_NCOUNTERS];

	context = set_deparse_context_plan(es->deparse_cxt, css->ss.ps.plan,
									   ancestors);
	if (cscan->custom_exprs != NIL)
		ExplainPropertyText("Batch Filter",
							deparse_expression((Node *) make_ands_explicit(cscan->custom_exprs),
											   context, useprefix, false), es);
	if (!es->analyze)
		return;
	filter_counters(state, own);
	totals = tess_shared_stats_totals_or(state->stats, own);
	if (cscan->custom_exprs != NIL)
		show_removed("Rows Removed by Batch Filter", totals[FILTER_BATCH_REMOVED],
					 css, es);
	if (totals[FILTER_KEY_REMOVED] > 0)
		show_removed("Rows Removed by Bloom Filter", totals[FILTER_KEY_REMOVED],
					 css, es);
	if (totals[FILTER_NULL_REMOVED] > 0)
		show_removed("Rows Removed by NULL Key", totals[FILTER_NULL_REMOVED],
					 css, es);
	if (cscan->scan.plan.qual != NIL)
		show_removed("Rows Removed by Residual Filter",
					 totals[FILTER_RESIDUAL_REMOVED], css, es);
	/* The batches and their columns: VERBOSE only. */
	if (!es->verbose)
		return;
	ExplainPropertyInteger("Input Batches", NULL, totals[FILTER_INPUT_BATCHES], es);
	ExplainPropertyInteger("Input Rows", NULL, totals[FILTER_INPUT_ROWS], es);
	ExplainPropertyInteger("Output Rows", NULL, totals[FILTER_OUTPUT_ROWS], es);
	if (state->projection != NULL)
		ExplainPropertyInteger("Computed Datums", NULL, totals[FILTER_COMPUTED], es);
}


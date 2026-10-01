#include "postgres.h"

#include "executor/executor.h"
#include "executor/instrument.h"
#include "utils/memutils.h"

#include "tessera/plan.h"
#include "tessera/planner.h"
#include "tessera/runtime.h"

struct TessUnary
{
	CustomScanState *node;
	PlanState  *child;
	/* Rescanned instead of the child, which it reaches. */
	PlanState  *rescan_child;
	/* The node's request binding; nothing is ever published through it. */
	TessOutput *output;
	TessInput  *input;
	TessLayout	layout;
	Bitmapset  *filter_columns;
	Bitmapset  *projection_columns;
	int			max_rows;
	TessUnaryProcess process;
	void	   *private_data;
	/* Both frozen at the first execution. */
	const TessRequest *request;
	const TessRequest *child_request;
	/* Row mode: the batch column of every slot attribute, per batch. */
	TessDatumColumn *columns;
	/* Computed columns: the active batch is then the projection's wrapper. */
	TessProjection *projection;
	TessBatch  *active_batch;
	int			next_row;
	bool		stopped;
	TessUnaryStats stats;
};

TessUnary *
tess_unary_create(const TessUnaryConfig *config)
{
	TessUnary  *unary;
	MemoryContext oldcontext;

	if (config == NULL || config->struct_size < TESS_UNARY_CONFIG_MIN_SIZE)
		elog(ERROR, "Tessera received an incompatible unary configuration");
	if (config->parent_context == NULL || config->node == NULL ||
		config->child == NULL || config->layout == NULL ||
		config->layout->struct_size < TESS_LAYOUT_MIN_SIZE)
		elog(ERROR, "Tessera unary node requires a context, node, child and layout");
	if (config->max_rows < 0)
		elog(ERROR, "Tessera unary node batch limit must not be negative");
	unary = MemoryContextAllocZero(config->parent_context, sizeof(*unary));
	unary->node = config->node;
	unary->child = config->child;
	unary->max_rows = config->max_rows;
	unary->process = config->process;
	unary->private_data = config->private_data;
	if (TESS_ABI_HAS_FIELD(config, TessUnaryConfig, projection))
		unary->projection = config->projection;
	unary->rescan_child = config->child;
	if (TESS_ABI_HAS_FIELD(config, TessUnaryConfig, rescan_child) &&
		config->rescan_child != NULL)
		unary->rescan_child = config->rescan_child;
	unary->next_row = -1;
	oldcontext = MemoryContextSwitchTo(config->parent_context);
	unary->filter_columns = bms_copy(config->filter_columns);
	unary->projection_columns = bms_copy(config->projection_columns);
	unary->layout = *config->layout;
	if (config->layout->target_columns != NULL)
	{
		int		   *columns = palloc_array(int, config->layout->ntargets);

		memcpy(columns, config->layout->target_columns,
			   sizeof(*columns) * config->layout->ntargets);
		unary->layout.target_columns = columns;
	}
	MemoryContextSwitchTo(oldcontext);
	unary->output = tess_output_create(config->parent_context,
									   &config->node->ss.ps,
									   config->node->ss.ps.ps_ResultTupleSlot,
									   config->layout);
	unary->input = tess_input_create(config->parent_context, config->child);
	unary->columns = MemoryContextAllocZero(config->parent_context,
											mul_size(sizeof(TessDatumColumn),
													 config->layout->ntargets));
	return unary;
}

PlanState *
tess_unary_child(TessUnary *unary)
{
	return unary->child;
}

const TessRequest *
tess_unary_request(TessUnary *unary)
{
	return unary->request;
}

const TessRequest *
tess_unary_child_request(TessUnary *unary)
{
	return unary->child_request;
}

const TessUnaryStats *
tess_unary_stats(TessUnary *unary)
{
	return &unary->stats;
}

void
tess_unary_stop(TessUnary *unary)
{
	unary->stopped = true;
}

void
tess_unary_set_tuple_bound(TessUnary *unary, int64 tuples_needed)
{
	tess_set_child_bound(unary->child, tuples_needed);
}

/* Derive the child's request from the parent's and the node's own. */
static void
forward_request(TessUnary *unary)
{
	const TessBindingOps *ops = tess_runtime_api()->binding_ops;
	const TessLayout *child_layout = tess_input_layout(unary->input);
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
	Bitmapset  *filter;
	Bitmapset  *projection;
	int			max_rows = unary->max_rows;

	unary->request = tess_output_request(unary->output);
	if (unary->projection == NULL ?
		child_layout->ncolumns != unary->layout.ncolumns :
		child_layout->ncolumns > unary->layout.ncolumns)
		elog(ERROR, "Tessera unary node cannot pass %d columns through as %d",
			 child_layout->ncolumns, unary->layout.ncolumns);
	filter = bms_union(unary->filter_columns, unary->request->filter_columns);
	projection = bms_union(unary->projection_columns,
						   unary->request->projection_columns);
	if (unary->request->output_mode == TESS_OUTPUT_ROWS)
	{
		/* Rows are served from the batch column of every slot attribute. */
		int			natts = unary->node->ss.ps.ps_ResultTupleSlot->tts_tupleDescriptor->natts;

		for (int attribute = 0; attribute < natts; attribute++)
			projection = bms_add_member(projection,
										tess_layout_column(&unary->layout, attribute));
	}
	/* The computed columns are the node's, not the child's. */
	for (int column = child_layout->ncolumns; column < unary->layout.ncolumns; column++)
	{
		filter = bms_del_member(filter, column);
		projection = bms_del_member(projection, column);
	}
	if (unary->request->max_batch_rows > 0)
		max_rows = max_rows == 0 ? unary->request->max_batch_rows :
			Min(max_rows, unary->request->max_batch_rows);
	/* The child is always consumed as whole batches. */
	request.filter_columns = filter;
	request.projection_columns = projection;
	request.output_mode = TESS_OUTPUT_BATCH;
	request.max_batch_rows = max_rows;
	tess_input_set_request(unary->input, &request);
	unary->child_request = ops->freeze_request(tess_input_binding(unary->input));
	bms_free(filter);
	bms_free(projection);
}

/* Make the next batch with rows active, or return false at the end. */
static bool
fetch_batch(TessUnary *unary)
{
	for (;;)
	{
		TessBatch  *batch;
		int			rows;
		int			kept;

		if (unary->stopped)
			return false;
		/* The parent finished the wrapper; the child's batch ends with it. */
		if (unary->projection != NULL && unary->active_batch != NULL)
		{
			tess_input_finish(unary->input);
			unary->active_batch = NULL;
		}
		batch = tess_input_next(unary->input);
		if (batch == NULL)
			return false;
		rows = tess_row_mask_count(&batch->rows);
		unary->stats.input_batches++;
		unary->stats.input_rows += rows;
		kept = rows;
		if (unary->process != NULL)
		{
			kept = unary->process(unary->private_data, batch, rows);
			if (kept != tess_row_mask_count(&batch->rows))
				elog(ERROR, "Tessera unary node process returned a wrong row count");
		}
		InstrCountFiltered1(unary->node, rows - kept);
		if (kept == 0)
		{
			tess_input_finish(unary->input);
			continue;
		}
		if (unary->projection != NULL)
			batch = tess_projection_wrap(unary->projection, batch);
		/* Row mode: the column of every slot attribute, for the whole batch. */
		if (unary->request->output_mode == TESS_OUTPUT_ROWS)
			tess_batch_target_columns(batch, &unary->layout,
									  unary->node->ss.ps.ps_ResultTupleSlot->tts_tupleDescriptor->natts,
									  unary->columns);
		unary->stats.output_rows += kept;
		unary->active_batch = batch;
		unary->next_row = tess_row_mask_next(&batch->rows, -1);
		return true;
	}
}

/*
 * Forward the child's slot: the parent reads and finishes the batch there.
 * With computed columns, publish the wrapper through the node's own slot
 * instead; the parent finishes it there, and the child's batch when the
 * next one is fetched.
 */
static TupleTableSlot *
exec_batch(TessUnary *unary)
{
	PlanState  *ps = &unary->node->ss.ps;

	if (unary->projection != NULL)
		tess_output_release(unary->output);
	if (!fetch_batch(unary))
	{
		unary->active_batch = NULL;
		return NULL;
	}
	if (unary->projection != NULL)
		return tess_output_publish(unary->output, unary->active_batch);
	/* One call returns the whole batch, which the executor counts as one. */
	if (ps->instrument != NULL)
		ps->instrument->tuplecount +=
			tess_row_mask_count(&unary->active_batch->rows) - 1;
	return tess_input_slot(unary->input);
}

/* Serve the rows of each batch from the node's own slot. */
static TupleTableSlot *
exec_rows(TessUnary *unary)
{
	TupleTableSlot *slot = unary->node->ss.ps.ps_ResultTupleSlot;
	int			natts = slot->tts_tupleDescriptor->natts;

	for (;;)
	{
		int			row;

		if (unary->active_batch == NULL && !fetch_batch(unary))
			return NULL;
		if (unary->next_row < 0)
		{
			/* Served in full: the wrapper's copies go with it. */
			if (unary->projection != NULL)
				unary->active_batch->ops->release(unary->active_batch);
			tess_input_finish(unary->input);
			unary->active_batch = NULL;
			continue;
		}
		row = unary->next_row;
		unary->next_row = tess_row_mask_next(&unary->active_batch->rows, row);
		ExecClearTuple(slot);
		for (int attribute = 0; attribute < natts; attribute++)
		{
			slot->tts_values[attribute] = unary->columns[attribute].values[row];
			slot->tts_isnull[attribute] = unary->columns[attribute].isnull[row];
		}
		slot->tts_tableOid = unary->active_batch->table_oid;
		return ExecStoreVirtualTuple(slot);
	}
}

TupleTableSlot *
tess_unary_exec(TessUnary *unary)
{
	if (unary->request == NULL)
		forward_request(unary);
	return unary->request->output_mode == TESS_OUTPUT_BATCH ?
		exec_batch(unary) : exec_rows(unary);
}

void
tess_unary_end(TessUnary *unary)
{
	tess_output_end(unary->output);
}

void
tess_unary_rescan(TessUnary *unary)
{
	tess_output_clear(unary->output);
	ExecClearTuple(unary->node->ss.ps.ps_ResultTupleSlot);
	if (unary->projection != NULL)
		tess_projection_reset(unary->projection);
	if (unary->active_batch != NULL)
		tess_input_finish(unary->input);
	/* The core passes changed parameters to outer and inner plans only. */
	if (unary->node->ss.ps.chgParam != NULL)
		UpdateChangedParamSet(unary->rescan_child, unary->node->ss.ps.chgParam);
	ExecReScan(unary->rescan_child);
	tess_input_rescan(unary->input);
	unary->active_batch = NULL;
	unary->next_row = -1;
	unary->stopped = false;
	MemSet(&unary->stats, 0, sizeof(unary->stats));
}

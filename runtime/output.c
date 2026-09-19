#include "postgres.h"

#include "executor/executor.h"
#include "executor/instrument.h"
#include "utils/memutils.h"

#include "tessera/runtime.h"

struct TessOutput
{
	const TessBindingOps *ops;
	/* The node, for the instrumentation the executor allocates after Begin. */
	PlanState  *ps;
	TupleTableSlot *slot;
	TessBinding *binding;
	/* The batch column of each slot attribute. */
	int		   *batch_columns;
};

/* Show one selected row of the batch in the slot. */
static void
select_row(TessOutput *output, TessBatch *batch, int row)
{
	TupleTableSlot *slot = output->slot;
	TupleDesc	desc = slot->tts_tupleDescriptor;
	int			nwords = tess_row_mask_word_count(batch->rows.nrows);
	uint64		local = 0;
	uint64	   *bits;
	TessRowMask one;
	int			attribute;

	if (row < 0 || row >= batch->rows.nrows ||
		!tess_row_mask_contains(&batch->rows, row))
		elog(ERROR, "Tessera output row %d is not selected", row);
	ExecClearTuple(slot);
	bits = nwords == 1 ? &local : palloc0_array(uint64, nwords);
	bits[row / 64] = UINT64CONST(1) << (row % 64);
	one.nrows = batch->rows.nrows;
	one.bits = bits;
	for (attribute = 0; attribute < desc->natts; attribute++)
	{
		TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);

		batch->ops->get_datum_column(batch, output->batch_columns[attribute],
									 &one, TESS_COLUMN_FOR_PROJECTION, &column);
		if (column.values == NULL || column.isnull == NULL ||
			column.nrows != batch->rows.nrows)
			elog(ERROR, "Tessera batch returned an invalid column");
		slot->tts_values[attribute] = column.values[row];
		slot->tts_isnull[attribute] = column.isnull[row];
	}
	if (nwords > 1)
		pfree(bits);
	slot->tts_tableOid = batch->table_oid;
	ExecStoreVirtualTuple(slot);
}

TessOutput *
tess_output_create(MemoryContext parent_context, PlanState *ps,
				   TupleTableSlot *slot, const TessLayout *layout)
{
	const TessApi *api = tess_runtime_api();
	TessOutput *output;
	int			natts;
	int			attribute;

	if (parent_context == NULL || slot == NULL || layout == NULL)
		elog(ERROR, "Tessera output requires a context, slot and layout");
	if (layout->struct_size < TESS_LAYOUT_MIN_SIZE)
		elog(ERROR, "Tessera output received an incompatible layout");
	if (slot->tts_ops != &TTSOpsVirtual)
		elog(ERROR, "Tessera output requires a virtual slot");
	natts = slot->tts_tupleDescriptor->natts;
	if (layout->ntargets < natts)
		elog(ERROR, "Tessera output layout describes %d targets but the slot has %d attributes",
			 layout->ntargets, natts);
	output = MemoryContextAllocZero(parent_context, sizeof(*output));
	output->ops = api->binding_ops;
	output->ps = ps;
	output->slot = slot;
	output->batch_columns = MemoryContextAlloc(parent_context,
											   mul_size(sizeof(int), natts));
	for (attribute = 0; attribute < natts; attribute++)
	{
		int			column = tess_layout_column(layout, attribute);

		if (column < 0 || column >= layout->ncolumns)
			elog(ERROR, "Tessera output target %d has no batch column",
				 attribute + 1);
		output->batch_columns[attribute] = column;
	}
	output->binding = output->ops->attach(slot, layout);
	return output;
}

TessBinding *
tess_output_binding(TessOutput *output)
{
	return output->binding;
}

const TessRequest *
tess_output_request(TessOutput *output)
{
	return output->ops->freeze_request(output->binding);
}

/* Return a finished batch to its owner and clear the slot. */
static void
recycle(TessOutput *output, bool require_consumed)
{
	if (output->ops->get_batch(output->binding) == NULL)
		return;
	if (require_consumed && !output->ops->is_consumed(output->binding))
		elog(ERROR, "Tessera parent requested a new batch too early");
	output->ops->release_batch(output->binding);
	ExecClearTuple(output->slot);
}

void
tess_output_release(TessOutput *output)
{
	recycle(output, true);
}

TupleTableSlot *
tess_output_publish(TessOutput *output, TessBatch *batch)
{
	const TessRequest *request;
	int			first;

	if (batch == NULL)
		elog(ERROR, "Tessera output cannot publish a null batch");
	first = tess_row_mask_next(&batch->rows, -1);
	if (first < 0)
		elog(ERROR, "Tessera output cannot publish an empty selection");
	recycle(output, true);
	output->ops->publish_batch(output->binding, batch);
	select_row(output, batch, first);
	/*
	 * A batch-aware parent gets the whole batch from one ExecProcNode call,
	 * which the executor's instrumentation counts as one row.
	 */
	request = output->ops->freeze_request(output->binding);
	if (request->output_mode == TESS_OUTPUT_BATCH &&
		output->ps != NULL && output->ps->instrument != NULL)
		output->ps->instrument->tuplecount += tess_row_mask_count(&batch->rows) - 1;
	return output->slot;
}

TupleTableSlot *
tess_output_select(TessOutput *output, int row)
{
	TessBatch  *batch = output->ops->get_batch(output->binding);

	if (batch == NULL)
		elog(ERROR, "Tessera output has no active batch");
	select_row(output, batch, row);
	return output->slot;
}

void
tess_output_finish(TessOutput *output)
{
	if (output->ops->get_batch(output->binding) != NULL &&
		!output->ops->is_consumed(output->binding))
		output->ops->mark_consumed(output->binding);
}

bool
tess_output_finished(TessOutput *output)
{
	return output->ops->is_consumed(output->binding);
}

void
tess_output_clear(TessOutput *output)
{
	recycle(output, false);
}

void
tess_output_end(TessOutput *output)
{
	recycle(output, false);
	output->ops->detach(output->binding);
	output->binding = NULL;
}

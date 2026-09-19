#include "postgres.h"

#include <string.h>

#include "catalog/pg_type_d.h"
#include "executor/executor.h"
#include "fmgr.h"
#include "utils/builtins.h"
#include "utils/memutils.h"

#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_input_batches);
PG_FUNCTION_INFO_V1(tessera_test_input_forwarding);
PG_FUNCTION_INFO_V1(tessera_test_input_errors);

/*
 * A stand-in for a batch node: a PlanState whose ExecProcNode publishes
 * batches of (row, text) rows through an output on its result slot, as
 * many rows per batch as the request allows. With forward set, it runs
 * that node instead and returns its slot; with forward_slot, it returns
 * that slot as it is.
 */
typedef struct FakeChild
{
	PlanState	ps;
	TessOutput *output;
	TessBuilder *builder;
	TupleTableSlot *input_slot;
	int			next_row;
	int			nrows;
	struct FakeChild *forward;
	TupleTableSlot *forward_slot;
} FakeChild;

static TupleDesc
make_desc(void)
{
	TupleDesc	desc = CreateTemplateTupleDesc(2);

	TupleDescInitEntry(desc, 1, "a", INT4OID, -1, 0);
	TupleDescInitEntry(desc, 2, "c", TEXTOID, -1, 0);
	return desc;
}

static TupleTableSlot *
fake_exec(PlanState *ps)
{
	FakeChild  *child = (FakeChild *) ps;
	const TessRequest *request;
	TessBatch  *batch;
	int			limit;
	int			count = 0;

	if (child->forward != NULL)
		return ExecProcNode(&child->forward->ps);
	if (child->forward_slot != NULL)
		return child->forward_slot;
	if (child->next_row >= child->nrows)
		return NULL;
	request = tess_output_request(child->output);
	limit = request->max_batch_rows > 0 ? request->max_batch_rows : child->nrows;
	tess_output_release(child->output);
	tess_builder_reset(child->builder);
	while (child->next_row < child->nrows && count < limit)
	{
		char		text[16];

		snprintf(text, sizeof(text), "row%d", child->next_row + 1);
		ExecClearTuple(child->input_slot);
		child->input_slot->tts_values[0] = Int32GetDatum(child->next_row + 1);
		child->input_slot->tts_isnull[0] = false;
		child->input_slot->tts_values[1] = PointerGetDatum(cstring_to_text(text));
		child->input_slot->tts_isnull[1] = false;
		ExecStoreVirtualTuple(child->input_slot);
		tess_builder_append_slot(child->builder, child->input_slot);
		child->next_row++;
		count++;
	}
	batch = tess_builder_finish(child->builder, InvalidOid);
	return tess_output_publish(child->output, batch);
}

static FakeChild *
make_child(bool bound)
{
	FakeChild  *child = palloc0(sizeof(FakeChild));
	TupleDesc	desc = make_desc();
	TessBuilderConfig config = TESS_STRUCT_INITIALIZER(TessBuilderConfig);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);

	child->ps.ExecProcNode = fake_exec;
	child->ps.ps_ResultTupleSlot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	child->input_slot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	child->nrows = 5;
	config.parent_context = CurrentMemoryContext;
	config.tuple_desc = desc;
	config.ncolumns = 2;
	config.capacity = 5;
	child->builder = tess_builder_create(&config);
	layout.ncolumns = 2;
	layout.ntargets = 2;
	if (bound)
		child->output = tess_output_create(CurrentMemoryContext, &child->ps,
										   child->ps.ps_ResultTupleSlot, &layout);
	return child;
}

/* The int4 values of the batch's selected rows, as a sum and a count. */
static bool
batch_holds(TessBatch *batch, int expected_count, int first_value)
{
	TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	int			row = -1;
	int			count = 0;

	batch->ops->get_datum_column(batch, 0, &batch->rows,
								 TESS_COLUMN_FOR_FILTER, &column);
	while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
	{
		if (column.isnull[row] ||
			DatumGetInt32(column.values[row]) != first_value + count)
			return false;
		count++;
	}
	return count == expected_count;
}

Datum
tessera_test_input_batches(PG_FUNCTION_ARGS)
{
	const TessBindingOps *ops = tess_runtime_api()->binding_ops;
	FakeChild  *child = make_child(true);
	TessInput  *input = tess_input_create(CurrentMemoryContext, &child->ps);
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
	TessBatch  *batch;
	bool		result;

	result = tess_input_layout(input)->ncolumns == 2 &&
		tess_input_binding(input) == ops->find(child->ps.ps_ResultTupleSlot) &&
		tess_input_slot(input) == NULL;
	request.output_mode = TESS_OUTPUT_BATCH;
	request.max_batch_rows = 2;
	tess_input_set_request(input, &request);

	batch = tess_input_next(input);
	result = result && batch != NULL && batch_holds(batch, 2, 1) &&
		tess_input_slot(input) == child->ps.ps_ResultTupleSlot &&
		!tess_input_finished(input) &&
		tess_output_request(child->output)->max_batch_rows == 2;
	tess_input_finish(input);
	batch = tess_input_next(input);
	result = result && batch != NULL && batch_holds(batch, 2, 3);
	tess_input_finish(input);
	batch = tess_input_next(input);
	result = result && batch != NULL && batch_holds(batch, 1, 5);
	tess_input_finish(input);
	result = result && tess_input_next(input) == NULL &&
		tess_input_next(input) == NULL && tess_input_slot(input) == NULL;
	tess_output_end(child->output);
	PG_RETURN_BOOL(result);
}

Datum
tessera_test_input_forwarding(PG_FUNCTION_ARGS)
{
	const TessBindingOps *ops = tess_runtime_api()->binding_ops;
	FakeChild  *inner = make_child(true);
	FakeChild  *outer = make_child(true);
	TessInput  *input;
	TessBatch  *batch;
	bool		result;

	/* The outer node returns the inner node's slot with its batch. */
	outer->forward = inner;
	input = tess_input_create(CurrentMemoryContext, &outer->ps);
	batch = tess_input_next(input);
	result = batch != NULL && batch_holds(batch, 5, 1) &&
		tess_input_slot(input) == inner->ps.ps_ResultTupleSlot &&
		ops->get_batch(ops->find(inner->ps.ps_ResultTupleSlot)) == batch;
	tess_input_finish(input);
	result = result && ops->is_consumed(ops->find(inner->ps.ps_ResultTupleSlot)) &&
		tess_input_next(input) == NULL;

	/* After a rescan of the child, batches flow again. */
	inner->next_row = 0;
	tess_output_clear(inner->output);
	tess_input_rescan(input);
	batch = tess_input_next(input);
	result = result && batch != NULL && batch_holds(batch, 5, 1);
	tess_input_finish(input);
	tess_output_end(inner->output);
	tess_output_end(outer->output);
	PG_RETURN_BOOL(result);
}

Datum
tessera_test_input_errors(PG_FUNCTION_ARGS)
{
	const TessBindingOps *ops = tess_runtime_api()->binding_ops;
	int32		kind = PG_GETARG_INT32(0);
	FakeChild  *child = make_child(kind != 0);
	TessInput  *input;

	input = tess_input_create(CurrentMemoryContext, &child->ps);
	if (kind == 1)
	{
		tess_input_next(input);
		tess_input_next(input);
	}
	else if (kind == 2)
	{
		TupleTableSlot *slot = MakeSingleTupleTableSlot(make_desc(), &TTSOpsVirtual);

		slot->tts_values[0] = Int32GetDatum(1);
		slot->tts_isnull[0] = false;
		slot->tts_values[1] = PointerGetDatum(cstring_to_text("x"));
		slot->tts_isnull[1] = false;
		ExecStoreVirtualTuple(slot);
		child->forward_slot = slot;
		tess_input_next(input);
	}
	else if (kind == 3)
		tess_input_finish(input);
	else if (kind == 4)
	{
		TupleTableSlot *slot = MakeSingleTupleTableSlot(make_desc(), &TTSOpsVirtual);
		TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);

		layout.ncolumns = 2;
		layout.ntargets = 2;
		ops->attach(slot, &layout);
		slot->tts_values[0] = Int32GetDatum(1);
		slot->tts_isnull[0] = false;
		slot->tts_values[1] = PointerGetDatum(cstring_to_text("x"));
		slot->tts_isnull[1] = false;
		ExecStoreVirtualTuple(slot);
		child->forward_slot = slot;
		tess_input_next(input);
	}
	else
		tess_input_finished(input);
	elog(ERROR, "Tessera test expected an input error");
	PG_RETURN_VOID();
}

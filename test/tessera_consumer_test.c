/* Independent consumer: no producer symbols or storage declarations. */
#include "postgres.h"

#include "catalog/pg_type_d.h"
#include "executor/tuptable.h"
#include "fmgr.h"
#include "utils/builtins.h"
#include "utils/memutils.h"

#include "tessera/bridge.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_consumer);
PG_FUNCTION_INFO_V1(tessera_test_consumer_error);

/* Private to this module and owned by the caller's FmgrInfo context. */
typedef struct TestConsumer
{
	const TessBindingOps *ops;
	TessBinding *binding;
	TupleTableSlot *slot;
	int			sequence;
} TestConsumer;

static const TessBindingOps *
consumer_binding_ops(void)
{
	const TessApi *api = *find_rendezvous_variable(TESS_API_RENDEZVOUS);
	const TessBindingOps *ops;

	if (api == NULL || api->abi_version != TESS_API_ABI_VERSION ||
		api->struct_size < TESS_API_MIN_SIZE ||
		api->binding_ops == NULL || api->sources == NULL || api->nodes == NULL)
		elog(ERROR, "Tessera consumer could not find a compatible bridge");
	ops = api->binding_ops;
	if (ops->abi_version != TESS_BINDING_OPS_ABI_VERSION ||
		ops->struct_size < TESS_BINDING_OPS_MIN_SIZE ||
		ops->find == NULL || ops->get_layout == NULL ||
		ops->set_request == NULL || ops->get_batch == NULL ||
		ops->mark_consumed == NULL || ops->is_consumed == NULL)
		elog(ERROR, "Tessera consumer found incompatible binding operations");
	return ops;
}

static void
consumer_prepare(FunctionCallInfo fcinfo, TupleTableSlot *slot)
{
	TestConsumer *state;
	const TessLayout *layout;
	TessRequest request = {
		.struct_size = sizeof(TessRequest),
		.output_mode = TESS_OUTPUT_BATCH,
		.max_batch_rows = 70,
	};

	if (fcinfo->flinfo->fn_extra != NULL)
		elog(ERROR, "Tessera test consumer is already prepared");
	state = MemoryContextAllocZero(fcinfo->flinfo->fn_mcxt, sizeof(*state));
	state->ops = consumer_binding_ops();
	state->slot = slot;
	state->binding = state->ops->find(slot);
	if (state->binding == NULL)
		elog(ERROR, "Tessera consumer could not find the slot binding");
	layout = state->ops->get_layout(state->binding);
	if (layout->struct_size < TESS_LAYOUT_MIN_SIZE ||
		layout->ncolumns != 2 || layout->ntargets != 2 ||
		tess_layout_column(layout, 0) != 0 ||
		tess_layout_column(layout, 1) != 1 ||
		slot->tts_tupleDescriptor == NULL || slot->tts_tupleDescriptor->natts != 2 ||
		TupleDescAttr(slot->tts_tupleDescriptor, 0)->atttypid != INT4OID ||
		TupleDescAttr(slot->tts_tupleDescriptor, 1)->atttypid != TEXTOID)
		elog(ERROR, "Tessera consumer received an unexpected layout");
	request.filter_columns = bms_make_singleton(0);
	request.projection_columns = bms_make_singleton(1);
	state->ops->set_request(state->binding, &request);
	bms_free((Bitmapset *) request.filter_columns);
	bms_free((Bitmapset *) request.projection_columns);
	fcinfo->flinfo->fn_extra = state;
}

static void
consumer_validate_column(const TessDatumColumn *column, int nrows)
{
	if (column->struct_size != sizeof(TessDatumColumn) ||
		column->nrows != nrows || column->values == NULL || column->isnull == NULL)
		elog(ERROR, "Tessera consumer received an invalid Datum column");
}

static void
consumer_read(TestConsumer *state, bool fail)
{
	TessBatch  *batch = state->ops->get_batch(state->binding);
	TessDatumColumn numbers = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	TessDatumColumn labels = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	TessRowMask original;
	uint64		bits[2];
	int			row = -1;

	if (batch == NULL || state->ops->is_consumed(state->binding) ||
		batch->abi_version != TESS_BATCH_ABI_VERSION ||
		batch->struct_size < TESS_BATCH_MIN_SIZE ||
		batch->table_oid != InvalidOid || batch->ops == NULL ||
		batch->ops->abi_version != TESS_BATCH_OPS_ABI_VERSION ||
		batch->ops->struct_size < TESS_BATCH_OPS_MIN_SIZE ||
		batch->ops->get_datum_column == NULL || batch->rows.bits == NULL ||
		batch->rows.nrows <= 0 || batch->rows.nrows > 70)
		elog(ERROR, "Tessera consumer received an incompatible batch");
	original = batch->rows;
	original.bits = bits;
	memcpy(bits, batch->rows.bits,
		   tess_row_mask_word_count(batch->rows.nrows) * sizeof(uint64));
	batch->ops->get_datum_column(batch, 0, &original,
								 TESS_COLUMN_FOR_FILTER, &numbers);
	consumer_validate_column(&numbers, original.nrows);
	if (fail)
		elog(ERROR, "Tessera test consumer failed after column access");

	while ((row = tess_row_mask_next(&original, row)) >= 0)
	{
		int			expected = state->sequence * 1000 + row + 1;

		if (numbers.isnull[row] != (row % 5 == 0) ||
			(!numbers.isnull[row] && DatumGetInt32(numbers.values[row]) != expected))
			elog(ERROR, "Tessera consumer received an incorrect integer at row %d", row);
		if (numbers.isnull[row] || DatumGetInt32(numbers.values[row]) % 2 != 0)
			tess_row_mask_clear(&batch->rows, row);
	}
	batch->ops->get_datum_column(batch, 1, &batch->rows,
								 TESS_COLUMN_FOR_PROJECTION, &labels);
	consumer_validate_column(&labels, original.nrows);
	row = -1;
	while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
	{
		if (labels.isnull[row] != (row % 7 == 0))
			elog(ERROR, "Tessera consumer received an incorrect text null flag");
		if (!labels.isnull[row])
		{
			char		expected[32];
			char	   *actual = TextDatumGetCString(labels.values[row]);

			snprintf(expected, sizeof(expected), "row-%d",
					 state->sequence * 1000 + row + 1);
			if (strcmp(actual, expected) != 0)
				elog(ERROR, "Tessera consumer received incorrect text at row %d", row);
			pfree(actual);
		}
	}
	/* The first view, including rows removed since, must still be valid. */
	row = -1;
	while ((row = tess_row_mask_next(&original, row)) >= 0)
	{
		if (numbers.isnull[row] != (row % 5 == 0) ||
			(!numbers.isnull[row] && DatumGetInt32(numbers.values[row]) !=
			 state->sequence * 1000 + row + 1))
			elog(ERROR, "Tessera column access invalidated an earlier view");
	}
	state->ops->mark_consumed(state->binding);
	/* No batch or column access is allowed past this point. */
	state->sequence++;
}

static Datum
consumer_call(FunctionCallInfo fcinfo, bool fail)
{
	TupleTableSlot *slot = (TupleTableSlot *) PG_GETARG_POINTER(0);
	TestConsumer *state;

	if (slot == NULL)
		elog(ERROR, "Tessera test consumer requires a slot");
	if (PG_GETARG_BOOL(1))
		consumer_prepare(fcinfo, slot);
	else
	{
		state = fcinfo->flinfo->fn_extra;
		if (state == NULL || state->slot != slot)
			elog(ERROR, "Tessera test consumer was not prepared for this slot");
		consumer_read(state, fail);
	}
	PG_RETURN_BOOL(true);
}

/* Called through fmgr: true prepares the request, false consumes one batch. */
Datum
tessera_test_consumer(PG_FUNCTION_ARGS)
{
	return consumer_call(fcinfo, false);
}

/* Same interface, but execution raises ERROR with an unconsumed batch. */
Datum
tessera_test_consumer_error(PG_FUNCTION_ARGS)
{
	return consumer_call(fcinfo, true);
}

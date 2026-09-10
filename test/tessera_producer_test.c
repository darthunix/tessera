/* Independent producer and driver for the cross-module transfer test. */
#include "postgres.h"

#include "catalog/pg_proc.h"
#include "catalog/pg_type_d.h"
#include "executor/tuptable.h"
#include "fmgr.h"
#include "utils/builtins.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"

#include "tessera/bridge.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_modules);

#define TEST_MAX_ROWS 70

/* These counters outlive every batch, including its release callback. */
typedef struct TestTransfer
{
	const TessBindingOps *ops;
	TupleTableSlot *slot;
	TessBinding *binding;
	MemoryContext batch_context;
	FmgrInfo	consumer;
	uint64		requested[2][2];
	int			column_calls[2];
	int			published;
	int			released;
	bool		released_while_active;
} TestTransfer;

/* Only this module knows the batch's physical storage. */
typedef struct TestBatchData
{
	TessBatch	batch;
	MemoryContext context;
	TestTransfer *owner;
	int			sequence;
	uint64		bits[2];
	Datum		values[2][TEST_MAX_ROWS];
	bool		isnull[2][TEST_MAX_ROWS];
	uint64		materialized[2][2];
} TestBatchData;

static const TessBindingOps *
producer_binding_ops(void)
{
	const TessApi *api = *find_rendezvous_variable(TESS_API_RENDEZVOUS);
	const TessBindingOps *ops;

	if (api == NULL || api->abi_version != TESS_API_ABI_VERSION ||
		api->struct_size < TESS_API_MIN_SIZE ||
		api->binding_ops == NULL || api->sources == NULL || api->nodes == NULL)
		elog(ERROR, "Tessera producer could not find a compatible bridge");
	ops = api->binding_ops;
	if (ops->abi_version != TESS_BINDING_OPS_ABI_VERSION ||
		ops->struct_size < TESS_BINDING_OPS_MIN_SIZE ||
		ops->attach == NULL || ops->find == NULL ||
		ops->freeze_request == NULL || ops->publish_batch == NULL ||
		ops->get_batch == NULL || ops->is_consumed == NULL ||
		ops->release_batch == NULL || ops->detach == NULL)
		elog(ERROR, "Tessera producer found incompatible binding operations");
	return ops;
}

static void
producer_get_datum_column(TessBatch *batch, int column,
						  const TessRowMask *rows, TessColumnPurpose purpose,
						  TessDatumColumn *result)
{
	TestBatchData *data = batch->private_data;
	TestTransfer *owner = data->owner;
	int			row = -1;

	if (column < 0 || column > 1 || rows == NULL || rows->bits == NULL ||
		rows->nrows != batch->rows.nrows || result == NULL ||
		result->struct_size < TESS_DATUM_COLUMN_MIN_SIZE ||
		purpose != (column == 0 ? TESS_COLUMN_FOR_FILTER :
					TESS_COLUMN_FOR_PROJECTION))
		elog(ERROR, "Tessera producer received an invalid column request");

	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		uint64		bit = UINT64CONST(1) << (row % 64);
		int			value = data->sequence * 1000 + row + 1;

		if (!tess_row_mask_contains(&batch->rows, row))
			elog(ERROR, "Tessera consumer requested an inactive row");
		owner->requested[column][row / 64] |= bit;
		if ((data->materialized[column][row / 64] & bit) != 0)
			continue;
		data->isnull[column][row] = row % (column == 0 ? 5 : 7) == 0;
		if (!data->isnull[column][row])
		{
			if (column == 0)
				data->values[column][row] = Int32GetDatum(value);
			else
			{
				MemoryContext oldcontext;
				char		label[32];

				snprintf(label, sizeof(label), "row-%d", value);
				oldcontext = MemoryContextSwitchTo(data->context);
				data->values[column][row] = CStringGetTextDatum(label);
				MemoryContextSwitchTo(oldcontext);
			}
		}
		data->materialized[column][row / 64] |= bit;
	}
	owner->column_calls[column]++;
	result->values = data->values[column];
	result->isnull = data->isnull[column];
	result->nrows = batch->rows.nrows;
}

static void
producer_release(TessBatch *batch)
{
	TestBatchData *data = batch->private_data;
	TestTransfer *owner = data->owner;
	MemoryContext context = data->context;

	owner->released++;
	if (owner->ops->get_batch(owner->binding) != NULL)
		owner->released_while_active = true;
	owner->batch_context = NULL;
	/* Includes the envelope, masks, arrays, and pass-by-reference Datums. */
	MemoryContextDelete(context);
}

static const TessBatchOps producer_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = producer_get_datum_column,
	.release = producer_release,
};

static TessBatch *
producer_make_batch(TestTransfer *owner, int nrows, int selection, int sequence)
{
	TestBatchData *data;
	TessBatch	batch = {
		TESS_ABI_INITIALIZER(TESS_BATCH_ABI_VERSION, TessBatch),
		.rows = {nrows, NULL},
		.table_oid = InvalidOid,
		.ops = &producer_batch_ops,
	};
	int			row;

	owner->batch_context = AllocSetContextCreate(CurrentMemoryContext,
												 "Tessera test batch", ALLOCSET_DEFAULT_SIZES);
	data = MemoryContextAllocZero(owner->batch_context, sizeof(*data));
	data->context = owner->batch_context;
	data->owner = owner;
	data->sequence = sequence;
	data->batch = batch;
	data->batch.rows.bits = data->bits;
	data->batch.private_data = data;
	for (row = 0; row < nrows; row++)
	{
		/* Full, sparse across the word boundary, or empty selection. */
		if (selection == 0 ||
			(selection == 1 && (row < 2 || row >= 62)))
			data->bits[row / 64] |= UINT64CONST(1) << (row % 64);
	}
	return &data->batch;
}

static void
producer_run_case(Oid consumer_oid, int nrows, int selection)
{
	MemoryContext oldcontext = CurrentMemoryContext;
	MemoryContext context;
	TestTransfer *state;

	context = AllocSetContextCreate(oldcontext,
									"Tessera module transfer test", ALLOCSET_DEFAULT_SIZES);
	state = MemoryContextAllocZero(context, sizeof(*state));
	state->ops = producer_binding_ops();
	PG_TRY();
	{
		TupleDesc	desc;
		TessLayout	layout = {
			.struct_size = sizeof(TessLayout),
			.ncolumns = 2,
			.ntargets = 2,
		};
		const TessRequest *request;
		int			sequence;

		MemoryContextSwitchTo(context);
		desc = CreateTemplateTupleDesc(2);
		TupleDescInitEntry(desc, 1, "number", INT4OID, -1, 0);
		TupleDescInitEntry(desc, 2, "label", TEXTOID, -1, 0);
		TupleDescFinalize(desc);
		state->slot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
		state->binding = state->ops->attach(state->slot, &layout);
		fmgr_info_cxt(consumer_oid, &state->consumer, context);
		if (!DatumGetBool(FunctionCall2(&state->consumer,
										PointerGetDatum(state->slot),
										BoolGetDatum(true))))
			elog(ERROR, "Tessera consumer preparation failed");
		request = state->ops->freeze_request(state->binding);
		if (request->struct_size < TESS_REQUEST_MIN_SIZE ||
			request->output_mode != TESS_OUTPUT_BATCH ||
			request->max_batch_rows != TEST_MAX_ROWS ||
			bms_num_members(request->filter_columns) != 1 ||
			!bms_is_member(0, request->filter_columns) ||
			bms_num_members(request->projection_columns) != 1 ||
			!bms_is_member(1, request->projection_columns))
			elog(ERROR, "Tessera producer did not receive the consumer request");

		for (sequence = 0; sequence < 2; sequence++)
		{
			TessBatch  *batch;
			uint64		expected[2][2] = {{0}};
			int			row = -1;

			memset(state->requested, 0, sizeof(state->requested));
			memset(state->column_calls, 0, sizeof(state->column_calls));
			batch = producer_make_batch(state, nrows, selection, sequence);
			while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
			{
				uint64		bit = UINT64CONST(1) << (row % 64);

				expected[0][row / 64] |= bit;
				if (row % 5 != 0 && (row + 1) % 2 == 0)
					expected[1][row / 64] |= bit;
			}
			state->ops->publish_batch(state->binding, batch);
			state->published++;
			if (state->ops->is_consumed(state->binding))
				elog(ERROR, "Tessera published batch is already consumed");
			if (!DatumGetBool(FunctionCall2(&state->consumer,
											PointerGetDatum(state->slot),
											BoolGetDatum(false))))
				elog(ERROR, "Tessera consumer verification failed");
			if (!state->ops->is_consumed(state->binding) ||
				state->ops->get_batch(state->binding) != batch ||
				state->released != sequence)
				elog(ERROR, "Tessera consumption changed batch ownership");
			state->ops->release_batch(state->binding);
			state->ops->release_batch(state->binding);
			if (state->released != sequence + 1 ||
				state->ops->get_batch(state->binding) != NULL ||
				!state->ops->is_consumed(state->binding) ||
				state->batch_context != NULL ||
				state->column_calls[0] != 1 || state->column_calls[1] != 1 ||
				memcmp(state->requested, expected, sizeof(expected)) != 0)
				elog(ERROR, "Tessera producer observed incorrect transfer results");
		}
	}
	PG_FINALLY();
	{
		bool		clean;

		/* On ERROR the current context may belong to the consumer. */
		MemoryContextSwitchTo(context);
		state->ops->detach(state->binding);
		clean = state->released == state->published &&
			!state->released_while_active &&
			state->ops->find(state->slot) == NULL;
		if (state->slot != NULL)
			ExecDropSingleTupleTableSlot(state->slot);
		/* Also handles failure while constructing an unpublished batch. */
		if (state->batch_context != NULL)
			MemoryContextDelete(state->batch_context);
		MemoryContextSwitchTo(oldcontext);
		MemoryContextDelete(context);
		if (!clean)
			elog(ERROR, "Tessera producer cleanup did not release each batch once");
	}
	PG_END_TRY();
}

/* SQL supplies a function identity, never a pointer or a batch representation. */
Datum
tessera_test_modules(PG_FUNCTION_ARGS)
{
	Oid			consumer_oid = PG_GETARG_OID(0);
	Oid		   *argtypes;
	Oid			result_type;
	int			nargs;
	const int	sizes[] = {1, 3, 63, 64, 65, 70};
	int			i;

	result_type = get_func_signature(consumer_oid, &argtypes, &nargs);
	if (nargs != 2 || argtypes[0] != INTERNALOID || argtypes[1] != BOOLOID ||
		result_type != BOOLOID || get_func_retset(consumer_oid) ||
		get_func_prokind(consumer_oid) != PROKIND_FUNCTION)
		elog(ERROR, "Tessera test consumer must have signature (internal, boolean) returning boolean");
	pfree(argtypes);
	for (i = 0; i < lengthof(sizes); i++)
		producer_run_case(consumer_oid, sizes[i], 0);
	producer_run_case(consumer_oid, TEST_MAX_ROWS, 1);
	producer_run_case(consumer_oid, TEST_MAX_ROWS, 2);
	PG_RETURN_BOOL(true);
}

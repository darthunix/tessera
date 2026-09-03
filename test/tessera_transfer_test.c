#include "postgres.h"

#include "executor/tuptable.h"
#include "fmgr.h"
#include "utils/memutils.h"

#include "tessera/bridge.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_transfer);
PG_FUNCTION_INFO_V1(tessera_test_invalid_batch);
PG_FUNCTION_INFO_V1(tessera_test_double_publish);
PG_FUNCTION_INFO_V1(tessera_test_publish_freezes_request);

typedef struct TestBatchData
{
	const TessApi *api;
	TessBinding *binding;
	int			release_calls;
	bool		saw_active_during_release;
} TestBatchData;

static const TessApi *
test_api(void)
{
	const TessApi *api;
	void	  **rendezvous;

	rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	api = *rendezvous;
	if (api == NULL || api->abi_version != TESS_API_ABI_VERSION ||
		api->struct_size < TESS_API_MIN_SIZE)
		elog(ERROR, "Tessera test could not find a compatible bridge");
	return api;
}

static void
test_get_datum_column(TessBatch *batch, int column,
					  const TessRowMask *rows, TessColumnPurpose purpose,
					  TessDatumColumn *result)
{
	(void) column;
	(void) rows;
	(void) purpose;
	result->values = NULL;
	result->isnull = NULL;
	result->nrows = batch->rows.nrows;
}

static void
test_release(TessBatch *batch)
{
	TestBatchData *data = batch->private_data;

	data->release_calls++;
	if (data->api != NULL && data->api->get_batch(data->binding) != NULL)
		data->saw_active_during_release = true;
}

static const TessBatchOps test_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = test_get_datum_column,
	.release = test_release,
};

static TessBinding *
make_binding(const TessApi *api, TupleTableSlot **slot)
{
	TessLayout	layout = {
		.struct_size = sizeof(TessLayout),
		.ncolumns = 1,
		.ntargets = 1,
	};

	*slot = MakeSingleTupleTableSlot(NULL, &TTSOpsVirtual);
	return api->attach(*slot, &layout);
}

static TessBatch
make_batch(TestBatchData *data, uint64 *bits, int nrows,
		   const TessBatchOps *ops)
{
	TessBatch	batch = {
		TESS_ABI_INITIALIZER(TESS_BATCH_ABI_VERSION, TessBatch),
		.rows = {nrows, bits},
		.ops = ops,
		.private_data = data,
	};

	return batch;
}

Datum
tessera_test_transfer(PG_FUNCTION_ARGS)
{
	const TessApi *api = test_api();
	MemoryContext context;
	MemoryContext oldcontext;
	TupleTableSlot *slot;
	TessBinding *binding;
	TestBatchData data = {0};
	uint64		active_bits = UINT64_MAX;
	uint64		empty_bits = 0;
	TessBatchOps short_ops = test_ops;
	TessBatch	batch;
	TessRequest request = {
		.struct_size = sizeof(TessRequest),
		.output_mode = TESS_OUTPUT_BATCH,
		.max_batch_rows = 64,
	};
	bool		result;

	context = AllocSetContextCreate(CurrentMemoryContext,
		"Tessera transfer test", ALLOCSET_DEFAULT_SIZES);
	oldcontext = MemoryContextSwitchTo(context);
	binding = make_binding(api, &slot);
	data.api = api;
	data.binding = binding;
	api->set_request(binding, &request);
	batch = make_batch(&data, &active_bits, 64, &test_ops);

	api->publish_batch(binding, &batch);
	result = api->get_batch(binding) == &batch;
	api->release_batch(binding);
	result = result && api->get_batch(binding) == NULL &&
		data.release_calls == 1;
	api->release_batch(binding);
	api->release_batch(NULL);
	result = result && data.release_calls == 1 &&
		api->get_batch(NULL) == NULL;

	batch.rows.bits = &empty_bits;
	api->publish_batch(binding, &batch);
	api->release_batch(binding);
	result = result && data.release_calls == 2;

	short_ops.struct_size = TESS_BATCH_OPS_MIN_SIZE;
	batch.ops = &short_ops;
	api->publish_batch(binding, &batch);
	api->release_batch(binding);
	result = result && data.release_calls == 2;

	batch.ops = &test_ops;
	api->publish_batch(binding, &batch);
	api->detach(binding);
	result = result && data.release_calls == 3 &&
		!data.saw_active_during_release &&
		api->find_binding(slot) == NULL;
	ExecDropSingleTupleTableSlot(slot);
	MemoryContextSwitchTo(oldcontext);
	MemoryContextDelete(context);

	PG_RETURN_BOOL(result);
}

Datum
tessera_test_invalid_batch(PG_FUNCTION_ARGS)
{
	const TessApi *api = test_api();
	TupleTableSlot *slot;
	TessBinding *binding = make_binding(api, &slot);
	TestBatchData data = {0};
	uint64		bits = 1;
	TessBatchOps ops = test_ops;
	TessBatch	batch = make_batch(&data, &bits, 1, &ops);
	TessRequest request = {
		.struct_size = sizeof(TessRequest),
		.output_mode = TESS_OUTPUT_BATCH,
		.max_batch_rows = 1,
	};
	int32		kind = PG_GETARG_INT32(0);

	if (kind == 0)
		batch.abi_version++;
	else if (kind == 1)
		batch.struct_size = 0;
	else if (kind == 2)
		batch.ops = NULL;
	else if (kind == 3)
		ops.abi_version++;
	else if (kind == 4)
		ops.struct_size = 0;
	else if (kind == 5)
		ops.get_datum_column = NULL;
	else if (kind == 6)
		batch.rows.nrows = 0;
	else if (kind == 7)
		batch.rows.bits = NULL;
	else if (kind == 8)
		bits = UINT64CONST(2);
	else if (kind == 9)
	{
		bits = UINT64CONST(3);
		batch.rows.nrows = 2;
		api->set_request(binding, &request);
	}
	else if (kind == 10)
		api->publish_batch(binding, NULL);
	else
		elog(ERROR, "unknown Tessera invalid-batch test");
	api->publish_batch(binding, &batch);
	PG_RETURN_VOID();
}

Datum
tessera_test_double_publish(PG_FUNCTION_ARGS)
{
	const TessApi *api = test_api();
	TupleTableSlot *slot;
	TessBinding *binding = make_binding(api, &slot);
	TestBatchData data = {0};
	uint64		bits = 1;
	TessBatch	batch = make_batch(&data, &bits, 1, &test_ops);

	api->publish_batch(binding, &batch);
	api->publish_batch(binding, &batch);
	PG_RETURN_VOID();
}

Datum
tessera_test_publish_freezes_request(PG_FUNCTION_ARGS)
{
	const TessApi *api = test_api();
	TupleTableSlot *slot;
	TessBinding *binding = make_binding(api, &slot);
	TestBatchData data = {0};
	uint64		bits = 1;
	TessBatch	batch = make_batch(&data, &bits, 1, &test_ops);
	TessRequest request = {
		.struct_size = sizeof(TessRequest),
		.output_mode = TESS_OUTPUT_ROWS,
	};

	api->publish_batch(binding, &batch);
	api->set_request(binding, &request);
	PG_RETURN_VOID();
}

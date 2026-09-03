#include "postgres.h"

#include "executor/tuptable.h"
#include "fmgr.h"
#include "nodes/bitmapset.h"
#include "utils/memutils.h"

#include "tessera/bridge.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_binding);
PG_FUNCTION_INFO_V1(tessera_test_binding_context_reset);
PG_FUNCTION_INFO_V1(tessera_test_duplicate_binding);
PG_FUNCTION_INFO_V1(tessera_test_invalid_layout);
PG_FUNCTION_INFO_V1(tessera_test_invalid_request);
PG_FUNCTION_INFO_V1(tessera_test_request_after_freeze);

static const TessBindingOps *
test_binding_ops(void)
{
	const TessApi *api;
	const TessBindingOps *ops;
	void	  **rendezvous;

	rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	api = *rendezvous;
	if (api == NULL || api->abi_version != TESS_API_ABI_VERSION ||
		api->struct_size < TESS_API_MIN_SIZE)
		elog(ERROR, "Tessera test could not find a compatible bridge");
	ops = api->binding_ops;
	if (ops == NULL ||
		ops->abi_version != TESS_BINDING_OPS_ABI_VERSION ||
		ops->struct_size < TESS_BINDING_OPS_MIN_SIZE)
		elog(ERROR, "Tessera test could not find compatible binding operations");
	return ops;
}

Datum
tessera_test_binding(PG_FUNCTION_ARGS)
{
	const TessBindingOps *ops = test_binding_ops();
	MemoryContext context;
	MemoryContext oldcontext;
	TupleTableSlot *first_slot;
	TupleTableSlot *second_slot;
	int			target_columns[] = {0, 2};
	TessLayout mapped = {
		.struct_size = sizeof(TessLayout),
		.ncolumns = 70,
		.ntargets = 2,
		.target_columns = target_columns,
	};
	TessLayout identity = {
		.struct_size = sizeof(TessLayout),
		.ncolumns = 1,
		.ntargets = 1,
	};
	TessBinding *first;
	TessBinding *second;
	const TessLayout *stored_layout;
	const TessRequest *stored_request;
	const TessRequest *default_request;
	Bitmapset  *filters;
	Bitmapset  *projections;
	TessRequest request = {
		.struct_size = sizeof(TessRequest),
		.output_mode = TESS_OUTPUT_ROWS,
		.max_batch_rows = 1,
	};
	bool		result;

	context = AllocSetContextCreate(CurrentMemoryContext,
		"Tessera binding test", ALLOCSET_DEFAULT_SIZES);
	oldcontext = MemoryContextSwitchTo(context);
	first_slot = MakeSingleTupleTableSlot(NULL, &TTSOpsVirtual);
	second_slot = MakeSingleTupleTableSlot(NULL, &TTSOpsVirtual);
	first = ops->attach(first_slot, &mapped);
	second = ops->attach(second_slot, &identity);
	target_columns[1] = 1;

	ops->set_request(first, &request);
	filters = bms_make_singleton(0);
	filters = bms_add_member(filters, 65);
	projections = bms_make_singleton(2);
	projections = bms_add_member(projections, 65);
	request.filter_columns = filters;
	request.projection_columns = projections;
	request.output_mode = TESS_OUTPUT_BATCH;
	request.max_batch_rows = 64;
	ops->set_request(first, &request);
	filters = bms_del_member(filters, 0);
	projections = bms_del_member(projections, 2);

	stored_layout = ops->get_layout(first);
	stored_request = ops->freeze_request(first);
	default_request = ops->freeze_request(second);
	result = ops->find(first_slot) == first &&
		ops->find(second_slot) == second &&
		stored_layout->target_columns != target_columns &&
		tess_layout_column(stored_layout, 1) == 2 &&
		stored_request == ops->freeze_request(first) &&
		stored_request->filter_columns != filters &&
		stored_request->projection_columns != projections &&
		bms_is_member(0, stored_request->filter_columns) &&
		bms_is_member(65, stored_request->filter_columns) &&
		bms_is_member(2, stored_request->projection_columns) &&
		bms_is_member(65, stored_request->projection_columns) &&
		stored_request->output_mode == TESS_OUTPUT_BATCH &&
		stored_request->max_batch_rows == 64 &&
		default_request->filter_columns == NULL &&
		default_request->projection_columns == NULL &&
		default_request->output_mode == TESS_OUTPUT_ROWS &&
		default_request->max_batch_rows == 0;
	bms_free(filters);
	bms_free(projections);

	ops->detach(first);
	result = result && ops->find(first_slot) == NULL &&
		ops->find(second_slot) == second;
	ops->detach(second);
	ops->detach(NULL);
	ExecDropSingleTupleTableSlot(first_slot);
	ExecDropSingleTupleTableSlot(second_slot);
	MemoryContextSwitchTo(oldcontext);
	MemoryContextDelete(context);

	PG_RETURN_BOOL(result);
}

Datum
tessera_test_binding_context_reset(PG_FUNCTION_ARGS)
{
	const TessBindingOps *ops = test_binding_ops();
	MemoryContext context;
	MemoryContext oldcontext;
	TupleTableSlot *slot;
	TessLayout layout = {
		.struct_size = sizeof(TessLayout),
		.ncolumns = 1,
		.ntargets = 1,
	};

	context = AllocSetContextCreate(CurrentMemoryContext,
		"Tessera binding reset test", ALLOCSET_DEFAULT_SIZES);
	oldcontext = MemoryContextSwitchTo(context);
	slot = MakeSingleTupleTableSlot(NULL, &TTSOpsVirtual);
	ops->attach(slot, &layout);
	MemoryContextSwitchTo(oldcontext);
	MemoryContextDelete(context);

	PG_RETURN_BOOL(ops->find(slot) == NULL);
}

Datum
tessera_test_duplicate_binding(PG_FUNCTION_ARGS)
{
	const TessBindingOps *ops = test_binding_ops();
	TupleTableSlot *slot = MakeSingleTupleTableSlot(NULL, &TTSOpsVirtual);
	TessLayout layout = {
		.struct_size = sizeof(TessLayout),
		.ncolumns = 1,
		.ntargets = 1,
	};

	ops->attach(slot, &layout);
	ops->attach(slot, &layout);
	PG_RETURN_VOID();
}

Datum
tessera_test_invalid_layout(PG_FUNCTION_ARGS)
{
	const TessBindingOps *ops = test_binding_ops();
	TupleTableSlot *slot = MakeSingleTupleTableSlot(NULL, &TTSOpsVirtual);
	int			column = 1;
	TessLayout layout = {
		.struct_size = sizeof(TessLayout),
		.ncolumns = 1,
		.ntargets = 1,
	};
	int32		kind = PG_GETARG_INT32(0);

	if (kind == 0)
		layout.ntargets = 2;
	else if (kind == 1)
		layout.target_columns = &column;
	else if (kind == 2)
		layout.struct_size = 0;
	else
		elog(ERROR, "unknown Tessera invalid-layout test");
	ops->attach(slot, &layout);
	PG_RETURN_VOID();
}

Datum
tessera_test_invalid_request(PG_FUNCTION_ARGS)
{
	const TessBindingOps *ops = test_binding_ops();
	TupleTableSlot *slot = MakeSingleTupleTableSlot(NULL, &TTSOpsVirtual);
	TessLayout layout = {
		.struct_size = sizeof(TessLayout),
		.ncolumns = 1,
		.ntargets = 1,
	};
	TessRequest request = {
		.struct_size = sizeof(TessRequest),
		.output_mode = TESS_OUTPUT_ROWS,
	};
	TessBinding *binding = ops->attach(slot, &layout);
	int32		kind = PG_GETARG_INT32(0);

	if (kind == 0)
		request.output_mode = (TessOutputMode) -1;
	else if (kind == 1)
		request.max_batch_rows = -1;
	else if (kind == 2)
		request.filter_columns = bms_make_singleton(1);
	else if (kind == 3)
		request.struct_size = 0;
	else
		elog(ERROR, "unknown Tessera invalid-request test");
	ops->set_request(binding, &request);
	PG_RETURN_VOID();
}

Datum
tessera_test_request_after_freeze(PG_FUNCTION_ARGS)
{
	const TessBindingOps *ops = test_binding_ops();
	TupleTableSlot *slot = MakeSingleTupleTableSlot(NULL, &TTSOpsVirtual);
	TessLayout layout = {
		.struct_size = sizeof(TessLayout),
		.ncolumns = 1,
		.ntargets = 1,
	};
	TessRequest request = {
		.struct_size = sizeof(TessRequest),
		.output_mode = TESS_OUTPUT_ROWS,
	};
	TessBinding *binding = ops->attach(slot, &layout);

	ops->freeze_request(binding);
	ops->set_request(binding, &request);
	PG_RETURN_VOID();
}

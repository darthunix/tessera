#include "postgres.h"

#include <string.h>

#include "catalog/pg_type_d.h"
#include "executor/instrument.h"
#include "executor/tuptable.h"
#include "fmgr.h"
#include "utils/builtins.h"
#include "utils/memutils.h"

#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_output_rows);
PG_FUNCTION_INFO_V1(tessera_test_output_batch);
PG_FUNCTION_INFO_V1(tessera_test_output_layout);
PG_FUNCTION_INFO_V1(tessera_test_output_errors);

/* A descriptor of (int4, text), the builder's input and the usual slot. */
static TupleDesc
make_desc(void)
{
	TupleDesc	desc = CreateTemplateTupleDesc(2);

	TupleDescInitEntry(desc, 1, "a", INT4OID, -1, 0);
	TupleDescInitEntry(desc, 2, "c", TEXTOID, -1, 0);
	return desc;
}

static TessBuilder *
make_builder(TupleDesc desc)
{
	TessBuilderConfig config = TESS_STRUCT_INITIALIZER(TessBuilderConfig);

	config.parent_context = CurrentMemoryContext;
	config.tuple_desc = desc;
	config.ncolumns = 2;
	config.capacity = 3;
	return tess_builder_create(&config);
}

/* Three rows: (1, "one"), (NULL, "two"), (3 + offset, "three"). */
static TessBatch *
make_batch(TessBuilder *builder, TupleTableSlot *input, int offset)
{
	static const char *const texts[] = {"one", "two", "three"};
	int			row;

	tess_builder_reset(builder);
	for (row = 0; row < 3; row++)
	{
		ExecClearTuple(input);
		input->tts_values[0] = Int32GetDatum(row == 2 ? 3 + offset : 1);
		input->tts_isnull[0] = row == 1;
		input->tts_values[1] = PointerGetDatum(cstring_to_text(texts[row]));
		input->tts_isnull[1] = false;
		ExecStoreVirtualTuple(input);
		tess_builder_append_slot(builder, input);
	}
	return tess_builder_finish(builder, 42);
}

static TessLayout
identity_layout(void)
{
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);

	layout.ncolumns = 2;
	layout.ntargets = 2;
	layout.target_columns = NULL;
	return layout;
}

static bool
shows(TupleTableSlot *slot, int32 a, bool a_null, const char *c)
{
	return !TupIsNull(slot) && slot->tts_nvalid == 2 &&
		slot->tts_isnull[0] == a_null &&
		(a_null || DatumGetInt32(slot->tts_values[0]) == a) &&
		!slot->tts_isnull[1] &&
		strcmp(text_to_cstring(DatumGetTextPP(slot->tts_values[1])), c) == 0 &&
		slot->tts_tableOid == 42;
}

Datum
tessera_test_output_rows(PG_FUNCTION_ARGS)
{
	const TessBindingOps *ops = tess_runtime_api()->binding_ops;
	TupleDesc	desc = make_desc();
	TupleTableSlot *input = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	TupleTableSlot *slot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	TessBuilder *builder = make_builder(desc);
	TessLayout	layout = identity_layout();
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
	PlanState  *ps = palloc0(sizeof(PlanState));
	TessOutput *output;
	TessBinding *binding;
	TessBatch  *batch;
	bool		result;

	ps->instrument = InstrAllocNode(INSTRUMENT_ROWS, false);
	output = tess_output_create(CurrentMemoryContext, ps, slot, &layout);
	binding = tess_output_binding(output);
	result = binding != NULL && ops->find(slot) == binding &&
		tess_output_finished(output);
	request.output_mode = TESS_OUTPUT_ROWS;
	ops->set_request(binding, &request);
	result = result && tess_output_request(output)->output_mode == TESS_OUTPUT_ROWS;

	batch = make_batch(builder, input, 0);
	result = result && tess_output_publish(output, batch) == slot &&
		shows(slot, 1, false, "one") && ops->get_batch(binding) == batch &&
		!tess_output_finished(output);
	result = result && tess_output_select(output, 2) == slot &&
		shows(slot, 3, false, "three");
	result = result && tess_output_select(output, 1) == slot &&
		shows(slot, 0, true, "two");
	tess_output_finish(output);
	tess_output_finish(output);
	result = result && tess_output_finished(output) &&
		ops->get_batch(binding) == batch;

	/* The next batch reuses the builder after the previous one is released. */
	tess_output_release(output);
	result = result && ops->get_batch(binding) == NULL && TupIsNull(slot);
	batch = make_batch(builder, input, 10);
	result = result && tess_output_publish(output, batch) == slot &&
		shows(slot, 1, false, "one") && tess_output_select(output, 2) == slot &&
		shows(slot, 13, false, "three");
	/* A row-wise parent counts rows itself: no instrumentation change. */
	result = result && ps->instrument->tuplecount == 0;
	tess_output_clear(output);
	result = result && ops->get_batch(binding) == NULL &&
		tess_output_finished(output);
	tess_output_end(output);
	result = result && ops->find(slot) == NULL;
	ExecDropSingleTupleTableSlot(slot);
	ExecDropSingleTupleTableSlot(input);
	PG_RETURN_BOOL(result);
}

Datum
tessera_test_output_batch(PG_FUNCTION_ARGS)
{
	const TessBindingOps *ops = tess_runtime_api()->binding_ops;
	TupleDesc	desc = make_desc();
	TupleTableSlot *input = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	TupleTableSlot *slot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	TessBuilder *builder = make_builder(desc);
	TessLayout	layout = identity_layout();
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
	PlanState  *ps = palloc0(sizeof(PlanState));
	TessOutput *output;
	TessBatch  *batch;
	TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	bool		result;

	ps->instrument = InstrAllocNode(INSTRUMENT_ROWS, false);
	output = tess_output_create(CurrentMemoryContext, ps, slot, &layout);
	request.output_mode = TESS_OUTPUT_BATCH;
	ops->set_request(tess_output_binding(output), &request);

	/* The producer narrows its own batch to rows 0 and 2 before publishing. */
	batch = make_batch(builder, input, 0);
	batch->rows.bits[0] = 5;
	result = tess_output_publish(output, batch) == slot &&
		shows(slot, 1, false, "one") && ps->instrument->tuplecount == 1;

	/* A batch-aware parent reads the batch through the binding. */
	batch = ops->get_batch(tess_output_binding(output));
	batch->ops->get_datum_column(batch, 0, &batch->rows,
								 TESS_COLUMN_FOR_FILTER, &column);
	result = result && batch != NULL && column.nrows == 3 &&
		DatumGetInt32(column.values[0]) == 1 && column.isnull[1] &&
		DatumGetInt32(column.values[2]) == 3;
	ops->mark_consumed(tess_output_binding(output));
	result = result && tess_output_finished(output);
	tess_output_end(output);
	ExecDropSingleTupleTableSlot(slot);
	ExecDropSingleTupleTableSlot(input);
	PG_RETURN_BOOL(result);
}

Datum
tessera_test_output_layout(PG_FUNCTION_ARGS)
{
	const TessBindingOps *ops = tess_runtime_api()->binding_ops;
	TupleDesc	desc = make_desc();
	TupleDesc	swapped = CreateTemplateTupleDesc(2);
	TupleTableSlot *input = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	TupleTableSlot *slot;
	TessBuilder *builder = make_builder(desc);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);
	const int	map[2] = {1, 0};
	TessOutput *output;
	TessBatch  *batch;
	bool		result;

	/* The slot is (text, int4): its attributes read batch columns 1 and 0. */
	TupleDescInitEntry(swapped, 1, "c", TEXTOID, -1, 0);
	TupleDescInitEntry(swapped, 2, "a", INT4OID, -1, 0);
	slot = MakeSingleTupleTableSlot(swapped, &TTSOpsVirtual);
	layout.ncolumns = 2;
	layout.ntargets = 2;
	layout.target_columns = map;
	output = tess_output_create(CurrentMemoryContext, NULL, slot, &layout);
	batch = make_batch(builder, input, 0);
	result = tess_output_publish(output, batch) == slot && !TupIsNull(slot) &&
		strcmp(text_to_cstring(DatumGetTextPP(slot->tts_values[0])), "one") == 0 &&
		DatumGetInt32(slot->tts_values[1]) == 1;
	result = result && tess_output_select(output, 1) == slot &&
		strcmp(text_to_cstring(DatumGetTextPP(slot->tts_values[0])), "two") == 0 &&
		slot->tts_isnull[1];
	tess_output_end(output);
	result = result && ops->find(slot) == NULL;
	ExecDropSingleTupleTableSlot(slot);
	ExecDropSingleTupleTableSlot(input);
	PG_RETURN_BOOL(result);
}

Datum
tessera_test_output_errors(PG_FUNCTION_ARGS)
{
	int32		kind = PG_GETARG_INT32(0);
	TupleDesc	desc = make_desc();
	TupleTableSlot *input = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	TupleTableSlot *slot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	TessBuilder *builder = make_builder(desc);
	TessLayout	layout = identity_layout();
	const int	missing[2] = {0, -1};
	TessOutput *output;
	TessBatch  *batch;

	if (kind == 0)
		slot = MakeSingleTupleTableSlot(desc, &TTSOpsMinimalTuple);
	else if (kind == 1)
		layout.ntargets = 1;
	else if (kind == 2)
		layout.target_columns = missing;
	output = tess_output_create(CurrentMemoryContext, NULL, slot, &layout);
	batch = make_batch(builder, input, 0);
	if (kind == 3)
	{
		tess_output_publish(output, batch);
		tess_output_publish(output, batch);
	}
	else if (kind == 4)
	{
		batch->rows.bits[0] = 5;
		tess_output_publish(output, batch);
		tess_output_select(output, 1);
	}
	else if (kind == 5)
	{
		batch->rows.bits[0] = 0;
		tess_output_publish(output, batch);
	}
	else
	{
		tess_output_publish(output, batch);
		tess_output_release(output);
	}
	elog(ERROR, "Tessera test expected an output error");
	PG_RETURN_VOID();
}

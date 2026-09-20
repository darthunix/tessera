#include "postgres.h"

#include <string.h>

#include "catalog/pg_type_d.h"
#include "executor/executor.h"
#include "executor/instrument.h"
#include "fmgr.h"
#include "nodes/extensible.h"
#include "utils/builtins.h"
#include "utils/memutils.h"

#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_unary_batches);
PG_FUNCTION_INFO_V1(tessera_test_unary_rows);
PG_FUNCTION_INFO_V1(tessera_test_unary_errors);

/*
 * A stand-in for a batch child: a custom scan state whose ExecProcNode
 * publishes batches of (a, c) rows through an output on its result slot,
 * as many rows per batch as the request allows, with c NULL in every third
 * row. Its rescan callback restarts the rows.
 */
typedef struct FakeChild
{
	CustomScanState css;
	TessOutput *output;
	TessBuilder *builder;
	TupleTableSlot *input_slot;
	int			next_row;
	int			nrows;
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

	if (child->next_row >= child->nrows)
		return NULL;
	request = tess_output_request(child->output);
	limit = request->max_batch_rows > 0 ? request->max_batch_rows : child->nrows;
	tess_output_release(child->output);
	tess_builder_reset(child->builder);
	while (child->next_row < child->nrows && count < limit)
	{
		char		text[16];
		int			value = child->next_row + 1;

		snprintf(text, sizeof(text), "row%d", value);
		ExecClearTuple(child->input_slot);
		child->input_slot->tts_values[0] = Int32GetDatum(value);
		child->input_slot->tts_isnull[0] = false;
		child->input_slot->tts_values[1] = PointerGetDatum(cstring_to_text(text));
		child->input_slot->tts_isnull[1] = value % 3 == 0;
		ExecStoreVirtualTuple(child->input_slot);
		tess_builder_append_slot(child->builder, child->input_slot);
		child->next_row++;
		count++;
	}
	batch = tess_builder_finish(child->builder, InvalidOid);
	return tess_output_publish(child->output, batch);
}

static void
fake_rescan(CustomScanState *css)
{
	FakeChild  *child = (FakeChild *) css;

	tess_output_clear(child->output);
	child->next_row = 0;
}

static const CustomExecMethods fake_methods = {
	.CustomName = "fake",
	.ReScanCustomScan = fake_rescan,
};

static FakeChild *
make_child(bool bound)
{
	FakeChild  *child = (FakeChild *)
		newNode(sizeof(FakeChild), T_CustomScanState);
	TupleDesc	desc = make_desc();
	TessBuilderConfig config = TESS_STRUCT_INITIALIZER(TessBuilderConfig);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);

	child->css.methods = &fake_methods;
	child->css.ss.ps.ExecProcNode = fake_exec;
	child->css.ss.ps.ps_ResultTupleSlot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
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
		child->output = tess_output_create(CurrentMemoryContext, &child->css.ss.ps,
										   child->css.ss.ps.ps_ResultTupleSlot,
										   &layout);
	return child;
}

/* The node under test: a result slot and instrumentation are enough. */
static CustomScanState *
make_node(TupleDesc desc)
{
	CustomScanState *node = makeNode(CustomScanState);

	node->ss.ps.ps_ResultTupleSlot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	node->ss.ps.instrument = InstrAllocNode(INSTRUMENT_ROWS, false);
	return node;
}

static TessUnaryConfig
make_config(CustomScanState *node, FakeChild *child, TessLayout *layout)
{
	TessUnaryConfig config = TESS_STRUCT_INITIALIZER(TessUnaryConfig);

	config.parent_context = CurrentMemoryContext;
	config.node = node;
	config.child = &child->css.ss.ps;
	config.layout = layout;
	return config;
}

/* Keep the rows with an even a; count the calls. */
static int
drop_odd(void *private_data, TessBatch *batch, int rows)
{
	TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	int		   *calls = private_data;
	int			row = -1;
	int			kept = 0;

	(*calls)++;
	batch->ops->get_datum_column(batch, 0, &batch->rows,
								 TESS_COLUMN_FOR_FILTER, &column);
	while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
	{
		if (DatumGetInt32(column.values[row]) % 2 != 0)
			tess_row_mask_clear(&batch->rows, row);
		else
			kept++;
	}
	return kept;
}

/* True when the batch's selected a values are exactly the given ones. */
static bool
batch_holds(TessBatch *batch, int count, const int *values)
{
	TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	int			row = -1;
	int			seen = 0;

	batch->ops->get_datum_column(batch, 0, &batch->rows,
								 TESS_COLUMN_FOR_FILTER, &column);
	while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
	{
		if (seen >= count || DatumGetInt32(column.values[row]) != values[seen])
			return false;
		seen++;
	}
	return seen == count;
}

static bool
mask_equals(const Bitmapset *mask, int count, const int *members)
{
	if (bms_num_members(mask) != count)
		return false;
	for (int i = 0; i < count; i++)
		if (!bms_is_member(members[i], mask))
			return false;
	return true;
}

Datum
tessera_test_unary_batches(PG_FUNCTION_ARGS)
{
	const TessBindingOps *ops = tess_runtime_api()->binding_ops;
	FakeChild  *child = make_child(true);
	CustomScanState *node = make_node(make_desc());
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);
	TessUnaryConfig config;
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
	const TessRequest *child_request;
	const TessUnaryStats *stats;
	TessUnary  *unary;
	TupleTableSlot *slot;
	TessBatch  *batch;
	int			calls = 0;
	int			two = 2;
	int			four = 4;
	int			both[2] = {0, 1};
	int			first[1] = {0};
	bool		result;

	layout.ncolumns = 2;
	layout.ntargets = 2;
	config = make_config(node, child, &layout);
	config.filter_columns = bms_make_singleton(0);
	config.max_rows = 3;
	config.process = drop_odd;
	config.private_data = &calls;
	unary = tess_unary_create(&config);
	result = tess_unary_child(unary) == &child->css.ss.ps &&
		tess_unary_request(unary) == NULL &&
		tess_unary_child_request(unary) == NULL &&
		ops->find(node->ss.ps.ps_ResultTupleSlot) != NULL;

	/* The parent asks for batches of two rows and columns of its own. */
	request.output_mode = TESS_OUTPUT_BATCH;
	request.max_batch_rows = 2;
	request.filter_columns = bms_make_singleton(1);
	request.projection_columns = bms_make_singleton(0);
	ops->set_request(ops->find(node->ss.ps.ps_ResultTupleSlot), &request);

	/* The first batch: rows 1 and 2, of which 2 remains. */
	slot = tess_unary_exec(unary);
	child_request = tess_unary_child_request(unary);
	result &= slot == child->css.ss.ps.ps_ResultTupleSlot &&
		child_request != NULL &&
		child_request->output_mode == TESS_OUTPUT_BATCH &&
		child_request->max_batch_rows == 2 &&
		mask_equals(child_request->filter_columns, 2, both) &&
		mask_equals(child_request->projection_columns, 1, first) &&
		tess_unary_request(unary)->max_batch_rows == 2 && calls == 1;
	batch = ops->get_batch(ops->find(slot));
	result &= batch != NULL && batch_holds(batch, 1, &two) &&
		node->ss.ps.instrument->tuplecount == 0 &&
		node->ss.ps.instrument->nfiltered1 == 1;
	ops->mark_consumed(ops->find(slot));

	/* The second batch: rows 3 and 4, of which 4 remains. */
	slot = tess_unary_exec(unary);
	batch = slot == NULL ? NULL : ops->get_batch(ops->find(slot));
	result &= batch != NULL && batch_holds(batch, 1, &four) &&
		node->ss.ps.instrument->nfiltered1 == 2 && calls == 2;
	ops->mark_consumed(ops->find(slot));

	/* The third batch loses its only row and is skipped; then the end. */
	result &= tess_unary_exec(unary) == NULL && calls == 3 &&
		tess_unary_exec(unary) == NULL && calls == 3;
	stats = tess_unary_stats(unary);
	result &= stats->input_batches == 3 && stats->input_rows == 5 &&
		stats->output_rows == 2;

	/* After a rescan the batches flow again until the node stops. */
	tess_unary_rescan(unary);
	result &= stats->input_batches == 0;
	slot = tess_unary_exec(unary);
	batch = slot == NULL ? NULL : ops->get_batch(ops->find(slot));
	result &= batch != NULL && batch_holds(batch, 1, &two);
	tess_unary_stop(unary);
	ops->mark_consumed(ops->find(slot));
	result &= tess_unary_exec(unary) == NULL && calls == 4 &&
		stats->input_batches == 1 && stats->output_rows == 1;
	tess_unary_end(unary);
	result &= ops->find(node->ss.ps.ps_ResultTupleSlot) == NULL;
	tess_output_end(child->output);
	PG_RETURN_BOOL(result);
}

/* True when the node's slot shows the given row: c, then a. */
static bool
slot_shows(TupleTableSlot *slot, int a)
{
	char		expected[16];

	snprintf(expected, sizeof(expected), "row%d", a);
	if (slot == NULL || TupIsNull(slot) || slot->tts_isnull[1] ||
		DatumGetInt32(slot->tts_values[1]) != a)
		return false;
	if (a % 3 == 0)
		return slot->tts_isnull[0];
	return !slot->tts_isnull[0] &&
		strcmp(text_to_cstring(DatumGetTextPP(slot->tts_values[0])),
			   expected) == 0;
}

Datum
tessera_test_unary_rows(PG_FUNCTION_ARGS)
{
	FakeChild  *child = make_child(true);
	TupleDesc	desc = CreateTemplateTupleDesc(2);
	CustomScanState *node;
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);
	TessUnaryConfig config;
	const TessRequest *child_request;
	const TessUnaryStats *stats;
	TessUnary  *unary;
	TupleTableSlot *slot;
	int			map[2] = {1, 0};
	int			both[2] = {0, 1};
	bool		result;

	/* The node shows the child's columns in the other order. */
	TupleDescInitEntry(desc, 1, "c", TEXTOID, -1, 0);
	TupleDescInitEntry(desc, 2, "a", INT4OID, -1, 0);
	node = make_node(desc);
	layout.ncolumns = 2;
	layout.ntargets = 2;
	layout.target_columns = map;
	config = make_config(node, child, &layout);
	config.max_rows = 2;
	unary = tess_unary_create(&config);

	/* Without a request from the parent, rows are served two per batch. */
	slot = tess_unary_exec(unary);
	child_request = tess_unary_child_request(unary);
	stats = tess_unary_stats(unary);
	result = slot == node->ss.ps.ps_ResultTupleSlot && slot_shows(slot, 1) &&
		child_request->output_mode == TESS_OUTPUT_BATCH &&
		child_request->max_batch_rows == 2 &&
		bms_is_empty(child_request->filter_columns) &&
		mask_equals(child_request->projection_columns, 2, both) &&
		tess_unary_request(unary)->output_mode == TESS_OUTPUT_ROWS &&
		node->ss.ps.instrument->tuplecount == 0 && stats->input_batches == 1;
	result &= slot_shows(tess_unary_exec(unary), 2) && stats->input_batches == 1;
	result &= slot_shows(tess_unary_exec(unary), 3) && stats->input_batches == 2;
	result &= slot_shows(tess_unary_exec(unary), 4);
	result &= slot_shows(tess_unary_exec(unary), 5) && stats->input_batches == 3;
	result &= tess_unary_exec(unary) == NULL && tess_unary_exec(unary) == NULL &&
		stats->input_rows == 5 && stats->output_rows == 5;

	/* A rescan in the middle of a batch starts over. */
	tess_unary_rescan(unary);
	result &= slot_shows(tess_unary_exec(unary), 1);
	tess_unary_rescan(unary);
	result &= slot_shows(tess_unary_exec(unary), 1) &&
		slot_shows(tess_unary_exec(unary), 2) &&
		slot_shows(tess_unary_exec(unary), 3);
	tess_unary_end(unary);
	tess_output_end(child->output);
	PG_RETURN_BOOL(result);
}

static int
return_wrong_count(void *private_data, TessBatch *batch, int rows)
{
	return rows + 1;
}

Datum
tessera_test_unary_errors(PG_FUNCTION_ARGS)
{
	const TessBindingOps *ops = tess_runtime_api()->binding_ops;
	int32		kind = PG_GETARG_INT32(0);
	FakeChild  *child = make_child(kind != 3);
	CustomScanState *node = make_node(make_desc());
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);
	TessUnaryConfig config;
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
	TessUnary  *unary;
	int			unmapped[2] = {-1, 0};

	layout.ncolumns = 2;
	layout.ntargets = 2;
	config = make_config(node, child, &layout);
	request.output_mode = TESS_OUTPUT_BATCH;
	switch (kind)
	{
		case 7:
			/* The output helper checks every attribute's column. */
			layout.target_columns = unmapped;
			tess_unary_create(&config);
			break;
		case 0:
			config.struct_size = 1;
			tess_unary_create(&config);
			break;
		case 1:
			config.node = NULL;
			tess_unary_create(&config);
			break;
		case 2:
			config.child = NULL;
			tess_unary_create(&config);
			break;
		case 3:
			tess_unary_create(&config);
			break;
		case 4:
			config.max_rows = -1;
			tess_unary_create(&config);
			break;
		case 5:
			layout.ncolumns = 3;
			unary = tess_unary_create(&config);
			ops->set_request(ops->find(node->ss.ps.ps_ResultTupleSlot), &request);
			tess_unary_exec(unary);
			break;
		case 6:
			config.process = return_wrong_count;
			unary = tess_unary_create(&config);
			ops->set_request(ops->find(node->ss.ps.ps_ResultTupleSlot), &request);
			tess_unary_exec(unary);
			break;
		default:
			elog(ERROR, "unknown error case %d", kind);
	}
	elog(ERROR, "Tessera test expected a unary error");
	PG_RETURN_VOID();
}

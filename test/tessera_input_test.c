#include "postgres.h"

#include <string.h>

#include "catalog/pg_type_d.h"
#include "executor/executor.h"
#include "fmgr.h"
#include "utils/builtins.h"
#include "utils/fmgrprotos.h"
#include "utils/memutils.h"

#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_input_batches);
PG_FUNCTION_INFO_V1(tessera_test_input_forwarding);
PG_FUNCTION_INFO_V1(tessera_test_input_errors);
PG_FUNCTION_INFO_V1(tessera_test_input_gather);

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

/*
 * A child for gathering: batches of up to 64 physical rows of (a int4, c
 * text, n numeric), a numbered from 1 over every physical row, c its
 * text, n a / 4; batch b selects counts[b] rows spread over it, and its
 * columns are stored values unless stored[b] is false.
 */
typedef struct GatherChild
{
	PlanState	ps;
	TessOutput *output;
	TessBuilder *builder;
	TupleTableSlot *input_slot;
	const int  *counts;
	const bool *stored;
	int			nbatches;
	int			next_batch;
	int			next_value;
	int			phys;
	/* The a of every selected row, in order, as the consumer must see them. */
	int		   *expected;
	int			nexpected;
} GatherChild;

static TupleDesc
gather_desc(void)
{
	TupleDesc	desc = CreateTemplateTupleDesc(3);

	TupleDescInitEntry(desc, 1, "a", INT4OID, -1, 0);
	TupleDescInitEntry(desc, 2, "c", TEXTOID, -1, 0);
	TupleDescInitEntry(desc, 3, "n", NUMERICOID, -1, 0);
	return desc;
}

static TupleTableSlot *
gather_exec(PlanState *ps)
{
	GatherChild *child = (GatherChild *) ps;
	TessBatch  *batch;
	int			count;
	int			phys = child->phys;
	const TessRequest *request = tess_output_request(child->output);

	if (child->next_batch >= child->nbatches)
		return NULL;
	if (request->max_batch_rows > 0 && request->max_batch_rows < phys)
		phys = request->max_batch_rows;
	count = child->counts[child->next_batch];
	tess_output_release(child->output);
	tess_builder_reset(child->builder);
	for (int row = 0; row < phys; row++)
	{
		char		text[16];
		int			value = child->next_value + row;

		ExecClearTuple(child->input_slot);
		snprintf(text, sizeof(text), "row%d", value);
		child->input_slot->tts_values[0] = Int32GetDatum(value);
		child->input_slot->tts_isnull[0] = false;
		child->input_slot->tts_values[1] = PointerGetDatum(cstring_to_text(text));
		child->input_slot->tts_isnull[1] = false;
		snprintf(text, sizeof(text), "%d.%02d", value / 4, value % 4 * 25);
		child->input_slot->tts_values[2] =
			DirectFunctionCall3(numeric_in, CStringGetDatum(text),
								ObjectIdGetDatum(InvalidOid), Int32GetDatum(-1));
		child->input_slot->tts_isnull[2] = false;
		ExecStoreVirtualTuple(child->input_slot);
		tess_builder_append_slot(child->builder, child->input_slot);
	}
	batch = tess_builder_finish(child->builder, (Oid) (100 + child->next_batch % 2));
	/* Select count rows spread over the batch. */
	batch->rows.bits[0] = 0;
	for (int index = 0; index < count; index++)
	{
		int			row = index * phys / count;

		batch->rows.bits[0] |= UINT64CONST(1) << row;
		child->expected[child->nexpected++] = child->next_value + row;
	}
	batch->values_stored = child->stored == NULL || child->stored[child->next_batch];
	child->next_value += phys;
	child->next_batch++;
	Assert(count <= phys);
	return tess_output_publish(child->output, batch);
}

static GatherChild *
make_gather_child(const int *counts, const bool *stored, int nbatches)
{
	GatherChild *child = palloc0(sizeof(GatherChild));
	TupleDesc	desc = gather_desc();
	TessBuilderConfig config = TESS_STRUCT_INITIALIZER(TessBuilderConfig);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);

	child->ps.ExecProcNode = gather_exec;
	child->ps.ps_ResultTupleSlot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	child->input_slot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	child->counts = counts;
	child->stored = stored;
	child->nbatches = nbatches;
	child->next_value = 1;
	child->phys = 64;
	child->expected = palloc(sizeof(int) * 64 * Max(nbatches, 1));
	config.parent_context = CurrentMemoryContext;
	config.tuple_desc = desc;
	config.ncolumns = 3;
	config.capacity = 64;
	child->builder = tess_builder_create(&config);
	layout.ncolumns = 3;
	layout.ntargets = 3;
	child->output = tess_output_create(CurrentMemoryContext, &child->ps,
									   child->ps.ps_ResultTupleSlot, &layout);
	return child;
}

/*
 * Read every batch of the input: their sizes into sizes, and whether the
 * values are the child's selected rows in order, the text of each a and,
 * where the kernels give decimals, n as a * 25 at scale 2.
 */
/* The batches read whose numeric column came as decimals. */
static int	decimal_batches;

static bool
gather_read(TessInput *input, GatherChild *child, int *sizes, int *nsizes,
			bool *stored_out)
{
	int			seen = 0;
	TessBatch  *batch;
	bool		ok = true;

	*nsizes = 0;
	while ((batch = tess_input_next(input)) != NULL)
	{
		TessDatumColumn a = TESS_STRUCT_INITIALIZER(TessDatumColumn);
		TessDatumColumn c = TESS_STRUCT_INITIALIZER(TessDatumColumn);
		TessDatumColumn n = TESS_STRUCT_INITIALIZER(TessDatumColumn);
		int			row = -1;
		const uint64 *decimals;

		if (stored_out != NULL)
			stored_out[*nsizes] = tess_batch_values_stored(batch);
		sizes[(*nsizes)++] = tess_row_mask_count(&batch->rows);
		batch->ops->get_datum_column(batch, 0, &batch->rows, TESS_COLUMN_FOR_FILTER, &a);
		batch->ops->get_datum_column(batch, 1, &batch->rows, TESS_COLUMN_FOR_PROJECTION, &c);
		n.accept_decimals = true;
		batch->ops->get_datum_column(batch, 2, &batch->rows, TESS_COLUMN_FOR_PROJECTION, &n);
		decimals = tess_column_decimal_rows(&n);
		decimal_batches += decimals != NULL;
		while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
		{
			int			value = DatumGetInt32(a.values[row]);
			char		label[16];

			snprintf(label, sizeof(label), "row%d", value);
			if (seen >= child->nexpected || value != child->expected[seen] ||
				strcmp(text_to_cstring(DatumGetTextPP(c.values[row])), label) != 0)
				ok = false;
			if (decimals != NULL &&
				((decimals[row / 64] >> (row % 64) & 1) == 0 || n.decimal_scale != 2 ||
				 (int64) n.values[row] != (int64) value * 25))
				ok = false;
			seen++;
		}
		tess_input_finish(input);
	}
	return ok && seen == child->nexpected;
}

static bool
sizes_are(const int *sizes, int nsizes, const int *expected, int nexpected)
{
	if (nsizes != nexpected)
		return false;
	for (int index = 0; index < nsizes; index++)
		if (sizes[index] != expected[index])
			return false;
	return true;
}

static TessInput *
gather_input(GatherChild *child, int max_batch_rows)
{
	TessInput  *input = tess_input_create(CurrentMemoryContext, &child->ps);
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);

	request.output_mode = TESS_OUTPUT_BATCH;
	request.max_batch_rows = max_batch_rows;
	request.filter_columns = bms_make_singleton(0);
	request.projection_columns = bms_add_member(bms_make_singleton(1), 2);
	tess_input_set_request(input, &request);
	tess_input_set_gather(input, true);
	return input;
}

/*
 * One case of the rules: the child's counts a batch (and stored flags),
 * the request's rows a batch, and the sizes the consumer must receive.
 */
static bool
gather_case(const int *counts, const bool *stored, int nbatches, int max_batch_rows,
			const int *expected, int nexpected, uint64 copied)
{
	GatherChild *child = make_gather_child(counts, stored, nbatches);
	TessInput  *input = gather_input(child, max_batch_rows);
	int			sizes[128];
	int			nsizes;
	bool		ok;

	ok = gather_read(input, child, sizes, &nsizes, NULL) &&
		sizes_are(sizes, nsizes, expected, nexpected) &&
		tess_input_gathered_rows(input) == copied &&
		tess_input_slot(input) == NULL;
	tess_output_end(child->output);
	if (!ok)
	{
		StringInfoData got;

		initStringInfo(&got);
		for (int index = 0; index < nsizes; index++)
			appendStringInfo(&got, " %d", sizes[index]);
		elog(NOTICE, "gathered batches:%s, copied " UINT64_FORMAT, got.data,
			 tess_input_gathered_rows(input));
	}
	return ok;
}

Datum
tessera_test_input_gather(PG_FUNCTION_ARGS)
{
	int32		kind = PG_GETARG_INT32(0);

	switch (kind)
	{
		case 0:
			{
				/* Sparse, dense and mixed, a split, the end. */
				static const int counts[] = {2, 1, 3, 20, 30, 25, 40, 1};
				static const int expected[] = {2, 64, 15, 40, 1};

				/* The gathered batches, three, answer decimals. */
				decimal_batches = 0;
				PG_RETURN_BOOL(gather_case(counts, NULL, 8, 0, expected, 5, 80) &&
							   decimal_batches == 3);
			}
		case 1:
			{
				/* Batches of 32 rows go as they are. */
				static const int counts[] = {32, 32, 32, 32};
				static const int expected[] = {32, 32, 32, 32};

				PG_RETURN_BOOL(gather_case(counts, NULL, 4, 0, expected, 4, 0));
			}
		case 2:
			{
				/* 31 and 33 in turn: each 31 copied, then flushed before 33. */
				static const int counts[] = {33, 31, 33, 31, 33, 31};
				static const int expected[] = {33, 31, 33, 31, 33, 31};

				PG_RETURN_BOOL(gather_case(counts, NULL, 6, 0, expected, 6, 93));
			}
		case 3:
			{
				/* A batch without stored values goes as it is, after the rows gathered. */
				static const int counts[] = {1, 5, 5, 3, 5, 5};
				static const bool stored[] = {true, true, true, false, true, true};
				static const int expected[] = {1, 10, 3, 10};

				PG_RETURN_BOOL(gather_case(counts, stored, 6, 0, expected, 4, 20));
			}
		case 4:
			{
				/* A request's 16 rows a batch: half is 8, and a split. */
				static const int counts[] = {3, 5, 7, 7, 2, 4, 9};
				static const int expected[] = {3, 16, 9, 9};

				PG_RETURN_BOOL(gather_case(counts, NULL, 7, 16, expected, 4, 25));
			}
		case 5:
			{
				/* A split rest fills the next batch. */
				static const int counts[] = {4, 30, 30, 30, 10};
				static const int expected[] = {4, 64, 36};

				PG_RETURN_BOOL(gather_case(counts, NULL, 5, 0, expected, 3, 100));
			}
		case 6:
			{
				/*
				 * Gathering turned off during a scan goes on to the scan's
				 * end and stops at the next; a rescan forgets a waiting rest
				 * and starts with a first batch as it is.
				 */
				static const int counts[] = {2, 20, 30, 30, 2};
				static const int gathered[] = {2, 64, 18};
				static const int plain[] = {2, 20, 30, 30, 2};
				GatherChild *child = make_gather_child(counts, NULL, 5);
				TessInput  *input = gather_input(child, 0);
				int			sizes[16];
				int			nsizes = 0;
				TessBatch  *batch;
				bool		ok;

				while ((batch = tess_input_next(input)) != NULL)
				{
					sizes[nsizes++] = tess_row_mask_count(&batch->rows);
					tess_input_finish(input);
					if (nsizes == 2)
						tess_input_set_gather(input, false);
				}
				ok = sizes_are(sizes, nsizes, gathered, 3);
				for (int scan = 0; scan < 3; scan++)
				{
					tess_output_clear(child->output);
					child->next_batch = 0;
					child->next_value = 1;
					child->nexpected = 0;
					tess_input_rescan(input);
					if (scan == 1)
					{
						tess_input_set_gather(input, true);
						/* A rest waiting at the rescan: read up to the split. */
						for (int index = 0; index < 2; index++)
						{
							batch = tess_input_next(input);
							tess_input_finish(input);
						}
						continue;
					}
					ok = ok && gather_read(input, child, sizes, &nsizes, NULL) &&
						(scan == 0 ? sizes_are(sizes, nsizes, plain, 5) :
						 sizes_are(sizes, nsizes, gathered, 3));
				}
				/* Over the input's life, rescans included: 82 + 64 + 82. */
				ok = ok && tess_input_gathered_rows(input) == 228;
				tess_output_end(child->output);
				PG_RETURN_BOOL(ok);
			}
		case 7:
			{
				/* A column outside the request is an error of the gathered batch. */
				static const int counts[] = {2, 2, 2};
				GatherChild *child = make_gather_child(counts, NULL, 3);
				TessInput  *input = tess_input_create(CurrentMemoryContext, &child->ps);
				TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
				TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
				TessBatch  *batch;

				request.output_mode = TESS_OUTPUT_BATCH;
				request.filter_columns = bms_make_singleton(0);
				tess_input_set_request(input, &request);
				tess_input_set_gather(input, true);
				batch = tess_input_next(input);
				tess_input_finish(input);
				batch = tess_input_next(input);
				batch->ops->get_datum_column(batch, 1, &batch->rows,
											 TESS_COLUMN_FOR_PROJECTION, &column);
				break;
			}
		case 8:
			{
				/* Gathering before a request is an error. */
				static const int counts[] = {2};
				GatherChild *child = make_gather_child(counts, NULL, 1);
				TessInput  *input = tess_input_create(CurrentMemoryContext, &child->ps);

				tess_input_set_gather(input, true);
				break;
			}
	}
	elog(ERROR, "Tessera test expected an input error");
	PG_RETURN_BOOL(false);
}

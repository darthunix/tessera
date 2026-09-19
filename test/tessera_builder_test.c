#include "postgres.h"

#include <string.h>

#include "catalog/pg_type_d.h"
#include "executor/tuptable.h"
#include "fmgr.h"
#include "utils/builtins.h"
#include "utils/memutils.h"

#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_builder_byval);
PG_FUNCTION_INFO_V1(tessera_test_builder_columns);
PG_FUNCTION_INFO_V1(tessera_test_builder_errors);

/* A descriptor of (int4, int4, text). */
static TupleDesc
make_desc(void)
{
	TupleDesc	desc = CreateTemplateTupleDesc(3);

	TupleDescInitEntry(desc, 1, "a", INT4OID, -1, 0);
	TupleDescInitEntry(desc, 2, "b", INT4OID, -1, 0);
	TupleDescInitEntry(desc, 3, "c", TEXTOID, -1, 0);
	return desc;
}

static TessBuilder *
make_builder(TupleDesc desc, int ncolumns, int capacity)
{
	TessBuilderConfig config = TESS_STRUCT_INITIALIZER(TessBuilderConfig);

	config.parent_context = CurrentMemoryContext;
	config.tuple_desc = desc;
	config.ncolumns = ncolumns;
	config.capacity = capacity;
	return tess_builder_create(&config);
}

static void
store(TupleTableSlot *slot, int32 a, bool a_null, int32 b, const char *c)
{
	ExecClearTuple(slot);
	slot->tts_values[0] = Int32GetDatum(a);
	slot->tts_isnull[0] = a_null;
	slot->tts_values[1] = Int32GetDatum(b);
	slot->tts_isnull[1] = false;
	slot->tts_values[2] = c == NULL ? (Datum) 0 : PointerGetDatum(cstring_to_text(c));
	slot->tts_isnull[2] = c == NULL;
	ExecStoreVirtualTuple(slot);
}

static bool
get_column(TessBatch *batch, int column, TessDatumColumn *result)
{
	*result = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
	batch->ops->get_datum_column(batch, column, &batch->rows,
								 TESS_COLUMN_FOR_FILTER, result);
	return result->struct_size == sizeof(TessDatumColumn) &&
		result->nrows == batch->rows.nrows &&
		result->values != NULL && result->isnull != NULL;
}

Datum
tessera_test_builder_byval(PG_FUNCTION_ARGS)
{
	TupleDesc	desc = make_desc();
	TupleTableSlot *slot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	TessBuilder *builder = make_builder(desc, 1, 70);
	TessBatch  *batch;
	TessDatumColumn column;
	bool		result = true;
	int			row;

	if (tess_builder_is_full(builder) || tess_builder_finish(builder, InvalidOid) != NULL)
		result = false;
	tess_builder_reset(builder);
	for (row = 0; row < 70; row++)
	{
		if (tess_builder_is_full(builder))
			result = false;
		store(slot, row * 3, row % 3 == 0, 0, NULL);
		tess_builder_append_slot(builder, slot);
	}
	result = result && tess_builder_is_full(builder);
	batch = tess_builder_finish(builder, 42);
	result = result && batch != NULL && batch->rows.nrows == 70 &&
		batch->rows.bits[0] == UINT64_MAX &&
		batch->rows.bits[1] == (UINT64CONST(1) << 6) - 1 &&
		batch->table_oid == 42 && batch->abi_version == TESS_BATCH_ABI_VERSION &&
		batch->struct_size >= TESS_BATCH_MIN_SIZE &&
		tess_builder_finish(builder, 42) == batch;
	result = result && get_column(batch, 0, &column);
	for (row = 0; result && row < 70; row++)
	{
		if (column.isnull[row] != (row % 3 == 0))
			result = false;
		else if (column.isnull[row] ? column.values[row] != (Datum) 0 :
				 DatumGetInt32(column.values[row]) != row * 3)
			result = false;
	}
	ExecDropSingleTupleTableSlot(slot);
	PG_RETURN_BOOL(result);
}

Datum
tessera_test_builder_columns(PG_FUNCTION_ARGS)
{
	TupleDesc	desc = make_desc();
	TupleTableSlot *slot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	TessBuilder *builder = make_builder(desc, 3, 3);
	TessBatch  *batch;
	TessDatumColumn a;
	TessDatumColumn b;
	TessDatumColumn c;
	bool		result = true;
	int			round;

	for (round = 0; round < 2; round++)
	{
		tess_builder_reset(builder);
		store(slot, 1 + round, false, 10, "first");
		tess_builder_append_slot(builder, slot);
		/* The slot is reused before the batch is read: the copy must hold. */
		store(slot, 2 + round, true, 20, NULL);
		tess_builder_append_slot(builder, slot);
		store(slot, 3 + round, false, 30, "third");
		tess_builder_append_slot(builder, slot);
		store(slot, 99, false, 99, "overwritten");
		batch = tess_builder_finish(builder, InvalidOid);
		result = result && batch != NULL && batch->rows.nrows == 3 &&
			batch->rows.bits[0] == 7 && tess_builder_is_full(builder) &&
			get_column(batch, 0, &a) && get_column(batch, 1, &b) &&
			get_column(batch, 2, &c);
		result = result && !a.isnull[0] && a.isnull[1] && !a.isnull[2] &&
			DatumGetInt32(a.values[0]) == 1 + round &&
			a.values[1] == (Datum) 0 &&
			DatumGetInt32(a.values[2]) == 3 + round &&
			DatumGetInt32(b.values[0]) == 10 && DatumGetInt32(b.values[1]) == 20 &&
			DatumGetInt32(b.values[2]) == 30 &&
			!c.isnull[0] && c.isnull[1] && !c.isnull[2] &&
			strcmp(text_to_cstring(DatumGetTextPP(c.values[0])), "first") == 0 &&
			strcmp(text_to_cstring(DatumGetTextPP(c.values[2])), "third") == 0;
	}
	tess_builder_reset(builder);
	result = result && !tess_builder_is_full(builder) &&
		tess_builder_finish(builder, InvalidOid) == NULL;
	ExecDropSingleTupleTableSlot(slot);
	PG_RETURN_BOOL(result);
}

Datum
tessera_test_builder_errors(PG_FUNCTION_ARGS)
{
	int32		kind = PG_GETARG_INT32(0);
	TupleDesc	desc = make_desc();
	TupleTableSlot *slot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	TessBuilderConfig config = TESS_STRUCT_INITIALIZER(TessBuilderConfig);
	TessBuilder *builder;
	TessBatch  *batch;
	TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);

	config.parent_context = CurrentMemoryContext;
	config.tuple_desc = desc;
	config.ncolumns = 2;
	config.capacity = 1;
	if (kind == 0)
		config.parent_context = NULL;
	else if (kind == 1)
		config.ncolumns = 0;
	else if (kind == 2)
		config.ncolumns = 4;
	else if (kind == 3)
		config.capacity = 0;
	builder = tess_builder_create(&config);
	store(slot, 1, false, 2, "x");
	tess_builder_append_slot(builder, slot);
	if (kind == 4)
	{
		tess_builder_finish(builder, InvalidOid);
		tess_builder_append_slot(builder, slot);
	}
	else if (kind == 5)
		tess_builder_append_slot(builder, slot);
	batch = tess_builder_finish(builder, InvalidOid);
	if (kind == 6)
		batch->ops->get_datum_column(batch, 2, &batch->rows,
									 TESS_COLUMN_FOR_FILTER, &column);
	else if (kind == 7)
	{
		column.struct_size = sizeof(Size);
		batch->ops->get_datum_column(batch, 0, &batch->rows,
									 TESS_COLUMN_FOR_FILTER, &column);
	}
	else
	{
		TupleDesc	narrow = CreateTemplateTupleDesc(1);
		TupleTableSlot *narrow_slot;

		TupleDescInitEntry(narrow, 1, "a", INT4OID, -1, 0);
		narrow_slot = MakeSingleTupleTableSlot(narrow, &TTSOpsVirtual);
		narrow_slot->tts_values[0] = Int32GetDatum(1);
		narrow_slot->tts_isnull[0] = false;
		ExecStoreVirtualTuple(narrow_slot);
		tess_builder_reset(builder);
		tess_builder_append_slot(builder, narrow_slot);
	}
	elog(ERROR, "Tessera test expected a builder error");
	PG_RETURN_VOID();
}

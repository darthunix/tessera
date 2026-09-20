#include "postgres.h"

#include "utils/datum.h"
#include "utils/memutils.h"

#include "tessera/runtime.h"

struct TessBuilder
{
	TessBatch	batch;
	TupleDesc	tuple_desc;
	/* Copies of pass-by-reference values; reset per batch. */
	MemoryContext payload_context;
	/* Column-major: column * capacity + row. */
	Datum	   *values;
	bool	   *isnull;
	uint64	   *selection;
	int			ncolumns;
	int			capacity;
	int			nrows;
	bool		has_byref;
	bool		sealed;
};

static void
builder_get_datum_column(TessBatch *batch, int column,
						 const TessRowMask *rows,
						 TessColumnPurpose purpose,
						 TessDatumColumn *result)
{
	TessBuilder *builder = batch->private_data;

	if (result == NULL || result->struct_size < TESS_DATUM_COLUMN_MIN_SIZE)
		elog(ERROR, "Tessera builder received an incompatible column request");
	if (column < 0 || column >= builder->ncolumns)
		elog(ERROR, "Tessera builder column is out of range");
	if (rows != NULL && rows->nrows != batch->rows.nrows)
		elog(ERROR, "Tessera builder received a row mask of another batch");
	/* Every row is materialized; the purpose changes nothing. */
	result->values = &builder->values[column * builder->capacity];
	result->isnull = &builder->isnull[column * builder->capacity];
	result->nrows = batch->rows.nrows;
}

static const TessBatchOps builder_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = builder_get_datum_column,
};

TessBuilder *
tess_builder_create(const TessBuilderConfig *config)
{
	TessBuilder *builder;
	Size		nvalues;
	int			column;

	if (config == NULL ||
		config->struct_size < TESS_BUILDER_CONFIG_MIN_SIZE ||
		config->parent_context == NULL || config->tuple_desc == NULL)
		elog(ERROR, "Tessera builder requires a memory context and tuple descriptor");
	if (config->ncolumns < 0 || config->ncolumns > config->tuple_desc->natts)
		elog(ERROR, "Tessera builder column count is out of range");
	if (config->capacity <= 0)
		elog(ERROR, "Tessera builder capacity must be positive");

	nvalues = mul_size(config->ncolumns, config->capacity);
	builder = MemoryContextAllocZero(config->parent_context, sizeof(*builder));
	builder->tuple_desc = config->tuple_desc;
	builder->ncolumns = config->ncolumns;
	builder->capacity = config->capacity;
	builder->values = MemoryContextAlloc(config->parent_context,
										 mul_size(sizeof(Datum), nvalues));
	builder->isnull = MemoryContextAlloc(config->parent_context,
										 mul_size(sizeof(bool), nvalues));
	builder->selection = MemoryContextAlloc(config->parent_context,
											mul_size(sizeof(uint64),
													 tess_row_mask_word_count(config->capacity)));
	builder->payload_context =
		AllocSetContextCreate(config->parent_context, "Tessera builder values",
							  ALLOCSET_DEFAULT_SIZES);
	for (column = 0; column < config->ncolumns; column++)
	{
		if (!TupleDescAttr(config->tuple_desc, column)->attbyval)
		{
			builder->has_byref = true;
			break;
		}
	}
	builder->batch.abi_version = TESS_BATCH_ABI_VERSION;
	builder->batch.struct_size = sizeof(TessBatch);
	builder->batch.ops = &builder_ops;
	builder->batch.private_data = builder;
	tess_builder_reset(builder);
	return builder;
}

void
tess_builder_reset(TessBuilder *builder)
{
	MemoryContextReset(builder->payload_context);
	builder->nrows = 0;
	builder->sealed = false;
	builder->batch.rows.nrows = 0;
	builder->batch.rows.bits = NULL;
	builder->batch.table_oid = InvalidOid;
}

bool
tess_builder_is_full(const TessBuilder *builder)
{
	return builder->sealed || builder->nrows == builder->capacity;
}

void
tess_builder_append_slot(TessBuilder *builder, TupleTableSlot *slot)
{
	MemoryContext oldcontext;
	int			row = builder->nrows;
	int			column;

	if (unlikely(builder->sealed))
		elog(ERROR, "cannot append to a finished Tessera builder");
	if (unlikely(row >= builder->capacity))
		elog(ERROR, "Tessera builder is full");
	if (unlikely(slot->tts_tupleDescriptor->natts < builder->ncolumns))
		elog(ERROR, "Tessera builder input has too few columns");

	/* Materialize only the leading attributes, whatever the slot's kind. */
	slot_getsomeattrs(slot, builder->ncolumns);
	if (likely(builder->ncolumns == 1 && !builder->has_byref))
	{
		builder->isnull[row] = slot->tts_isnull[0];
		builder->values[row] = slot->tts_isnull[0] ?
			(Datum) 0 : slot->tts_values[0];
		builder->nrows = row + 1;
		return;
	}
	if (!builder->has_byref)
	{
		for (column = 0; column < builder->ncolumns; column++)
		{
			int			offset = column * builder->capacity + row;

			builder->isnull[offset] = slot->tts_isnull[column];
			builder->values[offset] = slot->tts_isnull[column] ?
				(Datum) 0 : slot->tts_values[column];
		}
		builder->nrows = row + 1;
		return;
	}
	oldcontext = MemoryContextSwitchTo(builder->payload_context);
	for (column = 0; column < builder->ncolumns; column++)
	{
		Form_pg_attribute attr = TupleDescAttr(builder->tuple_desc, column);
		int			offset = column * builder->capacity + row;

		builder->isnull[offset] = slot->tts_isnull[column];
		if (slot->tts_isnull[column])
			builder->values[offset] = (Datum) 0;
		else if (attr->attbyval)
			builder->values[offset] = slot->tts_values[column];
		else
			builder->values[offset] =
				datumCopy(slot->tts_values[column], false, attr->attlen);
	}
	MemoryContextSwitchTo(oldcontext);
	builder->nrows = row + 1;
}

TessBatch *
tess_builder_finish(TessBuilder *builder, Oid table_oid)
{
	int			nwords;
	int			word;

	if (builder->sealed)
		return builder->nrows == 0 ? NULL : &builder->batch;
	builder->sealed = true;
	if (builder->nrows == 0)
		return NULL;

	nwords = tess_row_mask_word_count(builder->nrows);
	for (word = 0; word < nwords; word++)
	{
		int			remaining = builder->nrows - word * 64;

		builder->selection[word] = remaining >= 64 ? UINT64_MAX :
			UINT64_MAX >> (64 - remaining);
	}
	builder->batch.rows.nrows = builder->nrows;
	builder->batch.rows.bits = builder->selection;
	builder->batch.table_oid = table_oid;
	return &builder->batch;
}

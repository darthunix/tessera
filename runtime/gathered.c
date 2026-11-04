#include "postgres.h"

#include "catalog/pg_type_d.h"
#include "utils/datum.h"
#include "utils/memutils.h"

#include "tessera/runtime.h"

#include "decimal_cache.h"
#include "gathered.h"

/*
 * The rows copied from child batches, column-major by the request's
 * columns: a Datum and a NULL flag a row, a value by reference copied
 * into values_context, which the next batch resets. Every row is
 * selected; capacity is at most 64, one word of the mask.
 */
struct TessGathered
{
	TessBatch	batch;
	MemoryContext values_context;
	int			capacity;
	int			nrows;
	uint64		bits;
	/* The requested columns: the child's column, its purpose and type. */
	int			ncolumns;
	int		   *columns;
	TessColumnPurpose *purposes;
	int16	   *typlens;
	bool	   *typbyvals;
	bool	   *numeric;
	Datum	  **values;
	bool	  **isnull;
	/* Per column of the child's layout, its index above, or -1. */
	int			layout_columns;
	int		   *index_of;
	/* The rows of a child batch to copy, as many words as it needs. */
	uint64	   *take_bits;
	int			take_words;
	TessDecimalCache decimals;
};

static void
gathered_get_column(TessBatch *batch, int column, const TessRowMask *rows,
					TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessGathered *gathered = (TessGathered *) batch->private_data;
	int			index = column >= 0 && column < gathered->layout_columns ?
		gathered->index_of[column] : -1;

	if (index < 0)
		elog(ERROR, "Tessera gathered batch has no column %d: its consumer did not request it",
			 column + 1);
	result->values = gathered->values[index];
	result->isnull = gathered->isnull[index];
	result->nrows = batch->rows.nrows;
	if (result->struct_size >= TESS_DATUM_COLUMN_DECIMALS_SIZE)
	{
		result->decimal_rows = NULL;
		if (result->accept_decimals && gathered->numeric[index])
			tess_decimal_cache_read(&gathered->decimals, index, gathered->values[index],
									gathered->isnull[index], rows, result);
	}
}

static const TessBatchOps gathered_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = gathered_get_column,
};

TessGathered *
tess_gathered_create(MemoryContext parent, const TessLayout *layout, TupleDesc desc,
					 const Bitmapset *filter_columns, const Bitmapset *projection_columns,
					 int capacity)
{
	TessGathered *gathered;
	Bitmapset  *columns = bms_union(filter_columns, projection_columns);
	int			column = -1;
	int			index = 0;

	if (capacity <= 0 || capacity > 64)
		elog(ERROR, "Tessera gathered batch of %d rows", capacity);
	gathered = MemoryContextAllocZero(parent, sizeof(*gathered));
	gathered->values_context = AllocSetContextCreate(parent, "Tessera gathered values",
													 ALLOCSET_DEFAULT_SIZES);
	gathered->capacity = capacity;
	gathered->ncolumns = bms_num_members(columns);
	gathered->columns = MemoryContextAlloc(parent, sizeof(int) * Max(gathered->ncolumns, 1));
	gathered->purposes = MemoryContextAlloc(parent,
											sizeof(TessColumnPurpose) * Max(gathered->ncolumns, 1));
	gathered->typlens = MemoryContextAllocZero(parent, sizeof(int16) * Max(gathered->ncolumns, 1));
	gathered->typbyvals = MemoryContextAllocZero(parent, sizeof(bool) * Max(gathered->ncolumns, 1));
	gathered->numeric = MemoryContextAllocZero(parent, sizeof(bool) * Max(gathered->ncolumns, 1));
	gathered->values = MemoryContextAlloc(parent, sizeof(Datum *) * Max(gathered->ncolumns, 1));
	gathered->isnull = MemoryContextAlloc(parent, sizeof(bool *) * Max(gathered->ncolumns, 1));
	gathered->layout_columns = layout->ncolumns;
	gathered->index_of = MemoryContextAlloc(parent, sizeof(int) * Max(layout->ncolumns, 1));
	for (int each = 0; each < layout->ncolumns; each++)
		gathered->index_of[each] = -1;
	while ((column = bms_next_member(columns, column)) >= 0)
	{
		bool		typed = false;

		if (column >= layout->ncolumns)
			elog(ERROR, "Tessera request names column %d of a layout of %d",
				 column + 1, layout->ncolumns);
		gathered->columns[index] = column;
		gathered->purposes[index] = bms_is_member(column, filter_columns) ?
			TESS_COLUMN_FOR_FILTER : TESS_COLUMN_FOR_PROJECTION;
		/* The column's type: that of a target of the child it maps to. */
		for (int target = 0; target < Min(layout->ntargets, desc->natts) && !typed; target++)
		{
			Form_pg_attribute attr = TupleDescAttr(desc, target);

			if (tess_layout_column(layout, target) != column)
				continue;
			gathered->typlens[index] = attr->attlen;
			gathered->typbyvals[index] = attr->attbyval;
			gathered->numeric[index] = attr->atttypid == NUMERICOID;
			typed = true;
		}
		if (!typed)
			elog(ERROR, "Tessera cannot gather column %d: no target of the child holds it",
				 column + 1);
		gathered->values[index] = MemoryContextAllocZero(parent, sizeof(Datum) * capacity);
		gathered->isnull[index] = MemoryContextAllocZero(parent, sizeof(bool) * capacity);
		gathered->index_of[column] = index;
		index++;
	}
	bms_free(columns);
	tess_decimal_cache_init(&gathered->decimals, parent, Max(gathered->ncolumns, 1), capacity);
	gathered->batch.abi_version = TESS_BATCH_ABI_VERSION;
	gathered->batch.struct_size = sizeof(TessBatch);
	gathered->batch.rows = (TessRowMask) {0, &gathered->bits};
	gathered->batch.table_oid = InvalidOid;
	gathered->batch.ops = &gathered_ops;
	gathered->batch.private_data = gathered;
	gathered->batch.values_stored = true;
	return gathered;
}

int
tess_gathered_rows(const TessGathered *gathered)
{
	return gathered->nrows;
}

int
tess_gathered_capacity(const TessGathered *gathered)
{
	return gathered->capacity;
}

int
tess_gathered_take(TessGathered *gathered, TessBatch *child)
{
	int			room = gathered->capacity - gathered->nrows;
	int			nwords = tess_row_mask_word_count(child->rows.nrows);
	TessRowMask take;
	int			count = 0;
	int			row = -1;
	bool		rest;
	MemoryContext old;

	Assert(tess_batch_values_stored(child) && room > 0);
	if (nwords > gathered->take_words)
	{
		MemoryContext context = GetMemoryChunkContext(gathered);

		gathered->take_bits = gathered->take_bits == NULL ?
			MemoryContextAlloc(context, sizeof(uint64) * nwords) :
			repalloc(gathered->take_bits, sizeof(uint64) * nwords);
		gathered->take_words = nwords;
	}
	take = (TessRowMask) {child->rows.nrows, gathered->take_bits};
	memset(take.bits, 0, sizeof(uint64) * nwords);
	while (count < room && (row = tess_row_mask_next(&child->rows, row)) >= 0)
	{
		take.bits[row / 64] |= UINT64CONST(1) << (row % 64);
		count++;
	}
	if (count == 0)
		return 0;
	/* The loop stops at the room, or at the mask's end with row -1. */
	rest = count == room && tess_row_mask_next(&child->rows, row) >= 0;
	old = MemoryContextSwitchTo(gathered->values_context);
	for (int index = 0; index < gathered->ncolumns; index++)
	{
		TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
		Datum	   *values = gathered->values[index] + gathered->nrows;
		bool	   *isnull = gathered->isnull[index] + gathered->nrows;
		int			from = -1;

		child->ops->get_datum_column(child, gathered->columns[index], &take,
									 gathered->purposes[index], &column);
		for (int at = 0; at < count; at++)
		{
			from = tess_row_mask_next(&take, from);
			isnull[at] = column.isnull[from];
			if (isnull[at])
				values[at] = (Datum) 0;
			else if (gathered->typbyvals[index])
				values[at] = column.values[from];
			else
				values[at] = datumCopy(column.values[from], false, gathered->typlens[index]);
		}
	}
	MemoryContextSwitchTo(old);
	/* The rows copied leave a child batch whose rest waits. */
	if (rest)
		for (int word = 0; word < nwords; word++)
			child->rows.bits[word] &= ~take.bits[word];
	if (gathered->nrows == 0)
		gathered->batch.table_oid = child->table_oid;
	else if (gathered->batch.table_oid != child->table_oid)
		gathered->batch.table_oid = InvalidOid;
	gathered->nrows += count;
	gathered->bits = gathered->nrows == 64 ? ~UINT64CONST(0) :
		(UINT64CONST(1) << gathered->nrows) - 1;
	gathered->batch.rows.nrows = gathered->nrows;
	return count;
}

TessBatch *
tess_gathered_batch(TessGathered *gathered)
{
	return &gathered->batch;
}

void
tess_gathered_reset(TessGathered *gathered)
{
	gathered->nrows = 0;
	gathered->bits = 0;
	gathered->batch.rows.nrows = 0;
	gathered->batch.table_oid = InvalidOid;
	MemoryContextReset(gathered->values_context);
	tess_decimal_cache_reset(&gathered->decimals);
}

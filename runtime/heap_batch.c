#include "postgres.h"

#include "access/htup_details.h"
#include "executor/tuptable.h"
#include "storage/bufmgr.h"
#include "utils/memutils.h"

#include "tessera/heap_deform.h"
#include "tessera/runtime.h"

struct TessHeapBatch
{
	TessBatch	batch;
	/* The slots' descriptor, taken from the first row. */
	TupleDesc	tuple_desc;
	int			first_non_guaranteed;
	/* Tuples copied from slots without a pinned page; reset per batch. */
	MemoryContext copies;
	/* One tuple header per row; t_data points into a page or a copy. */
	HeapTupleData *tuples;
	TessDeformCursor *cursors;
	/* The pages the rows point into, pinned once each. */
	Buffer	   *pins;
	int			npins;
	/* Column-major: column * capacity + row; deformed on request. */
	Datum	   *values;
	bool	   *isnull;
	/* Per column, the rows whose value was deformed: column * nwords. */
	uint64	   *deformed;
	uint64	   *selection;
	int			ncolumns;
	int			capacity;
	int			nwords;
	int			nrows;
	bool		sealed;
	TessHeapBatchStats stats;
};

/* Deform one value, resuming the row's cursor or restarting before it. */
static void
deform(TessHeapBatch *heap, int column, int row)
{
	AttrNumber	attnum = column + 1;
	TessDeformCursor *cursor = &heap->cursors[row];
	TessDeformCursor local;
	Datum		value = (Datum) 0;
	bool		isnull = false;
	Size		offset = (Size) column * heap->capacity + row;

	if (cursor->nvalid >= attnum)
	{
		tess_deform_cursor_init(&local);
		cursor = &local;
		heap->stats.restarted_datums++;
	}
	if (!tess_deform_advance(cursor, &heap->tuples[row], heap->tuple_desc,
							 heap->first_non_guaranteed, attnum,
							 &value, &isnull, false))
		value = getmissingattr(heap->tuple_desc, attnum, &isnull);
	heap->values[offset] = isnull ? (Datum) 0 : value;
	heap->isnull[offset] = isnull;
	heap->stats.deformed_datums++;
}

static void
heap_get_datum_column(TessBatch *batch, int column, const TessRowMask *rows,
					  TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessHeapBatch *heap = batch->private_data;
	uint64	   *deformed;

	if (result == NULL || result->struct_size < TESS_DATUM_COLUMN_MIN_SIZE)
		elog(ERROR, "Tessera heap batch received an incompatible column request");
	if (column < 0 || column >= heap->ncolumns)
		elog(ERROR, "Tessera heap batch column is out of range");
	if (rows == NULL || rows->nrows != batch->rows.nrows)
		elog(ERROR, "Tessera heap batch received a row mask of another batch");
	deformed = &heap->deformed[(Size) column * heap->nwords];
	for (int word = 0; word < tess_row_mask_word_count(rows->nrows); word++)
	{
		uint64		pending = rows->bits[word] & ~deformed[word];

		while (pending != 0)
		{
			deform(heap, column, word * 64 + pg_rightmost_one_pos64(pending));
			pending &= pending - 1;
		}
		deformed[word] |= rows->bits[word];
	}
	result->values = &heap->values[(Size) column * heap->capacity];
	result->isnull = &heap->isnull[(Size) column * heap->capacity];
	result->nrows = batch->rows.nrows;
}

static void
unpin(TessHeapBatch *heap)
{
	while (heap->npins > 0)
		ReleaseBuffer(heap->pins[--heap->npins]);
	MemoryContextReset(heap->copies);
}

static void
heap_release(TessBatch *batch)
{
	unpin(batch->private_data);
}

static const TessBatchOps heap_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = heap_get_datum_column,
	.release = heap_release,
};

TessHeapBatch *
tess_heap_batch_create(const TessHeapBatchConfig *config)
{
	TessHeapBatch *heap;
	MemoryContext context;
	Size		nvalues;

	if (config == NULL ||
		config->struct_size < TESS_HEAP_BATCH_CONFIG_MIN_SIZE ||
		config->parent_context == NULL)
		elog(ERROR, "Tessera heap batch requires a memory context");
	if (config->ncolumns < 0)
		elog(ERROR, "Tessera heap batch column count is out of range");
	if (config->capacity <= 0)
		elog(ERROR, "Tessera heap batch capacity must be positive");
	context = config->parent_context;
	nvalues = mul_size(config->ncolumns, config->capacity);
	heap = MemoryContextAllocZero(context, sizeof(*heap));
	heap->ncolumns = config->ncolumns;
	heap->capacity = config->capacity;
	heap->nwords = tess_row_mask_word_count(config->capacity);
	heap->tuples = MemoryContextAlloc(context,
									  mul_size(sizeof(HeapTupleData), config->capacity));
	heap->cursors = MemoryContextAlloc(context,
									   mul_size(sizeof(TessDeformCursor), config->capacity));
	heap->pins = MemoryContextAlloc(context,
									mul_size(sizeof(Buffer), config->capacity));
	heap->values = MemoryContextAllocZero(context, mul_size(sizeof(Datum), nvalues));
	heap->isnull = MemoryContextAllocZero(context, mul_size(sizeof(bool), nvalues));
	heap->deformed = MemoryContextAllocZero(context,
											mul_size(sizeof(uint64),
													 mul_size(config->ncolumns, heap->nwords)));
	heap->selection = MemoryContextAlloc(context, sizeof(uint64) * heap->nwords);
	heap->copies = AllocSetContextCreate(context, "Tessera heap batch copies",
										 ALLOCSET_DEFAULT_SIZES);
	if (TESS_ABI_HAS_FIELD(config, TessHeapBatchConfig, first_non_guaranteed_attr) &&
		config->tuple_desc != NULL)
	{
		if (config->tuple_desc->natts < config->ncolumns)
			elog(ERROR, "Tessera heap batch descriptor has too few columns");
		if (config->first_non_guaranteed_attr < 0 ||
			config->first_non_guaranteed_attr > config->tuple_desc->natts)
			elog(ERROR, "Tessera heap batch guaranteed prefix is out of range");
		heap->tuple_desc = config->tuple_desc;
		heap->first_non_guaranteed = config->first_non_guaranteed_attr;
	}
	heap->batch.abi_version = TESS_BATCH_ABI_VERSION;
	heap->batch.struct_size = sizeof(TessBatch);
	heap->batch.ops = &heap_ops;
	heap->batch.private_data = heap;
	tess_heap_batch_reset(heap);
	return heap;
}

/* Keep a row: the tuple header, and a pin on its page once per page. */
static void
keep_tuple(TessHeapBatch *heap, const HeapTupleData *tuple, Buffer buffer)
{
	int			row = heap->nrows;

	if (unlikely(heap->sealed))
		elog(ERROR, "cannot append to a finished Tessera heap batch");
	if (unlikely(row >= heap->capacity))
		elog(ERROR, "Tessera heap batch is full");
	if (BufferIsValid(buffer))
	{
		heap->tuples[row] = *tuple;
		if (heap->npins == 0 || heap->pins[heap->npins - 1] != buffer)
		{
			IncrBufferRefCount(buffer);
			heap->pins[heap->npins++] = buffer;
		}
	}
	else
	{
		MemoryContext oldcontext = MemoryContextSwitchTo(heap->copies);

		heap->tuples[row] = *heap_copytuple((HeapTuple) tuple);
		MemoryContextSwitchTo(oldcontext);
		heap->stats.copied_tuples++;
	}
	tess_deform_cursor_init(&heap->cursors[row]);
	heap->nrows = row + 1;
}

void
tess_heap_batch_reset(TessHeapBatch *heap)
{
	unpin(heap);
	memset(heap->deformed, 0, sizeof(uint64) * (Size) heap->ncolumns * heap->nwords);
	heap->nrows = 0;
	heap->sealed = false;
	heap->batch.rows.nrows = 0;
	heap->batch.rows.bits = NULL;
	heap->batch.table_oid = InvalidOid;
}

bool
tess_heap_batch_is_full(const TessHeapBatch *heap)
{
	return heap->sealed || heap->nrows == heap->capacity;
}

void
tess_heap_batch_append_slot(TessHeapBatch *heap, TupleTableSlot *slot)
{
	BufferHeapTupleTableSlot *bslot = (BufferHeapTupleTableSlot *) slot;

	if (heap->tuple_desc == NULL)
	{
		if (slot->tts_tupleDescriptor->natts < heap->ncolumns)
			elog(ERROR, "Tessera heap batch input has too few columns");
		heap->tuple_desc = slot->tts_tupleDescriptor;
		heap->first_non_guaranteed = slot->tts_first_nonguaranteed;
	}
	else if (slot->tts_tupleDescriptor != heap->tuple_desc)
		elog(ERROR, "Tessera heap batch input changed its descriptor");
	/* The slot's tuple header is overwritten by the next fetch. */
	if (TTS_IS_BUFFERTUPLE(slot) && BufferIsValid(bslot->buffer))
		keep_tuple(heap, bslot->base.tuple, bslot->buffer);
	else
	{
		int			row = heap->nrows;
		MemoryContext oldcontext;

		if (unlikely(heap->sealed))
			elog(ERROR, "cannot append to a finished Tessera heap batch");
		if (unlikely(row >= heap->capacity))
			elog(ERROR, "Tessera heap batch is full");
		oldcontext = MemoryContextSwitchTo(heap->copies);
		heap->tuples[row] = *ExecCopySlotHeapTuple(slot);
		MemoryContextSwitchTo(oldcontext);
		heap->stats.copied_tuples++;
		tess_deform_cursor_init(&heap->cursors[row]);
		heap->nrows = row + 1;
	}
}

void
tess_heap_batch_append_tuple(TessHeapBatch *heap, const HeapTupleData *tuple,
							 Buffer buffer)
{
	if (heap->tuple_desc == NULL)
		elog(ERROR, "Tessera heap batch needs a descriptor before tuples");
	keep_tuple(heap, tuple, buffer);
}

TessBatch *
tess_heap_batch_finish(TessHeapBatch *heap, Oid table_oid)
{
	int			nwords;

	if (heap->sealed)
		return heap->nrows == 0 ? NULL : &heap->batch;
	heap->sealed = true;
	if (heap->nrows == 0)
		return NULL;
	nwords = tess_row_mask_word_count(heap->nrows);
	for (int word = 0; word < nwords; word++)
	{
		int			remaining = heap->nrows - word * 64;

		heap->selection[word] = remaining >= 64 ? UINT64_MAX :
			UINT64_MAX >> (64 - remaining);
	}
	heap->batch.rows.nrows = heap->nrows;
	heap->batch.rows.bits = heap->selection;
	heap->batch.table_oid = table_oid;
	return &heap->batch;
}

const TessHeapBatchStats *
tess_heap_batch_stats(const TessHeapBatch *heap)
{
	return &heap->stats;
}

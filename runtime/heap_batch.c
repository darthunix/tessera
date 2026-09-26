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
	/*
	 * The slots' descriptor, taken from the first row; another one of the
	 * same layout already accepted (a Gather returns the leader's rows in
	 * its child's slot and the workers' in its own).
	 */
	TupleDesc	tuple_desc;
	TupleDesc	other_desc;
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
}

/*
 * A by-value column at a cached offset: in a tuple that has the attribute
 * and no NULL up to it, the value lies at that offset, and is read there
 * in a plain loop with the width fixed per call; the arrays are locals, so
 * the stores do not make the compiler reload the batch's fields. Such a
 * read leaves the row's cursor alone: a later column starts from the
 * cached offsets anyway. A tuple with a NULL among the attributes up to
 * this one, or too short for it, goes through the cursor.
 */
static pg_always_inline void
deform_cached_width(TessHeapBatch *heap, int column, const TessRowMask *rows,
					const uint64 *deformed, int nwords, int offset, int attlen)
{
	const HeapTupleData *tuples = heap->tuples;
	Datum	   *values = &heap->values[(Size) column * heap->capacity];
	bool	   *isnull = &heap->isnull[(Size) column * heap->capacity];
	int			upto = column + 1;

	for (int word = 0; word < nwords; word++)
	{
		uint64		pending = rows->bits[word] & ~deformed[word];

		while (pending != 0)
		{
			int			row = word * 64 + pg_rightmost_one_pos64(pending);
			HeapTupleHeader tup = tuples[row].t_data;
			bool		cached = HeapTupleHeaderGetNatts(tup) >= upto &&
				(!HeapTupleHasNulls(&tuples[row]) ||
				 first_null_attr(tup->t_bits, upto) == upto);

			if (likely(cached))
			{
				const char *tp = (const char *) tup + tup->t_hoff + offset;

				values[row] = fetch_att_noerr(tp, true, attlen);
				isnull[row] = false;
			}
			else
				deform(heap, column, row);
			pending &= pending - 1;
		}
	}
}

static void
deform_cached(TessHeapBatch *heap, int column, const TessRowMask *rows,
			  const uint64 *deformed, int nwords)
{
	const CompactAttribute *cattr = &heap->tuple_desc->compact_attrs[column];
	int			offset = cattr->attcacheoff;

	Assert(offset >= 0 && cattr->attbyval);
	switch (cattr->attlen)
	{
		case 1:
			deform_cached_width(heap, column, rows, deformed, nwords, offset, 1);
			break;
		case 2:
			deform_cached_width(heap, column, rows, deformed, nwords, offset, 2);
			break;
		case 4:
			deform_cached_width(heap, column, rows, deformed, nwords, offset, 4);
			break;
		case 8:
			deform_cached_width(heap, column, rows, deformed, nwords, offset, 8);
			break;
		default:
			elog(ERROR, "Tessera heap batch attribute %d is by value with length %d",
				 column + 1, cattr->attlen);
	}
}

static void
heap_get_datum_column(TessBatch *batch, int column, const TessRowMask *rows,
					  TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessHeapBatch *heap = batch->private_data;
	uint64	   *deformed;
	int			nwords;
	uint64		count = 0;

	if (result == NULL || result->struct_size < TESS_DATUM_COLUMN_MIN_SIZE)
		elog(ERROR, "Tessera heap batch received an incompatible column request");
	if (column < 0 || column >= heap->ncolumns)
		elog(ERROR, "Tessera heap batch column is out of range");
	if (rows == NULL || rows->nrows != batch->rows.nrows)
		elog(ERROR, "Tessera heap batch received a row mask of another batch");
	deformed = &heap->deformed[(Size) column * heap->nwords];
	nwords = tess_row_mask_word_count(rows->nrows);
	if (column < heap->tuple_desc->firstNonCachedOffsetAttr &&
		heap->tuple_desc->compact_attrs[column].attbyval)
		deform_cached(heap, column, rows, deformed, nwords);
	else
	{
		for (int word = 0; word < nwords; word++)
		{
			uint64		pending = rows->bits[word] & ~deformed[word];

			while (pending != 0)
			{
				deform(heap, column, word * 64 + pg_rightmost_one_pos64(pending));
				pending &= pending - 1;
			}
		}
	}
	for (int word = 0; word < nwords; word++)
	{
		count += pg_popcount64(rows->bits[word] & ~deformed[word]);
		deformed[word] |= rows->bits[word];
	}
	heap->stats.deformed_datums += count;
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

/* Pin a page for the batch, once per page: rows of a page come together. */
static inline void
pin_page(TessHeapBatch *heap, Buffer buffer)
{
	if (heap->npins == 0 || heap->pins[heap->npins - 1] != buffer)
	{
		IncrBufferRefCount(buffer);
		heap->pins[heap->npins++] = buffer;
	}
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
		pin_page(heap, buffer);
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

/*
 * Whether tuples of one descriptor deform the same by the other: the same
 * attributes, each of the same type, length, passing and alignment,
 * dropped or not, with a missing value or not. Names and the row type may
 * differ, as between a relation's descriptor and one made from a target
 * list.
 */
static bool
same_layout(TupleDesc a, TupleDesc b)
{
	if (a->natts != b->natts)
		return false;
	for (int attr = 0; attr < a->natts; attr++)
	{
		const CompactAttribute *left = TupleDescCompactAttr(a, attr);
		const CompactAttribute *right = TupleDescCompactAttr(b, attr);

		if (left->attlen != right->attlen || left->attbyval != right->attbyval ||
			left->attalignby != right->attalignby ||
			left->attisdropped != right->attisdropped ||
			left->atthasmissing != right->atthasmissing ||
			TupleDescAttr(a, attr)->atttypid != TupleDescAttr(b, attr)->atttypid)
			return false;
	}
	return true;
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
	else if (slot->tts_tupleDescriptor != heap->tuple_desc &&
			 slot->tts_tupleDescriptor != heap->other_desc)
	{
		if (!same_layout(slot->tts_tupleDescriptor, heap->tuple_desc))
			elog(ERROR, "Tessera heap batch input changed its descriptor");
		heap->other_desc = slot->tts_tupleDescriptor;
		/* Columns known not NULL only as far as both descriptors know it. */
		heap->first_non_guaranteed = Min(heap->first_non_guaranteed,
										 slot->tts_first_nonguaranteed);
	}
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

/*
 * The rows of one page in one call: the checks and the pin once, then a
 * plain loop over the line pointers into the batch's arrays. The page's
 * fields are read into locals first, so that the stores do not make the
 * compiler reload them.
 */
void
tess_heap_batch_append_page(TessHeapBatch *heap, Buffer buffer,
							BlockNumber block, const OffsetNumber *offsets,
							int n, Oid table_oid)
{
	int			first = heap->nrows;
	Page		page;
	HeapTupleData *tuples;
	TessDeformCursor *cursors;

	if (heap->tuple_desc == NULL)
		elog(ERROR, "Tessera heap batch needs a descriptor before tuples");
	if (unlikely(heap->sealed))
		elog(ERROR, "cannot append to a finished Tessera heap batch");
	if (n < 0 || first + n > heap->capacity)
		elog(ERROR, "Tessera heap batch is full");
	if (!BufferIsValid(buffer))
		elog(ERROR, "Tessera heap batch needs a pinned page");
	if (n == 0)
		return;
	pin_page(heap, buffer);
	page = BufferGetPage(buffer);
	tuples = heap->tuples + first;
	cursors = heap->cursors + first;
	for (int index = 0; index < n; index++)
	{
		OffsetNumber offset = offsets[index];
		ItemId		item = PageGetItemId(page, offset);

		tuples[index].t_len = ItemIdGetLength(item);
		tuples[index].t_data = (HeapTupleHeader) PageGetItem(page, item);
		ItemPointerSet(&tuples[index].t_self, block, offset);
		tuples[index].t_tableOid = table_oid;
		tess_deform_cursor_init(&cursors[index]);
	}
	heap->nrows = first + n;
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

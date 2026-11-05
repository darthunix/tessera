#include "postgres.h"

#include "access/htup_details.h"
#include "catalog/pg_type_d.h"
#include "executor/tuptable.h"
#include "storage/bufmgr.h"
#include "utils/memutils.h"

#include "tessera/heap_deform.h"
#include "tessera/kernel_ops.h"
#include "tessera/runtime.h"

#include "decimal_cache.h"
#include "heap_batch_internal.h"

#if TESS_GREENGAGE
/* Of master's CompactAttribute, the fields a walk reads. */
typedef struct TessHeapAttr
{
	int32		attcacheoff;
	int16		attlen;
	bool		attbyval;
	uint8		attalignby;
} TessHeapAttr;
#else
typedef CompactAttribute TessHeapAttr;
#endif

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
#if TESS_GREENGAGE
	/*
	 * PostgreSQL 12 has no compact attributes and no count of the
	 * attributes at cached offsets, and fills attcacheoff only as rows are
	 * deformed, so the batch keeps its own, once a descriptor: of each
	 * column the fields a walk reads, and the offsets of the leading
	 * fixed-width columns, the same in every tuple without a NULL up to
	 * them, as master's TupleDescFinalize counts them.
	 */
	TessHeapAttr *attrs;
	int			first_noncached;
#endif
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
	/*
	 * A numeric column's decimals, made once a batch for a consumer that
	 * reads them (TessDatumColumn.accept_decimals).
	 */
	TessDecimalCache decimals;
	uint64	   *selection;
	int			ncolumns;
	int			capacity;
	int			nwords;
	int			nrows;
	bool		sealed;
	TessHeapBatchStats stats;
	/*
	 * Per row, the cursor of its last restart: a column before the row's
	 * cursor continues it when it lies before the column, so that columns
	 * read in ascending order behind the row's cursor walk the tuple once
	 * between them, not once each from its start. Set at a row's first
	 * restart (back_set, a bit a row), so that appending a row stores no
	 * more than before.
	 */
	TessDeformCursor *back_cursors;
	uint64	   *back_set;
	/*
	 * The columns a walk keeps (deform_walk): per column, whether a walk
	 * forward stores its value as it passes it, and the first such column,
	 * ncolumns when none. They change only between batches, by the counts of
	 * a window of rows (count_keeping): per column, the rows a read of it or
	 * of a later column carried a walk to it or past it, and the rows read
	 * for it behind their cursor (count_behind), each a walk from the
	 * tuple's start unless the column is kept. A scan that has asked for
	 * one column past the cached offsets only (asked, nasked) counts
	 * nothing: it has nothing to keep. Only every KEEP_SAMPLE-th batch is
	 * counted (counting, batches), so that a scan that keeps nothing pays
	 * for the counts on a few of its batches.
	 */
	bool	   *kept;
	int			first_kept;
	uint64	   *reached;
	uint64	   *behind;
	uint64		window_rows;
	uint64		batches;
	bool		counting;
	bool	   *asked;
	int			nasked;
#ifdef USE_ASSERT_CHECKING
	/* Per column, the rows a walk stored it for: column * nwords. */
	uint64	   *stored;
#endif
};

/*
 * A column is kept when the walks its reads behind the cursor would take
 * outweigh a store on every row a walk carries past it: behind × distance
 * × 2 ≥ reached, a store costing about half a step of a walk. Counted on
 * one batch in KEEP_SAMPLE, over a window of KEEP_WINDOW_ROWS rows
 * counted, about 4096 rows read; the first window keeps nothing. See
 * "Columns a walk keeps" in docs/runtime.md.
 */
#define KEEP_SAMPLE 4
#define KEEP_WINDOW_ROWS 1024

#if TESS_GREENGAGE
#define heap_attrs(heap) ((heap)->attrs)
#define heap_first_noncached(heap) ((heap)->first_noncached)

/* Three helpers of master's access/tupmacs.h, as master writes them. */
static inline int
first_null_attr(const bits8 *bits, int natts)
{
	for (int attr = 0; attr < natts; attr++)
		if (att_isnull(attr, bits))
			return attr;
	return natts;
}

#define fetch_att_noerr(T, attbyval, attlen) fetch_att(T, attbyval, attlen)

static inline Datum
align_fetch_then_add(const char *tupptr, uint32 *off, bool attbyval, int attlen,
					 uint8 attalignby)
{
	Datum		res;

	if (attlen > 0)
	{
		const char *offset_ptr;

		*off = TYPEALIGN(attalignby, *off);
		offset_ptr = tupptr + *off;
		*off += attlen;
		if (attbyval)
			return fetch_att(offset_ptr, true, attlen);
		return PointerGetDatum(offset_ptr);
	}
	else if (attlen == -1)
	{
		if (!VARATT_IS_SHORT(tupptr + *off))
			*off = TYPEALIGN(attalignby, *off);
		res = PointerGetDatum(tupptr + *off);
		*off += VARSIZE_ANY(DatumGetPointer(res));
		return res;
	}
	Assert(attlen == -2);
	*off = TYPEALIGN(attalignby, *off);
	res = PointerGetDatum(tupptr + *off);
	*off += strlen(tupptr + *off) + 1;
	return res;
}

/*
 * The batch's attributes of a new descriptor: the fields a walk reads, and
 * the offsets of the leading fixed-width columns, as master's
 * TupleDescFinalize computes them.
 */
static void
cache_offsets(TessHeapBatch *heap)
{
	int			off = 0;
	int			column;

	for (column = 0; column < heap->ncolumns; column++)
	{
		Form_pg_attribute att = TupleDescAttr(heap->tuple_desc, column);
		TessHeapAttr *attr = &heap->attrs[column];

		attr->attcacheoff = -1;
		attr->attlen = att->attlen;
		attr->attbyval = att->attbyval;
		attr->attalignby = att->attalign == 'd' ? ALIGNOF_DOUBLE :
			att->attalign == 'i' ? ALIGNOF_INT :
			att->attalign == 's' ? ALIGNOF_SHORT : 1;
	}
	for (column = 0; column < heap->ncolumns; column++)
	{
		TessHeapAttr *attr = &heap->attrs[column];

		if (attr->attlen <= 0)
			break;
		off = TYPEALIGN(attr->attalignby, off);
		if (off > PG_INT16_MAX)
			break;
		attr->attcacheoff = off;
		off += attr->attlen;
	}
	heap->first_noncached = column;
}
#else
#define heap_attrs(heap) ((heap)->tuple_desc->compact_attrs)
#define heap_first_noncached(heap) ((heap)->tuple_desc->firstNonCachedOffsetAttr)
#define cache_offsets(heap) ((void) (heap))
#endif

/*
 * The cursor of a row's column before the row's cursor: the cursor of the
 * row's last restart when it lies before the column, else that one
 * restarted from the tuple's start. Out of line, so that the common path
 * of deform stays as it was.
 */
static pg_noinline TessDeformCursor *
back_cursor(TessHeapBatch *heap, int row, AttrNumber attnum)
{
	TessDeformCursor *cursor = &heap->back_cursors[row];
	uint64		bit = UINT64CONST(1) << (row % 64);

	if ((heap->back_set[row / 64] & bit) == 0 || cursor->nvalid >= attnum)
	{
		tess_deform_cursor_init(cursor);
		heap->back_set[row / 64] |= bit;
		heap->stats.restarted_datums++;
	}
	return cursor;
}

/* Store a value of the row a walk passes; under assertions, mark it stored. */
static pg_always_inline void
keep_value(TessHeapBatch *heap, int column, int row, Datum value, bool isnull)
{
	Size		offset = (Size) column * heap->capacity + row;

	heap->values[offset] = value;
	heap->isnull[offset] = isnull;
#ifdef USE_ASSERT_CHECKING
	heap->stored[(Size) column * heap->nwords + row / 64] |= UINT64CONST(1) << (row % 64);
#endif
}

/*
 * One attribute of a walk. A value of varying length with a one-byte
 * header, the form of every short value, is read without the general
 * step's checks for padding, an external value and a four-byte header.
 */
static pg_always_inline Datum
walk_step(const char *tp, uint32 *off, const TessHeapAttr *cattr)
{
	pg_assume(cattr->attlen > 0 || cattr->attlen == -1);
	if (cattr->attlen == -1)
	{
		const char *value = tp + *off;

		if (VARATT_IS_1B(value) && !VARATT_IS_1B_E(value))
		{
			*off += VARSIZE_1B(value);
			return PointerGetDatum(value);
		}
	}
	return align_fetch_then_add(tp, off, cattr->attbyval, cattr->attlen,
								cattr->attalignby);
}

/*
 * Walk a row's cursor forward to a column past the cached offsets, as
 * tess_deform_advance does, storing the column's value and, when keeping,
 * the value of every kept column it passes, so that a request for one of
 * them behind the cursor finds it (deform_kept_rows). Every forward step
 * of a cursor past a kept column keeps: a row with NULLs stores its NULLs,
 * and a row shorter than the column stops at its last attribute, so no
 * column the row lacks lies behind its cursor. The attributes at cached
 * offsets are jumped over: they are fixed-width, and no kept column lies
 * among them.
 */
static pg_always_inline void
deform_walk(TessHeapBatch *heap, int column, int row, bool keeping)
{
	HeapTuple	tuple = &heap->tuples[row];
	HeapTupleHeader tup = tuple->t_data;
	TessDeformCursor *cursor = &heap->cursors[row];
	TupleDesc	desc = heap->tuple_desc;
	const TessHeapAttr *cattrs = heap_attrs(heap);
	const bool *kept = heap->kept;
	char	   *tp = (char *) tup + tup->t_hoff;
	int			end = Min(HeapTupleHeaderGetNatts(tup), column + 1);
	int			first_null = HeapTupleHasNulls(tuple) ?
		first_null_attr(tup->t_bits, end) : end;
	int			cached = Min(heap_first_noncached(heap), first_null);
	int			attr = cursor->nvalid;
	uint32		off = cursor->off;

	Assert(attr <= column && column >= heap_first_noncached(heap) &&
		   heap->first_kept >= heap_first_noncached(heap));
	if (attr < cached)
	{
		const TessHeapAttr *cattr = &cattrs[cached - 1];

		Assert(cattr->attcacheoff >= 0 && cattr->attlen > 0);
		off = cattr->attcacheoff + cattr->attlen;
		attr = cached;
	}
	for (; attr < first_null; attr++)
	{
		Datum		value = walk_step(tp, &off, &cattrs[attr]);

		if (attr == column || (keeping && kept[attr]))
			keep_value(heap, attr, row, value, false);
	}
	for (; attr < end; attr++)
	{
		Datum		value = (Datum) 0;
		bool		isnull = att_isnull(attr, tup->t_bits);

		if (!isnull)
			value = walk_step(tp, &off, &cattrs[attr]);
		if (attr == column || (keeping && kept[attr]))
			keep_value(heap, attr, row, value, isnull);
	}
	cursor->nvalid = end;
	cursor->off = off;
	if (end <= column)
	{
		bool		isnull;
		Datum		value = getmissingattr(desc, column + 1, &isnull);

		keep_value(heap, column, row, isnull ? (Datum) 0 : value, isnull);
	}
}

/*
 * Deform one value, resuming the row's cursor or, before it, its back
 * cursor. A walk past a kept column, and a kept column behind the cursor,
 * do not come here (deform_kept_rows).
 */
static void
deform(TessHeapBatch *heap, int column, int row)
{
	AttrNumber	attnum = column + 1;
	TessDeformCursor *cursor = &heap->cursors[row];
	Datum		value = (Datum) 0;
	bool		isnull = false;
	Size		offset = (Size) column * heap->capacity + row;

	Assert(heap->first_kept >= column || cursor->nvalid >= attnum);
	if (unlikely(cursor->nvalid >= attnum))
	{
		Assert(!heap->kept[column]);
		cursor = back_cursor(heap, row, attnum);
	}
	else if (column >= heap_first_noncached(heap))
	{
		deform_walk(heap, column, row, false);
		return;
	}
	if (!tess_deform_advance(cursor, &heap->tuples[row], heap->tuple_desc,
							 heap->first_non_guaranteed, attnum,
							 &value, &isnull, false))
		value = getmissingattr(heap->tuple_desc, attnum, &isnull);
	heap->values[offset] = isnull ? (Datum) 0 : value;
	heap->isnull[offset] = isnull;
}

/*
 * A column's requested rows in a batch that keeps columns: a row whose
 * cursor passed a kept column holds it, a walk forward to a column past a
 * kept one keeps them on the way, and any other row deforms as before.
 * Chosen once a request, so that a scan that keeps no column before the
 * one it reads runs deform's loop as without kept columns.
 */
static pg_noinline void
deform_kept_rows(TessHeapBatch *heap, int column, const TessRowMask *rows,
				 const uint64 *deformed, int nwords)
{
	bool		kept = heap->kept[column];
	bool		keeping = heap->first_kept < column;

	for (int word = 0; word < nwords; word++)
	{
		uint64		pending = rows->bits[word] & ~deformed[word];

		while (pending != 0)
		{
			int			row = word * 64 + pg_rightmost_one_pos64(pending);

			if (heap->cursors[row].nvalid <= column)
			{
				if (keeping)
					deform_walk(heap, column, row, true);
				else
					deform(heap, column, row);
			}
			else if (!kept)
				deform(heap, column, row);
			else
				Assert(heap->stored[(Size) column * heap->nwords + word] &
					   (UINT64CONST(1) << (row % 64)));
			pending &= pending - 1;
		}
	}
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
	const TessHeapAttr *cattr = &heap_attrs(heap)[column];
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

/*
 * Count a request's rows behind their cursor: a row whose cursor a read of
 * a later column carried past this one, as the masks of the later columns
 * tell, without a look at the row. A short row whose walk stopped before
 * the column counts too: these are counts, not the path of a row.
 */
static inline void
count_behind(TessHeapBatch *heap, int column, const TessRowMask *rows,
			 const uint64 *deformed, int nwords)
{
	for (int word = 0; word < nwords; word++)
	{
		uint64		ahead = 0;

		for (int later = column + 1; later < heap->ncolumns; later++)
			ahead |= heap->deformed[(Size) later * heap->nwords + word];
		heap->behind[column] +=
			pg_popcount64(rows->bits[word] & ~deformed[word] & ahead);
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
	if (column >= heap_first_noncached(heap))
	{
		if (unlikely(!heap->asked[column]))
		{
			heap->asked[column] = true;
			heap->nasked++;
		}
		if (heap->nasked > 1 && heap->counting)
			count_behind(heap, column, rows, deformed, nwords);
	}
	if (column < heap_first_noncached(heap) && heap_attrs(heap)[column].attbyval)
		deform_cached(heap, column, rows, deformed, nwords);
	else if (heap->kept[column] || heap->first_kept < column)
		deform_kept_rows(heap, column, rows, deformed, nwords);
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
	if (result->struct_size >= TESS_DATUM_COLUMN_DECIMALS_SIZE)
	{
		result->decimal_rows = NULL;
		if (result->accept_decimals && heap->tuple_desc != NULL &&
			TupleDescAttr(heap->tuple_desc, column)->atttypid == NUMERICOID)
			tess_decimal_cache_read(&heap->decimals, column,
									&heap->values[(Size) column * heap->capacity],
									&heap->isnull[(Size) column * heap->capacity],
									rows, result);
	}
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
	tess_decimal_cache_init(&heap->decimals, context, heap->ncolumns, heap->capacity);
	heap->tuples = MemoryContextAlloc(context,
									  mul_size(sizeof(HeapTupleData), config->capacity));
	heap->cursors = MemoryContextAlloc(context,
									   mul_size(sizeof(TessDeformCursor), config->capacity));
	heap->back_cursors = MemoryContextAlloc(context,
											mul_size(sizeof(TessDeformCursor), config->capacity));
	heap->back_set = MemoryContextAllocZero(context, sizeof(uint64) * heap->nwords);
	heap->kept = MemoryContextAllocZero(context, sizeof(bool) * Max(config->ncolumns, 1));
	heap->first_kept = config->ncolumns;
	heap->reached = MemoryContextAllocZero(context, sizeof(uint64) * Max(config->ncolumns, 1));
	heap->behind = MemoryContextAllocZero(context, sizeof(uint64) * Max(config->ncolumns, 1));
	heap->asked = MemoryContextAllocZero(context, sizeof(bool) * Max(config->ncolumns, 1));
	heap->counting = true;
#ifdef USE_ASSERT_CHECKING
	heap->stored = MemoryContextAllocZero(context,
										  mul_size(sizeof(uint64),
												   mul_size(config->ncolumns, heap->nwords)));
#endif
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
#if TESS_GREENGAGE
	heap->attrs = MemoryContextAlloc(context,
									 sizeof(TessHeapAttr) * Max(config->ncolumns, 1));
#endif
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
		cache_offsets(heap);
	}
	heap->batch.abi_version = TESS_BATCH_ABI_VERSION;
	heap->batch.struct_size = sizeof(TessBatch);
	heap->batch.values_stored = true;
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

/*
 * The counts of a batch about to be reset, if it is one counted, for the
 * columns past the cached offsets, and at a window's end the columns kept
 * for the next one. A row's cursor stands past a column when the row was
 * read for it or for a later one: the union of their masks, taken from
 * the last column back. A column no walk reached in the window keeps its
 * flag. Sets whether the next batch is counted.
 */
static void
count_keeping(TessHeapBatch *heap)
{
	int			start;
	int			ncolumns = heap->ncolumns;
	int			nwords = tess_row_mask_word_count(heap->nrows);
	bool		counted = heap->counting;

	/* An empty batch read nothing, and leaves the turn of the counted one. */
	if (heap->nrows == 0)
		return;
	heap->counting = ++heap->batches % KEEP_SAMPLE == 0;
	if (!counted || heap->nasked < 2)
		return;
	start = heap_first_noncached(heap);
	for (int word = 0; word < nwords; word++)
	{
		uint64		reached = 0;

		for (int column = ncolumns - 1; column >= start; column--)
		{
			reached |= heap->deformed[(Size) column * heap->nwords + word];
			heap->reached[column] += pg_popcount64(reached);
		}
	}
	heap->window_rows += heap->nrows;
	if (heap->window_rows < KEEP_WINDOW_ROWS)
		return;
	heap->first_kept = ncolumns;
	for (int column = start; column < ncolumns; column++)
	{
		/* The steps of a walk from the cached offsets to the column. */
		uint64		distance = column - start + 1;

		if (heap->reached[column] > 0)
			heap->kept[column] = heap->behind[column] * distance * 2 >=
				heap->reached[column];
		if (heap->kept[column] && heap->first_kept == ncolumns)
			heap->first_kept = column;
		heap->reached[column] = 0;
		heap->behind[column] = 0;
	}
	heap->window_rows = 0;
}

void
tess_heap_batch_reset(TessHeapBatch *heap)
{
	unpin(heap);
	count_keeping(heap);
	memset(heap->deformed, 0, sizeof(uint64) * (Size) heap->ncolumns * heap->nwords);
#ifdef USE_ASSERT_CHECKING
	memset(heap->stored, 0, sizeof(uint64) * (Size) heap->ncolumns * heap->nwords);
#endif
	memset(heap->back_set, 0, sizeof(uint64) * heap->nwords);
	tess_decimal_cache_reset(&heap->decimals);
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
#if TESS_GREENGAGE
		const Form_pg_attribute left = TupleDescAttr(a, attr);
		const Form_pg_attribute right = TupleDescAttr(b, attr);

		if (left->attlen != right->attlen || left->attbyval != right->attbyval ||
			left->attalign != right->attalign ||
#else
		const CompactAttribute *left = TupleDescCompactAttr(a, attr);
		const CompactAttribute *right = TupleDescCompactAttr(b, attr);

		if (left->attlen != right->attlen || left->attbyval != right->attbyval ||
			left->attalignby != right->attalignby ||
#endif
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
		heap->first_non_guaranteed = tess_slot_first_nonguaranteed(slot);
		cache_offsets(heap);
	}
	else if (slot->tts_tupleDescriptor != heap->tuple_desc &&
			 slot->tts_tupleDescriptor != heap->other_desc)
	{
		if (!same_layout(slot->tts_tupleDescriptor, heap->tuple_desc))
			elog(ERROR, "Tessera heap batch input changed its descriptor");
		heap->other_desc = slot->tts_tupleDescriptor;
		/* Columns known not NULL only as far as both descriptors know it. */
		heap->first_non_guaranteed = Min(heap->first_non_guaranteed,
										 tess_slot_first_nonguaranteed(slot));
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

bool
tess_batch_is_heap(const TessBatch *batch)
{
	return batch != NULL && batch->ops == &heap_ops;
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

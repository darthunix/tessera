#include "postgres.h"

#include <string.h>

#include "access/heapam.h"
#include "access/relscan.h"
#include "access/table.h"
#include "access/tableam.h"
#include "executor/tuptable.h"
#include "fmgr.h"
#include "storage/bufmgr.h"
#include "storage/bufpage.h"
#include "utils/builtins.h"
#include "utils/snapmgr.h"

#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_heap_batch);
PG_FUNCTION_INFO_V1(tessera_test_heap_batch_errors);

/* An expectation, reported by number on failure. */
static bool
check(int number, bool holds)
{
	if (!holds)
		elog(NOTICE, "expectation %d failed", number);
	return holds;
}

/* A heap batch taking its descriptor from the first slot, or from desc. */
static TessHeapBatch *
make_heap(int ncolumns, int capacity, TupleDesc desc)
{
	TessHeapBatchConfig config = TESS_STRUCT_INITIALIZER(TessHeapBatchConfig);

	config.parent_context = CurrentMemoryContext;
	config.ncolumns = ncolumns;
	config.capacity = capacity;
	config.tuple_desc = desc;
	config.first_non_guaranteed_attr = desc == NULL ? 0 :
		desc->firstNonGuaranteedAttr;
	return tess_heap_batch_create(&config);
}

static void
get_column(TessBatch *batch, int column, const TessRowMask *rows,
		   TessDatumColumn *result)
{
	*result = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
	batch->ops->get_datum_column(batch, column, rows,
								 TESS_COLUMN_FOR_FILTER, result);
	if (result->values == NULL || result->isnull == NULL ||
		result->nrows != batch->rows.nrows)
		elog(ERROR, "the heap batch returned an invalid column");
}

/* The rows of the mask with the given parity of their number. */
static TessRowMask
parity_mask(int nrows, int parity, uint64 *bits)
{
	TessRowMask mask = {nrows, bits};

	memset(bits, 0, sizeof(uint64) * tess_row_mask_word_count(nrows));
	for (int row = parity; row < nrows; row += 2)
		bits[row / 64] |= UINT64CONST(1) << (row % 64);
	return mask;
}

/* Row r of the first batch is the tuple with i = r + 1 (see the script). */
static bool
row_holds(const TessDatumColumn *column, int row, int attnum)
{
	int			i = row + 1;

	switch (attnum)
	{
		case 1:
			return !column->isnull[row] && DatumGetInt32(column->values[row]) == i;
		case 2:
			if (i % 3 == 0)
				return column->isnull[row];
			return !column->isnull[row] &&
				VARSIZE_ANY_EXHDR(DatumGetTextPP(column->values[row])) == 300 &&
				memcmp(VARDATA_ANY(DatumGetTextPP(column->values[row])),
					   "xxxx", 4) == 0;
		case 3:
			return !column->isnull[row] &&
				DatumGetInt64(column->values[row]) == (int64) i * 10;
		default:
			return false;
	}
}

typedef enum FillMode
{
	FILL_SLOTS,
	FILL_TUPLES,
	FILL_PAGES
} FillMode;

/*
 * Fill the batch with the relation's first rows, from the scan's slots,
 * as tuples rebuilt from the page items the slots point at, or page by
 * page from the scan's list of visible tuples as the scan node does, and
 * keep scanning so that the scan leaves the batch's pages. Returns the
 * rows scanned.
 */
static int
fill(TessHeapBatch *heap, Relation rel, FillMode mode, int capacity)
{
	TupleTableSlot *slot = table_slot_create(rel, NULL);
	TableScanDesc scan = table_beginscan(rel, GetActiveSnapshot(), 0, NULL, 0);
	HeapScanDesc hscan = (HeapScanDesc) scan;
	int			scanned = 0;
	int			room = capacity;

	while (table_scan_getnextslot(scan, ForwardScanDirection, slot))
	{
		BufferHeapTupleTableSlot *bslot = (BufferHeapTupleTableSlot *) slot;

		if (mode == FILL_PAGES)
		{
			/* The first tuple of a page: take the page's list, skip the rest. */
			int			n = Min(room, hscan->rs_ntuples);

			tess_heap_batch_append_page(heap, bslot->buffer, hscan->rs_cblock,
										hscan->rs_vistuples, n,
										RelationGetRelid(rel));
			room -= n;
			scanned += hscan->rs_ntuples;
			hscan->rs_cindex = hscan->rs_ntuples - 1;
			continue;
		}
		if (!tess_heap_batch_is_full(heap))
		{
			if (mode == FILL_TUPLES)
			{
				Page		page = BufferGetPage(bslot->buffer);
				OffsetNumber offset = ItemPointerGetOffsetNumber(&bslot->base.tuple->t_self);
				ItemId		item = PageGetItemId(page, offset);
				HeapTupleData tuple;

				tuple.t_len = ItemIdGetLength(item);
				tuple.t_data = (HeapTupleHeader) PageGetItem(page, item);
				tuple.t_self = bslot->base.tuple->t_self;
				tuple.t_tableOid = RelationGetRelid(rel);
				tess_heap_batch_append_tuple(heap, &tuple, bslot->buffer);
			}
			else
				tess_heap_batch_append_slot(heap, slot);
		}
		scanned++;
	}
	table_endscan(scan);
	ExecDropSingleTupleTableSlot(slot);
	return scanned;
}

/*
 * A batch of 64 rows: columns deformed on request and only for the
 * requested rows, cursors resumed and restarted, values valid after the
 * scan left their pages, and the release of the pins.
 */
static bool
verify(TessHeapBatch *heap, Oid relid, int base)
{
	const TessHeapBatchStats *stats = tess_heap_batch_stats(heap);
	TessBatch  *batch;
	TessDatumColumn column;
	TessRowMask mask;
	uint64		bits[1];
	bool		result = true;
	bool		holds;

	batch = tess_heap_batch_finish(heap, relid);
	result &= check(base + 1, batch != NULL && batch->rows.nrows == 64 &&
		tess_row_mask_count(&batch->rows) == 64 && batch->table_oid == relid &&
		stats->deformed_datums == 0 && stats->copied_tuples == 0);

	/* The first column for the even rows only, then for the rest. */
	mask = parity_mask(64, 0, bits);
	get_column(batch, 0, &mask, &column);
	holds = stats->deformed_datums == 32;
	for (int row = 0; row < 64; row += 2)
		holds &= row_holds(&column, row, 1);
	result &= check(base + 2, holds);
	get_column(batch, 0, &batch->rows, &column);
	holds = stats->deformed_datums == 64;
	for (int row = 0; row < 64; row++)
		holds &= row_holds(&column, row, 1);
	result &= check(base + 3, holds);
	get_column(batch, 0, &batch->rows, &column);
	result &= check(base + 4, stats->deformed_datums == 64 &&
		stats->restarted_datums == 0);

	/* The bigint behind the text resumes each row's cursor past the text. */
	mask = parity_mask(64, 1, bits);
	get_column(batch, 2, &mask, &column);
	holds = stats->deformed_datums == 96 && stats->restarted_datums == 0;
	for (int row = 1; row < 64; row += 2)
		holds &= row_holds(&column, row, 3);
	result &= check(base + 5, holds);
	/* The text before those cursors restarts them; the others resume. */
	get_column(batch, 1, &batch->rows, &column);
	holds = stats->deformed_datums == 160 && stats->restarted_datums == 32;
	for (int row = 0; row < 64; row++)
		holds &= row_holds(&column, row, 2);
	result &= check(base + 6, holds);
	/* Values point into pages the scan has long left. */
	result &= check(base + 7, row_holds(&column, 0, 2) && row_holds(&column, 1, 2));

	batch->ops->release(batch);
	tess_heap_batch_reset(heap);
	result &= check(base + 8, tess_heap_batch_finish(heap, relid) == NULL);
	return result;
}

Datum
tessera_test_heap_batch(PG_FUNCTION_ARGS)
{
	Oid			relid = PG_GETARG_OID(0);
	Relation	rel = table_open(relid, AccessShareLock);
	TessHeapBatch *heap;
	TessBatch  *batch;
	TessDatumColumn column;
	bool		result = true;

	/* From the scan's slots, the descriptor taken from the first one. */
	heap = make_heap(3, 64, NULL);
	result &= check(1, fill(heap, rel, FILL_SLOTS, 64) > 64 &&
		tess_heap_batch_is_full(heap));
	result &= verify(heap, relid, 1);
	/* As tuples rebuilt from page items, the descriptor configured. */
	heap = make_heap(3, 64, RelationGetDescr(rel));
	result &= check(10, fill(heap, rel, FILL_TUPLES, 64) > 64 &&
		tess_heap_batch_is_full(heap));
	result &= verify(heap, relid, 10);
	/* Page by page from the scan's list, the last page taken in part. */
	heap = make_heap(3, 64, RelationGetDescr(rel));
	result &= check(30, fill(heap, rel, FILL_PAGES, 64) > 64 &&
		tess_heap_batch_is_full(heap));
	result &= verify(heap, relid, 30);

	/* A row from a virtual slot, and a tuple without a page, are copied. */
	{
		TupleTableSlot *virtual = MakeSingleTupleTableSlot(RelationGetDescr(rel),
														   &TTSOpsVirtual);
		TessHeapBatch *other = make_heap(3, 4, NULL);
		HeapTuple	tuple;

		ExecClearTuple(virtual);
		virtual->tts_values[0] = Int32GetDatum(7);
		virtual->tts_isnull[0] = false;
		virtual->tts_values[1] = PointerGetDatum(cstring_to_text("copied"));
		virtual->tts_isnull[1] = false;
		virtual->tts_values[2] = Int64GetDatum(70);
		virtual->tts_isnull[2] = false;
		ExecStoreVirtualTuple(virtual);
		tess_heap_batch_append_slot(other, virtual);
		tuple = ExecCopySlotHeapTuple(virtual);
		ExecClearTuple(virtual);
		tess_heap_batch_append_tuple(other, tuple, InvalidBuffer);
		heap_freetuple(tuple);
		batch = tess_heap_batch_finish(other, relid);
		get_column(batch, 1, &batch->rows, &column);
		result &= check(20, batch->rows.nrows == 2 &&
			tess_heap_batch_stats(other)->copied_tuples == 2 &&
			strcmp(text_to_cstring(DatumGetTextPP(column.values[0])), "copied") == 0 &&
			strcmp(text_to_cstring(DatumGetTextPP(column.values[1])), "copied") == 0);
		batch->ops->release(batch);
		ExecDropSingleTupleTableSlot(virtual);
	}
	table_close(rel, AccessShareLock);
	PG_RETURN_BOOL(result);
}

Datum
tessera_test_heap_batch_errors(PG_FUNCTION_ARGS)
{
	int32		kind = PG_GETARG_INT32(0);
	Oid			relid = PG_GETARG_OID(1);
	Relation	rel = table_open(relid, AccessShareLock);
	TupleTableSlot *slot = table_slot_create(rel, NULL);
	TableScanDesc scan = table_beginscan(rel, GetActiveSnapshot(), 0, NULL, 0);
	TessHeapBatch *heap = make_heap(3, 2, NULL);
	TessBatch  *batch;
	TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	TessRowMask foreign = {5, NULL};
	HeapTupleData tuple = {0};

	while (!tess_heap_batch_is_full(heap) &&
		   table_scan_getnextslot(scan, ForwardScanDirection, slot))
		tess_heap_batch_append_slot(heap, slot);
	switch (kind)
	{
		case 0:
			tess_heap_batch_append_slot(heap, slot);
			break;
		case 1:
			batch = tess_heap_batch_finish(heap, relid);
			batch->ops->get_datum_column(batch, 3, &batch->rows,
										 TESS_COLUMN_FOR_FILTER, &column);
			break;
		case 2:
			batch = tess_heap_batch_finish(heap, relid);
			batch->ops->get_datum_column(batch, 0, &foreign,
										 TESS_COLUMN_FOR_FILTER, &column);
			break;
		case 3:
			tess_heap_batch_append_tuple(make_heap(3, 2, NULL), &tuple, InvalidBuffer);
			break;
		case 4:
			tess_heap_batch_append_page(make_heap(3, 2, RelationGetDescr(rel)),
										((BufferHeapTupleTableSlot *) slot)->buffer,
										((HeapScanDesc) scan)->rs_cblock,
										((HeapScanDesc) scan)->rs_vistuples, 3,
										relid);
			break;
		case 5:
			tess_heap_batch_append_page(make_heap(3, 2, RelationGetDescr(rel)),
										InvalidBuffer, 0,
										((HeapScanDesc) scan)->rs_vistuples, 1,
										relid);
			break;
		default:
			elog(ERROR, "unknown error case %d", kind);
	}
	elog(ERROR, "Tessera test expected a heap batch error");
	PG_RETURN_VOID();
}

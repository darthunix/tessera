/*
 * Batches of heap tuples, deformed a column at a time on request. Part of
 * tessera/runtime.h.
 */
#ifndef TESSERA_RUNTIME_HEAP_BATCH_H
#define TESSERA_RUNTIME_HEAP_BATCH_H

#include "postgres.h"

#include "access/tupdesc.h"
#include "executor/tuptable.h"
#include "storage/buf.h"

#include "tessera/abi.h"
#include "tessera/batch.h"

/*
 * A heap batch keeps the heap tuples of up to capacity rows instead of
 * copying their columns: a row appended from a buffer heap tuple slot is
 * kept as a reference into its page, which stays pinned until the batch
 * is released, and a row from any other slot is copied as a tuple. A
 * column is deformed only when a consumer asks for it and only for the
 * rows it asks for, resuming each row from where an earlier request
 * stopped (see tessera/heap_deform.h); by-reference values point into
 * the tuples. The batch exposes get_datum_column and release. See
 * docs/runtime.md.
 */
typedef struct TessHeapBatch TessHeapBatch;

typedef struct TessHeapBatchConfig
{
	Size		struct_size;
	/* Owns the batch and its arrays. */
	MemoryContext parent_context;
	/* Columns of the batch: the leading attributes of the tuples appended. */
	int			ncolumns;
	/* Rows in one batch; more than 64 is allowed. */
	int			capacity;
	/*
	 * The tuples' descriptor and the number of leading attributes every
	 * tuple has present, non-NULL and by value (the descriptor's
	 * firstNonGuaranteedAttr, or 0). NULL takes both from the first slot
	 * appended; tuples appended directly need them here.
	 */
	TupleDesc	tuple_desc;
	int			first_non_guaranteed_attr;
} TessHeapBatchConfig;

#define TESS_HEAP_BATCH_CONFIG_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessHeapBatchConfig, capacity)

typedef struct TessHeapBatchStats
{
	/* Values deformed on request. */
	uint64		deformed_datums;
	/* Of those, values before a row's cursor, deformed from the row's start. */
	uint64		restarted_datums;
	/* Rows copied as tuples, from slots without a pinned page. */
	uint64		copied_tuples;
} TessHeapBatchStats;

/* Allocate an empty heap batch in the configured context. */
extern TessHeapBatch *tess_heap_batch_create(const TessHeapBatchConfig *config);

/*
 * Drop the previous rows and start an empty batch. The caller must first
 * take a previously returned batch off its slot binding, which releases
 * it; a batch never released is released here.
 */
extern void tess_heap_batch_reset(TessHeapBatch *batch);

/* True after capacity rows were appended or the batch was finished. */
extern bool tess_heap_batch_is_full(const TessHeapBatch *batch);

/*
 * Append one row: a reference into the page of a buffer heap tuple slot,
 * or a copy of any other slot's tuple. The slot's descriptor is the
 * batch's from the first row on; the slot may be reused afterwards.
 */
extern void tess_heap_batch_append_slot(TessHeapBatch *batch,
										TupleTableSlot *slot);

/*
 * Append one tuple by its header: a reference into the page of buffer,
 * which the batch pins, or a copy when buffer is invalid. The descriptor
 * must be known, from the configuration or an earlier slot.
 */
extern void tess_heap_batch_append_tuple(TessHeapBatch *batch,
										 const HeapTupleData *tuple,
										 Buffer buffer);

/*
 * Append n tuples of the page of buffer, which the batch pins once, by
 * their line pointers: the visible tuples of a page as a scan lists them.
 * The descriptor must be known; n rows must fit.
 */
extern void tess_heap_batch_append_page(TessHeapBatch *batch, Buffer buffer,
										BlockNumber block,
										const OffsetNumber *offsets, int n,
										Oid table_oid);

/*
 * Finish the batch and return it, or NULL without rows. The batch stays
 * valid until it is released; finishing again returns the same batch.
 */
extern TessBatch *tess_heap_batch_finish(TessHeapBatch *batch, Oid table_oid);

extern const TessHeapBatchStats *tess_heap_batch_stats(const TessHeapBatch *batch);

#endif							/* TESSERA_RUNTIME_HEAP_BATCH_H */

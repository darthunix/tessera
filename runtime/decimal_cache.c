#include "postgres.h"

#include "utils/memutils.h"

#include "tessera/runtime.h"

#include "decimal_cache.h"

void
tess_decimal_cache_init(TessDecimalCache *cache, MemoryContext context,
						int ncolumns, int capacity)
{
	cache->context = context;
	cache->ncolumns = ncolumns;
	cache->capacity = capacity;
	cache->nwords = tess_row_mask_word_count(capacity);
}

/*
 * A value of at most 18 digits at the scale of the batch's first decimal
 * goes as its int64 and its bit, any other (NULL, NaN, longer, of another
 * scale) as its numeric, so every consumer of the column's decimals reads
 * the same values without reading a numeric again.
 */
void
tess_decimal_cache_read(TessDecimalCache *cache, int column,
						const Datum *values, const bool *isnull,
						const TessRowMask *rows, TessDatumColumn *result)
{
	TessDatumColumn source = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	TessRowMask pending;
	TessRowMask decimals;
	uint64	   *read;
	int			nwords = tess_row_mask_word_count(rows->nrows);

	Assert(column >= 0 && column < cache->ncolumns && rows->nrows <= cache->capacity);
	if (cache->kernels == NULL && (cache->kernels = tess_runtime_kernels()) == NULL)
		return;
	if (cache->values == NULL)
	{
		cache->values = MemoryContextAllocZero(cache->context, sizeof(Datum *) * cache->ncolumns);
		cache->read = MemoryContextAllocZero(cache->context, sizeof(uint64 *) * cache->ncolumns);
		cache->rows = MemoryContextAllocZero(cache->context, sizeof(uint64 *) * cache->ncolumns);
		cache->scale = MemoryContextAlloc(cache->context, sizeof(int) * cache->ncolumns);
		cache->pending = MemoryContextAllocZero(cache->context, sizeof(uint64) * cache->nwords);
		cache->status = (TessStatus) TESS_STRUCT_INITIALIZER(TessStatus);
		for (int index = 0; index < cache->ncolumns; index++)
			cache->scale[index] = -1;
	}
	if (cache->values[column] == NULL)
	{
		cache->values[column] = MemoryContextAllocZero(cache->context,
													   sizeof(Datum) * cache->capacity);
		cache->read[column] = MemoryContextAllocZero(cache->context,
													 sizeof(uint64) * cache->nwords);
		cache->rows[column] = MemoryContextAllocZero(cache->context,
													 sizeof(uint64) * cache->nwords);
	}
	read = cache->read[column];
	pending = (TessRowMask) {rows->nrows, cache->pending};
	for (int word = 0; word < nwords; word++)
		pending.bits[word] = rows->bits[word] & ~read[word];
	source.values = values;
	source.isnull = isnull;
	source.nrows = rows->nrows;
	decimals = (TessRowMask) {rows->nrows, cache->rows[column]};
	if (cache->kernels->decimal_read(&source, &pending, &cache->scale[column],
									 cache->values[column], NULL, &decimals,
									 &cache->status) != TESS_OK)
		tess_status_report(&cache->status);
	for (int word = 0; word < nwords; word++)
		read[word] |= rows->bits[word];
	result->values = cache->values[column];
	result->decimal_rows = cache->rows[column];
	result->decimal_scale = cache->scale[column];
}

void
tess_decimal_cache_reset(TessDecimalCache *cache)
{
	if (cache->values == NULL)
		return;
	for (int column = 0; column < cache->ncolumns; column++)
	{
		cache->scale[column] = -1;
		if (cache->values[column] == NULL)
			continue;
		memset(cache->read[column], 0, sizeof(uint64) * cache->nwords);
		memset(cache->rows[column], 0, sizeof(uint64) * cache->nwords);
	}
}

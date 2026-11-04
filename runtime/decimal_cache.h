/*
 * A batch's numeric columns read as decimals once a batch, shared by the
 * runtime's batches that store numerics: kept out of the installed
 * headers, not part of the C API.
 */
#ifndef TESSERA_RUNTIME_DECIMAL_CACHE_H
#define TESSERA_RUNTIME_DECIMAL_CACHE_H

#include "tessera/batch.h"
#include "tessera/kernel_ops.h"
#include "tessera/status.h"

/*
 * Per column, allocated in context at its first request: the values with
 * a decimal's int64 in place of its numeric, the rows read, those that
 * are decimals, and the scale the batch's first decimal fixed (-1 before
 * it). The owner embeds the struct, zeroed, and sets its sizes once.
 */
typedef struct TessDecimalCache
{
	MemoryContext context;
	int			ncolumns;
	int			capacity;
	int			nwords;
	Datum	  **values;
	uint64	  **read;
	uint64	  **rows;
	int		   *scale;
	/* The rows a request reads that the column's decimals do not have yet. */
	uint64	   *pending;
	/* The kernels that read them, found once, and their status. */
	const TessKernelOps *kernels;
	TessStatus	status;
} TessDecimalCache;

/* Set the sizes of an embedded, zeroed cache; allocates nothing yet. */
extern void tess_decimal_cache_init(TessDecimalCache *cache, MemoryContext context,
									int ncolumns, int capacity);

/*
 * Answer a numeric column's requested rows as decimals into result, from
 * the column's numerics (values, isnull) of the batch's current rows:
 * the rows read before keep their answer. Without the kernels result
 * stays as it is, without decimals.
 */
extern void tess_decimal_cache_read(TessDecimalCache *cache, int column,
									const Datum *values, const bool *isnull,
									const TessRowMask *rows, TessDatumColumn *result);

/* Forget every column's decimals, for the batch's next rows. */
extern void tess_decimal_cache_reset(TessDecimalCache *cache);

#endif

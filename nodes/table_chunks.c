/*
 * A hash table's directory and its chunks, as every node keeps them
 * (table_chunks.h).
 */
#include "postgres.h"

#include "tessera/runtime_api.h"
#include "tessera/table.h"

#include "memory_limit.h"
#include "table_chunks.h"

Size
table_chunk_len(const TessKernelOps *kernels, TessStatus *status)
{
	TessSpillShape shape = {
		.share = 8,
		.min_chunk = TABLE_FIRST_CHUNK,
		.max_chunk = TESS_TABLE_MAX_CHUNK_LEN,
		.min_partitions = 1,
		.max_partitions = 1,
	};
	TessSpillLevel level = {.limit = tess_hash_memory_limit()};
	TessSpillPlan plan;

	tess_status_check(kernels->spill_plan(&shape, &level, &plan, status), status);
	return plan.chunk_len;
}

void
table_chunks_limit(uint64 nchunks, const char *table)
{
	if (nchunks > TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("%s cannot hold more than %d chunks", table,
						TESS_TABLE_MAX_CHUNKS)));
}

void
table_chunks_reserve(void ***bases, Size **lens, int *slots, int nchunks,
					 MemoryContext context, const char *table)
{
	int			grown = Max(*slots, 16);

	table_chunks_limit(nchunks, table);
	if (nchunks <= *slots && *bases != NULL)
		return;
	while (grown < nchunks)
		grown *= 2;
	if (*bases == NULL)
	{
		*bases = MemoryContextAlloc(context, sizeof(void *) * grown);
		*lens = MemoryContextAlloc(context, sizeof(Size) * grown);
	}
	else
	{
		*bases = repalloc(*bases, sizeof(void *) * grown);
		*lens = repalloc(*lens, sizeof(Size) * grown);
	}
	*slots = grown;
}

void *
table_chunk_new(const TessKernelOps *kernels, MemoryContext context, Size len,
				TessStatus *status)
{
	void	   *base = MemoryContextAlloc(context, len);

	tess_status_check(kernels->table_chunk_init(base, len, status), status);
	return base;
}

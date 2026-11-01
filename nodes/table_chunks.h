/*
 * A hash table's directory and its chunks, as every node keeps them: the
 * arrays of the chunks' bases and lengths that the table reads, grown by
 * doubling, and a chunk allocated in a memory context and initialized for
 * the table. A table holds at most TESS_TABLE_MAX_CHUNKS chunks: a node
 * fails with SQLSTATE 54000 before its directory would pass them, whether
 * it adds a chunk or adopts chunks it did not allocate. Each node keeps
 * its own bookkeeping beside: its counts of bytes, its partitions, the
 * slots it reserves.
 */
#ifndef TABLE_CHUNKS_H
#define TABLE_CHUNKS_H

#include "postgres.h"

#include "tessera/kernel_ops.h"

/*
 * A table's first chunk, so that a table of a few rows takes little; the
 * others take table_chunk_len.
 */
#define TABLE_FIRST_CHUNK (64 * 1024)

/*
 * The length of a table's chunks past the first: the level plan's for one
 * partition at a share of 8 of hash_mem (tess_spill_chunk_len), between
 * TABLE_FIRST_CHUNK and TESS_TABLE_MAX_CHUNK_LEN, so that a table spills
 * only near its limit.
 */
extern Size table_chunk_len(const TessKernelOps *kernels, TessStatus *status);

/* 54000, naming table, if a table would hold nchunks chunks, past the most. */
extern void table_chunks_limit(uint64 nchunks, const char *table);

/*
 * Room for nchunks chunks in a directory: *bases and *lens of *slots,
 * grown by doubling from 16, allocated in context when NULL, else where
 * they are. Fails as table_chunks_limit first.
 */
extern void table_chunks_reserve(void ***bases, Size **lens, int *slots,
								 int nchunks, MemoryContext context,
								 const char *table);

/* A chunk of len bytes allocated in context and initialized: its base. */
extern void *table_chunk_new(const TessKernelOps *kernels, MemoryContext context,
							 Size len, TessStatus *status);

#endif							/* TABLE_CHUNKS_H */

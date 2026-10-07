/* The limits of memory of the Tessera nodes. */
#ifndef TESSERA_NODES_MEMORY_LIMIT_H
#define TESSERA_NODES_MEMORY_LIMIT_H

#include "postgres.h"

#include "executor/nodeHash.h"
#include "miscadmin.h"

#include "tessera/table.h"

/*
 * hash_mem in bytes, at most what one table of the kernels may take
 * (tess_table_memory_limit): the memory a join or a grouping fills before
 * it sends rows to disk, and the one its planner prices.
 */
static inline Size
tess_hash_memory_limit(void)
{
	return tess_table_memory_limit(get_hash_memory_limit());
}

/* work_mem in bytes, at most what one table may take: a sort's memory. */
static inline Size
tess_work_memory_limit(void)
{
	return tess_table_memory_limit((Size) work_mem * 1024);
}

#endif							/* TESSERA_NODES_MEMORY_LIMIT_H */

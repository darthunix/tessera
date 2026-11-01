/* The limits of memory of the Tessera nodes. */
#ifndef TESSERA_NODES_MEMORY_LIMIT_H
#define TESSERA_NODES_MEMORY_LIMIT_H

#include "postgres.h"

#include "executor/nodeHash.h"
#include "miscadmin.h"
#include "nodes/execnodes.h"
#include "nodes/extensible.h"
#include "nodes/plannodes.h"
#include "optimizer/cost.h"

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

/*
 * The memory of a node while it runs: on a core of the Greenplum family,
 * where its path asked for a share of the query's memory
 * (CUSTOMPATH_GP_MEMORY_INTENSIVE, a fork's patch), the share the resource
 * manager gave its plan; elsewhere hash_mem. At most what one table of the
 * kernels may take, as tess_hash_memory_limit.
 */
static inline Size
tess_node_memory_limit(const PlanState *ps)
{
#if TESS_MPP && defined(CUSTOMPATH_GP_MEMORY_INTENSIVE)
	if (IsA(ps->plan, CustomScan) &&
		(((const CustomScan *) ps->plan)->flags & CUSTOMPATH_GP_MEMORY_INTENSIVE) != 0)
		return tess_table_memory_limit((Size) PlanStateOperatorMemKB(ps) * 1024);
#endif
	return tess_hash_memory_limit();
}

/*
 * The memory the planner prices such a node with, before the resource
 * manager shares the query's memory out: on a core of the family, what
 * the family's planner sizes its own hash join's table with
 * (global_work_mem, planner_work_mem of every segment, against one
 * segment's rows, as cost_hashjoin does); elsewhere hash_mem.
 */
static inline Size
tess_planned_memory_limit(PlannerInfo *root)
{
#if TESS_MPP
	return tess_table_memory_limit((Size) Min(global_work_mem(root), (double) SIZE_MAX / 2));
#else
	(void) root;
	return tess_hash_memory_limit();
#endif
}

/* work_mem in bytes, at most what one table may take: a sort's memory. */
static inline Size
tess_work_memory_limit(void)
{
	return tess_table_memory_limit((Size) work_mem * 1024);
}

#endif							/* TESSERA_NODES_MEMORY_LIMIT_H */

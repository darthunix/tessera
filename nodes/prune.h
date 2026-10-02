/* A hash join's keys pruning the partitions of TessAppend below it. */
#ifndef TESSERA_NODES_PRUNE_H
#define TESSERA_NODES_PRUNE_H

#include "nodes/execnodes.h"
#include "nodes/plannodes.h"

/*
 * A hash join's keys of the column that prunes its outer side's partitions,
 * once built (hashjoin.c): the inner rows with a key, whether its words are
 * 8 bytes, the lowest and the highest, and the keys themselves while few
 * (nvalues, -1 past them).
 */
typedef struct TessJoinKeys
{
	uint64		rows;
	bool		int8;
	int64		min;
	int64		max;
	int			nvalues;
	int64	   *values;
} TessJoinKeys;
/* TessAppend's pruning by a hash join's keys: see append.c. */
extern bool tess_append_join_prune_begin(PlanState *node, const PartitionPruneInfo *values,
										 const PartitionPruneInfo *range, const int *params);
extern void tess_append_join_prune(PlanState *node, const TessJoinKeys *keys);

#endif							/* TESSERA_NODES_PRUNE_H */

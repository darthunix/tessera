/*
 * The parts of the node module Greengage 7 does not build yet: Tessera's
 * own heap scan, the gather nodes of PostgreSQL parallel queries, the
 * append node and the hash join planner. Their entry points refuse, so
 * the core's scan under TessPack reads every table, heap or append-only,
 * and joins and appends stay the core's. The nodes are not registered;
 * their descriptions exist for the planners that compare paths with them.
 * shortcut: stand-ins until the steps of the greengage-port roadmap entry
 * port each part.
 */
#include "postgres.h"

#include "internal.h"
#include "costs.h"
#include "prune.h"
#include "registry.h"

#if TESS_GREENGAGE

const TessNode tess_heap_scan_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_HEAP_SCAN_NODE_NAME,
};

const TessNode tess_append_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_APPEND_NODE_NAME,
};

/* No parallel query on Greengage: one participant. */
double
tess_parallel_divisor(const Path *path)
{
	return 1.0;
}

bool
tess_bitmap_only_brin(Path *bitmapqual)
{
	return false;
}

double
tess_index_correlation(PlannerInfo *root, IndexOptInfo *index)
{
	return 0;
}

Path *
tess_heap_bitmap_path(PlannerInfo *root, BitmapHeapPath *bitmap, PathTarget *target)
{
	return NULL;
}

Path *
tess_heap_index_path(PlannerInfo *root, IndexPath *index, PathTarget *target)
{
	return NULL;
}

void
tess_gather_planner_init(void)
{
}

void
tess_gather_add_paths(PlannerInfo *root, RelOptInfo *rel)
{
}

Path *
tess_gather_path(PlannerInfo *root, RelOptInfo *rel, Path *subpath)
{
	return NULL;
}

Path *
tess_gather_merge_path(PlannerInfo *root, RelOptInfo *rel, Path *sorted,
					   PathTarget *target)
{
	return NULL;
}

void
tess_hash_join_planner_init(void)
{
}

/* No TessAppend stands under a join on Greengage: nothing to prune. */
bool
tess_append_join_prune_begin(PlanState *node, const PartitionPruneInfo *values,
							 const PartitionPruneInfo *range, const int *params)
{
	return false;
}

void
tess_append_join_prune(PlanState *node, const TessJoinKeys *keys)
{
}

#endif							/* TESS_GREENGAGE */

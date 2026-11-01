/*
 * The parts of the node module Greengage 7 does not build yet: the gather
 * nodes of PostgreSQL parallel queries, the append node and the hash join
 * planner. Their entry points refuse, so joins and appends stay the
 * core's. The nodes are not registered;
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

const TessNode tess_append_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_APPEND_NODE_NAME,
};

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

/*
 * The entry points of the parts of the node module a core of the
 * Greenplum family does not build: the gather nodes of PostgreSQL
 * parallel query, which the family runs none of, and, not ported yet,
 * the hash join planner, so that joins stay the core's. The nodes are
 * not registered.
 * shortcut: the join planner until it is ported.
 */
#include "postgres.h"

#include "internal.h"
#include "costs.h"
#include "prune.h"
#include "registry.h"

#if TESS_MPP

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

#endif							/* TESS_MPP */

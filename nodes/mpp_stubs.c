/*
 * The entry points of the parts of the node module a core of the
 * Greenplum family does not build: the gather nodes of PostgreSQL
 * parallel query, which the family runs none of. The nodes are not
 * registered.
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

#endif							/* TESS_MPP */

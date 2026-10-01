/*
 * A module of TessLimit's sources alone, built outside the tree against
 * the installed headers and runtime library, as a node of another
 * extension would be; make installcheck builds it and loads it never.
 */
#include "postgres.h"

#include "fmgr.h"

#include "tessera/runtime.h"

#include "limit.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

void
_PG_init(void)
{
	tess_runtime_api()->nodes->add(&tess_limit_node);
	tess_limit_planner_init();
}

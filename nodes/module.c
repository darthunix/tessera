#include "postgres.h"

#include "fmgr.h"

#include "tessera/runtime.h"

#include "internal.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

/*
 * The module registers its node kinds and scan methods. The pack and heap
 * scan nodes are created by batch parents and need no hook; the filter
 * node offers its path to base relations through the set_rel_pathlist
 * hook; the aggregate node is registered for the planner side that
 * follows.
 */
void
_PG_init(void)
{
	/* Raises ERROR without the bridge: CREATE EXTENSION tessera first. */
	const TessApi *api = tess_runtime_api();

	RegisterCustomScanMethods(&tess_pack_scan_methods);
	api->nodes->add(&tess_pack_node);
	RegisterCustomScanMethods(&tess_heap_scan_scan_methods);
	api->nodes->add(&tess_heap_scan_node);
	RegisterCustomScanMethods(&tess_filter_scan_methods);
	api->nodes->add(&tess_filter_node);
	tess_filter_planner_init();
	RegisterCustomScanMethods(&tess_agg_scan_methods);
	api->nodes->add(&tess_agg_node);
}

#include "postgres.h"

#include "fmgr.h"

#include "tessera/runtime.h"

#include "internal.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

/*
 * The module registers its node kinds and scan methods. The pack node is
 * created by batch parents and needs no hook; the filter node offers its
 * path to base relations through the set_rel_pathlist hook.
 */
void
_PG_init(void)
{
	/* Raises ERROR without the bridge: CREATE EXTENSION tessera first. */
	const TessApi *api = tess_runtime_api();

	RegisterCustomScanMethods(&tess_pack_scan_methods);
	api->nodes->add(&tess_pack_node);
	RegisterCustomScanMethods(&tess_filter_scan_methods);
	api->nodes->add(&tess_filter_node);
	tess_filter_planner_init();
}

#include "postgres.h"

#include "fmgr.h"

#include "tessera/runtime.h"

#include "internal.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

/*
 * The module registers its node kinds and scan methods; it installs no
 * planner hook, since the pack node is created by batch parents.
 */
void
_PG_init(void)
{
	/* Raises ERROR without the bridge: CREATE EXTENSION tessera first. */
	const TessApi *api = tess_runtime_api();

	RegisterCustomScanMethods(&tess_pack_scan_methods);
	api->nodes->add(&tess_pack_node);
}

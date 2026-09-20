#include "postgres.h"

#include "fmgr.h"

#include "tessera/runtime.h"

#include "internal.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

const TessNode tess_limit_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_LIMIT_NODE_NAME,
};

/*
 * TessLimit is built against the public headers and the runtime library
 * alone, as a node of another extension would be: it registers its kind
 * with the bridge and installs its planner hook. It defines no setting of
 * its own; tessera.enable turns it off with every other batch node.
 */
void
_PG_init(void)
{
	/* Raises ERROR without the bridge: CREATE EXTENSION tessera first. */
	const TessApi *api = tess_runtime_api();

	api->nodes->add(&tess_limit_node);
	tess_limit_planner_init();
}

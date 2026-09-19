#include "postgres.h"

#include "fmgr.h"
#include "utils/guc.h"

#include "tessera/bridge.h"

#include "internal.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

static bool tess_enable = true;

static const TessSettings tess_settings = {
	TESS_ABI_INITIALIZER(TESS_SETTINGS_ABI_VERSION, TessSettings),
	.enable = &tess_enable,
};

static const TessApi tess_api = {
	TESS_ABI_INITIALIZER(TESS_API_ABI_VERSION, TessApi),
	.binding_ops = &tess_binding_ops,
	.sources = &tess_source_registry_ops,
	.nodes = &tess_node_registry_ops,
	.functions = &tess_function_registry_ops,
	.settings = &tess_settings,
};

void
_PG_init(void)
{
	void	  **rendezvous;

	DefineCustomBoolVariable("tessera.enable",
							 "Adds Tessera batch paths to query plans.",
							 NULL, &tess_enable, true, PGC_USERSET, 0,
							 NULL, NULL, NULL);
	MarkGUCPrefixReserved("tessera");
	rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	if (*rendezvous != NULL && *rendezvous != &tess_api)
		elog(ERROR, "Tessera API rendezvous variable is already in use");
	*rendezvous = (void *) &tess_api;
}

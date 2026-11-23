#include "postgres.h"

#include "fmgr.h"
#include "utils/guc.h"

#include "tessera/bridge.h"

#include "internal.h"

PG_MODULE_MAGIC_EXT(.name = "tessera", .version = TESS_VERSION);

PGDLLEXPORT void _PG_init(void);

static bool tess_enable = true;

static const TessSettings tess_settings = {
	TESS_ABI_INITIALIZER(TESS_SETTINGS_ABI_VERSION, TessSettings),
	.enable = &tess_enable,
};

/* The identities of the bytes form's dictionaries given in this backend. */
static uint64 tess_dictionary_identities = 0;

static uint64
tess_next_dictionary_identity(void)
{
	if (tess_dictionary_identities == PG_UINT64_MAX)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("no identity of a dictionary is left in this backend")));
	return ++tess_dictionary_identities;
}

static const TessApi tess_api = {
	TESS_ABI_INITIALIZER(TESS_API_ABI_VERSION, TessApi),
	.binding_ops = &tess_binding_ops,
	.sources = &tess_source_registry_ops,
	.nodes = &tess_node_registry_ops,
	.functions = &tess_function_registry_ops,
	.settings = &tess_settings,
	.kernels = &tess_kernel_registry_ops,
	.next_dictionary_identity = tess_next_dictionary_identity,
};

void
_PG_init(void)
{
	void	  **rendezvous;

	/*
	 * The prefix is reserved by tessera_nodes, once it has defined the other
	 * tessera.* settings: reserved here, before they exist, it would drop
	 * the values given for them earlier, in postgresql.conf, ALTER SYSTEM,
	 * ALTER ROLE or a SET before the modules load, as unknown placeholders.
	 */
	DefineCustomBoolVariable("tessera.enable",
							 "Adds Tessera batch paths to query plans.",
							 NULL, &tess_enable, true, PGC_USERSET, GUC_EXPLAIN,
							 NULL, NULL, NULL);
	rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	if (*rendezvous != NULL && *rendezvous != &tess_api)
		elog(ERROR, "Tessera API rendezvous variable is already in use");
	*rendezvous = (void *) &tess_api;
}

#include "postgres.h"

#include "fmgr.h"

#include "tessera/bridge.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

static const TessApi tess_api = {
	.abi_version = TESS_API_ABI_VERSION,
	.struct_size = sizeof(TessApi),
};

void
_PG_init(void)
{
	void	  **rendezvous;

	rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	if (*rendezvous != NULL && *rendezvous != &tess_api)
		elog(ERROR, "Tessera API rendezvous variable is already in use");
	*rendezvous = (void *) &tess_api;
}

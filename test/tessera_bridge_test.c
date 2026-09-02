#include "postgres.h"

#include "fmgr.h"

#include "tessera/bridge.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_api_visible);

Datum
tessera_test_api_visible(PG_FUNCTION_ARGS)
{
	const TessApi *api;
	void	  **rendezvous;

	rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	api = *rendezvous;

	PG_RETURN_BOOL(api != NULL &&
					   api->abi_version == TESS_API_ABI_VERSION &&
					   api->struct_size == sizeof(TessApi));
}

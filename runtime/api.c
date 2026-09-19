#include "postgres.h"

#include "fmgr.h"

#include "tessera/runtime.h"

/* The validated API, found once per backend. */
static const TessApi *cached_api = NULL;

const TessApi *
tess_runtime_api(void)
{
	const TessApi *api;
	void	  **rendezvous;

	if (cached_api != NULL)
		return cached_api;
	rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	api = *rendezvous;
	if (api == NULL)
		ereport(ERROR,
				(errcode(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE),
				 errmsg("Tessera bridge is not loaded"),
				 errhint("Create the tessera extension first.")));
	if (api->abi_version != TESS_API_ABI_VERSION ||
		api->struct_size < TESS_API_MIN_SIZE)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("incompatible Tessera API")));
	if (api->binding_ops == NULL ||
		api->binding_ops->abi_version != TESS_BINDING_OPS_ABI_VERSION ||
		api->binding_ops->struct_size < TESS_BINDING_OPS_MIN_SIZE)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("incompatible Tessera binding operations")));
	if (api->settings == NULL ||
		api->settings->abi_version != TESS_SETTINGS_ABI_VERSION ||
		api->settings->struct_size < TESS_SETTINGS_MIN_SIZE ||
		api->settings->enable == NULL)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("incompatible Tessera settings")));
	cached_api = api;
	return api;
}

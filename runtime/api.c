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

const TessKernelOps *
tess_runtime_kernels(void)
{
	const TessApi *api = tess_runtime_api();
	const TessKernelRegistryOps *registry;
	const TessKernelOps *ops;

	if (!TESS_ABI_HAS_FIELD(api, TessApi, kernels) || api->kernels == NULL)
		return NULL;
	registry = api->kernels;
	if (registry->abi_version != TESS_KERNEL_REGISTRY_OPS_ABI_VERSION ||
		registry->struct_size < TESS_KERNEL_REGISTRY_OPS_MIN_SIZE)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("incompatible Tessera kernel registry")));
	ops = registry->get();
	if (ops == NULL)
		return NULL;
	if (ops->abi_version != TESS_KERNEL_OPS_ABI_VERSION ||
		ops->struct_size < TESS_KERNEL_OPS_MIN_SIZE ||
		ops->table_format_version != TESS_TABLE_FORMAT_VERSION)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("incompatible Tessera kernels"),
				 errdetail("Expected version %u, at least %zu bytes and "
						   "table format %u, got version %u, %zu bytes and "
						   "table format %u.",
						   TESS_KERNEL_OPS_ABI_VERSION, TESS_KERNEL_OPS_MIN_SIZE,
						   TESS_TABLE_FORMAT_VERSION, ops->abi_version,
						   ops->struct_size, ops->table_format_version)));
	return ops;
}

void
tess_status_report(const TessStatus *status)
{
	const char *sqlstate = status->sqlstate;

	ereport(ERROR,
			(errcode(MAKE_SQLSTATE(sqlstate[0], sqlstate[1], sqlstate[2],
								   sqlstate[3], sqlstate[4])),
			 errmsg("%s", status->message)));
	pg_unreachable();
}

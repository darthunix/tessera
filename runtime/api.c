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
				 errhint("Load the tessera library first: LOAD 'tessera', or list tessera first in shared_preload_libraries or session_preload_libraries.")));
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

/* Whether a status is a failure with a five-character SQLSTATE of [0-9A-Z]. */
static bool
status_valid(const TessStatus *status)
{
	if (status->code == TESS_OK)
		return false;
	for (int i = 0; i < 5; i++)
	{
		char		c = status->sqlstate[i];

		if (!((c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z')))
			return false;
	}
	return true;
}

/*
 * A status that is not a failure with a valid SQLSTATE, a caller's or a
 * kernel's mistake, still raises an ERROR, XX000, rather than a code made of
 * whatever the status holds.
 */
void
tess_status_report(const TessStatus *status)
{
	const char *sqlstate = status->sqlstate;

	if (!status_valid(status))
		ereport(ERROR,
				(errcode(ERRCODE_INTERNAL_ERROR),
				 errmsg("Tessera call failed without a valid status")));
	ereport(ERROR,
			(errcode(MAKE_SQLSTATE(sqlstate[0], sqlstate[1], sqlstate[2],
								   sqlstate[3], sqlstate[4])),
			 errmsg("%.*s", TESS_STATUS_MESSAGE_SIZE, status->message)));
	pg_unreachable();
}

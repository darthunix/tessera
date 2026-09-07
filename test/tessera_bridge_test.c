#include "postgres.h"

#include "fmgr.h"

#include "tessera/bridge.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_api_visible);
PG_FUNCTION_INFO_V1(tessera_test_abi_helpers);

typedef struct TessTestOps
{
	uint32		abi_version;
	Size		struct_size;
	void		(*required) (void);
	void		(*optional) (void);
} TessTestOps;

typedef struct TessTestValue
{
	Size		struct_size;
	int		value;
} TessTestValue;

typedef struct ExtendedApi
{
	TessApi		base;
	void	   *future_field;
} ExtendedApi;

Datum
tessera_test_api_visible(PG_FUNCTION_ARGS)
{
	const TessApi *api;
	void	  **rendezvous;

	rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	api = *rendezvous;

	PG_RETURN_BOOL(api != NULL &&
					   api->abi_version == TESS_API_ABI_VERSION &&
					   api->struct_size >= TESS_API_MIN_SIZE &&
					   api->binding_ops != NULL &&
					   api->binding_ops->abi_version ==
					   TESS_BINDING_OPS_ABI_VERSION &&
					   api->binding_ops->struct_size >=
					   TESS_BINDING_OPS_MIN_SIZE &&
					   api->sources != NULL &&
					   api->sources->abi_version ==
					   TESS_SOURCE_REGISTRY_OPS_ABI_VERSION &&
					   api->sources->struct_size >=
					   TESS_SOURCE_REGISTRY_OPS_MIN_SIZE);
}

Datum
tessera_test_abi_helpers(PG_FUNCTION_ARGS)
{
	TessTestOps ops = {
		TESS_ABI_INITIALIZER(7, TessTestOps),
	};
	TessTestValue value = TESS_STRUCT_INITIALIZER(TessTestValue);
	TessApi		api = {
		TESS_ABI_INITIALIZER(TESS_API_ABI_VERSION, TessApi),
	};
	ExtendedApi extended = {
		.base = api,
	};

	if (ops.abi_version != 7 || ops.struct_size != sizeof(ops) ||
		value.struct_size != sizeof(value))
		PG_RETURN_BOOL(false);

	/* The current root requires sources, but still accepts later fields. */
	api.struct_size = TESS_ABI_SIZE_INCLUDING_FIELD(TessApi, binding_ops);
	if (api.struct_size >= TESS_API_MIN_SIZE)
		PG_RETURN_BOOL(false);
	api.struct_size = TESS_ABI_SIZE_INCLUDING_FIELD(TessApi, sources);
	extended.base.struct_size = sizeof(extended);
	if (api.struct_size < TESS_API_MIN_SIZE ||
		extended.base.struct_size < TESS_API_MIN_SIZE)
		PG_RETURN_BOOL(false);

	ops.struct_size = TESS_ABI_SIZE_INCLUDING_FIELD(TessTestOps, required);
	PG_RETURN_BOOL(TESS_ABI_HAS_FIELD(&ops, TessTestOps, required) &&
					   !TESS_ABI_HAS_FIELD(&ops, TessTestOps, optional));
}

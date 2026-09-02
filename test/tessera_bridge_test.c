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

Datum
tessera_test_api_visible(PG_FUNCTION_ARGS)
{
	const TessApi *api;
	void	  **rendezvous;

	rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	api = *rendezvous;

	PG_RETURN_BOOL(api != NULL &&
					   api->abi_version == TESS_API_ABI_VERSION &&
					   api->struct_size >= TESS_API_MIN_SIZE);
}

Datum
tessera_test_abi_helpers(PG_FUNCTION_ARGS)
{
	TessTestOps ops = {
		TESS_ABI_INITIALIZER(7, TessTestOps),
	};
	TessTestValue value = TESS_STRUCT_INITIALIZER(TessTestValue);

	if (ops.abi_version != 7 || ops.struct_size != sizeof(ops) ||
		value.struct_size != sizeof(value))
		PG_RETURN_BOOL(false);

	ops.struct_size = TESS_ABI_SIZE_THROUGH(TessTestOps, required);
	PG_RETURN_BOOL(TESS_ABI_HAS_FIELD(&ops, TessTestOps, required) &&
					   !TESS_ABI_HAS_FIELD(&ops, TessTestOps, optional));
}

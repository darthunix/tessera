#include "postgres.h"

#include "fmgr.h"

#include "tessera/bridge.h"
#include "tessera/kernel_ops.h"
#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_kernel_registry);
PG_FUNCTION_INFO_V1(tessera_test_invalid_kernels);
PG_FUNCTION_INFO_V1(tessera_test_second_kernels);
PG_FUNCTION_INFO_V1(tessera_test_foreign_table_format);
PG_FUNCTION_INFO_V1(tessera_test_status_report);

/* The entry points of the kernels this module links, as a module installs them. */
static const TessKernelOps kernels = {
	TESS_ABI_INITIALIZER(TESS_KERNEL_OPS_ABI_VERSION, TessKernelOps),
	.table_format_version = TESS_TABLE_FORMAT_VERSION,
	.int4_hash = tess_int4_hash,
	.int4_hash_next = tess_int4_hash_next,
	.int8_hash = tess_int8_hash,
	.int8_hash_next = tess_int8_hash_next,
	.table_size = tess_table_size,
	.table_create = tess_table_create,
	.table_stats = tess_table_stats,
	.table_insert = tess_table_insert,
	.table_probe = tess_table_probe,
	.table_next_match = tess_table_next_match,
	.table_gather = tess_table_gather,
	.table_record = tess_table_record,
	.table_find_or_insert = tess_table_find_or_insert,
	.table_payload = tess_table_payload,
	.table_scan = tess_table_scan,
	.table_grow = tess_table_grow,
	.table_insert_grouped = tess_table_insert_grouped,
	.table_next_in_group = tess_table_next_in_group,
	.table_accumulate = tess_table_accumulate,
	.table_gather_key = tess_table_gather_key,
};

static const TessKernelRegistryOps *
registry(void)
{
	const TessApi *api = tess_runtime_api();

	if (!TESS_ABI_HAS_FIELD(api, TessApi, kernels) || api->kernels == NULL ||
		api->kernels->abi_version != TESS_KERNEL_REGISTRY_OPS_ABI_VERSION ||
		api->kernels->struct_size < TESS_KERNEL_REGISTRY_OPS_MIN_SIZE)
		elog(ERROR, "Tessera test could not find a compatible kernel registry");
	return api->kernels;
}

/*
 * Nothing is installed at first; the same table installs twice and is
 * found by the registry and the runtime; clearing another table or NULL
 * changes nothing, clearing this one empties the registry.
 */
Datum
tessera_test_kernel_registry(PG_FUNCTION_ARGS)
{
	const TessKernelRegistryOps *ops = registry();
	TessKernelOps other = kernels;
	bool		result;

	result = ops->get() == NULL && tess_runtime_kernels() == NULL;
	ops->set(&kernels);
	ops->set(&kernels);
	result = result && ops->get() == &kernels &&
		tess_runtime_kernels() == &kernels;
	ops->clear(NULL);
	ops->clear(&other);
	result = result && ops->get() == &kernels;
	ops->clear(&kernels);
	ops->clear(&kernels);
	result = result && ops->get() == NULL && tess_runtime_kernels() == NULL;

	/* Another table installs once the first is cleared. */
	ops->set(&other);
	result = result && tess_runtime_kernels() == &other;
	ops->clear(&other);
	PG_RETURN_BOOL(result && ops->get() == NULL);
}

/* Install a table with one thing wrong; every case is an ERROR. */
Datum
tessera_test_invalid_kernels(PG_FUNCTION_ARGS)
{
	const TessKernelRegistryOps *ops = registry();
	TessKernelOps invalid = kernels;
	int32		kind = PG_GETARG_INT32(0);

	if (kind == 0)
		ops->set(NULL);
	else if (kind == 1)
		invalid.abi_version = TESS_KERNEL_OPS_ABI_VERSION + 1;
	else if (kind == 2)
		invalid.struct_size = TESS_KERNEL_OPS_MIN_SIZE - 1;
	else
		invalid.table_gather = NULL;
	ops->set(&invalid);
	ops->clear(&invalid);
	elog(ERROR, "Tessera test installed invalid kernels");
	PG_RETURN_VOID();
}

/* A second table while one is installed is refused. */
Datum
tessera_test_second_kernels(PG_FUNCTION_ARGS)
{
	const TessKernelRegistryOps *ops = registry();
	TessKernelOps second = kernels;

	ops->set(&kernels);
	PG_TRY();
	{
		ops->set(&second);
	}
	PG_FINALLY();
	{
		ops->clear(&kernels);
	}
	PG_END_TRY();
	elog(ERROR, "Tessera test installed a second table of kernels");
	PG_RETURN_VOID();
}

/*
 * The registry accepts a table built for another region format, since
 * only its users know the format they need: the runtime refuses it.
 */
Datum
tessera_test_foreign_table_format(PG_FUNCTION_ARGS)
{
	const TessKernelRegistryOps *ops = registry();
	TessKernelOps foreign = kernels;

	foreign.table_format_version = TESS_TABLE_FORMAT_VERSION + 1;
	ops->set(&foreign);
	PG_TRY();
	{
		(void) tess_runtime_kernels();
	}
	PG_FINALLY();
	{
		ops->clear(&foreign);
	}
	PG_END_TRY();
	elog(ERROR, "Tessera test accepted a foreign table format");
	PG_RETURN_VOID();
}

/*
 * A failed kernel call through the table, reported with the SQLSTATE and
 * message it stored (0), and a status naming another SQLSTATE (1).
 */
Datum
tessera_test_status_report(PG_FUNCTION_ARGS)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	TessTableKeyKind kind = TESS_TABLE_KEY_INT4;
	Size		size;

	if (PG_GETARG_INT32(0) == 0)
	{
		if (kernels.table_size(0, &kind, 8, 10, &size, &status) == TESS_OK)
			elog(ERROR, "Tessera test sized a table without keys");
	}
	else
	{
		status.code = TESS_ERROR_DIVISION_BY_ZERO;
		strlcpy(status.sqlstate, "22012", sizeof(status.sqlstate));
		strlcpy(status.message, "division by zero", sizeof(status.message));
	}
	tess_status_report(&status);
}

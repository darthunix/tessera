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
	.table_chunk_init = tess_table_chunk_init,
	.table_append = tess_table_append,
	.table_link = tess_table_link,
	.table_link_grouped = tess_table_link_grouped,
	.table_probe = tess_table_probe,
	.table_next_match = tess_table_next_match,
	.table_gather = tess_table_gather,
	.table_record = tess_table_record,
	.table_find_or_insert = tess_table_find_or_insert,
	.table_payload = tess_table_payload,
	.table_payloads = tess_table_payloads,
	.table_scan = tess_table_scan,
	.table_regrow = tess_table_regrow,
	.table_next_in_group = tess_table_next_in_group,
	.table_accumulate = tess_table_accumulate,
	.table_gather_key = tess_table_gather_key,
	.table_bloom_words = tess_table_bloom_words,
	.table_bloom = tess_table_bloom,
	.bloom_probe = tess_bloom_probe,
	.bloom_shared_words = tess_bloom_shared_words,
	.bloom_shared_init = tess_bloom_shared_init,
	.table_try_build_bloom = tess_table_try_build_bloom,
	.bloom_shared_ready = tess_bloom_shared_ready,
	.bloom_shared_probe = tess_bloom_shared_probe,
	.build_counters_init = tess_build_counters_init,
	.build_report = tess_build_report,
	.build_take_chunk = tess_build_take_chunk,
	.build_add_duplicates = tess_build_add_duplicates,
	.build_totals = tess_build_totals,
	.build_step = tess_build_step,
	.table_fingerprint = tess_table_fingerprint,
	.spill_header_write = tess_spill_header_write,
	.spill_header_read = tess_spill_header_read,
	.spill_pack = tess_spill_pack,
	.spill_unpack = tess_spill_unpack,
	.table_append_partitioned = tess_table_append_partitioned,
	.table_split = tess_table_split,
	.bloom_add = tess_bloom_add,
	.table_find_or_insert_partitioned = tess_table_find_or_insert_partitioned,
	.table_combine = tess_table_combine,
	.table_spill_words = tess_table_spill_words,
	.table_spill_init = tess_table_spill_init,
	.table_spill_split = tess_table_spill_split,
	.table_spill_partitions = tess_table_spill_partitions,
	.table_spill_add_bytes = tess_table_spill_add_bytes,
	.table_spill_evict = tess_table_spill_evict,
	.table_spill_flags = tess_table_spill_flags,
	.table_spill_records = tess_table_spill_records,
	.table_spill_start = tess_table_spill_start,
	.table_spill_take_file = tess_table_spill_take_file,
	.table_spill_take_alone = tess_table_spill_take_alone,
	.round_step = tess_round_step,
	.bloom_shared_add = tess_bloom_shared_add,
	.table_spill_evictions = tess_table_spill_evictions,
	.sort_item_words = tess_sort_item_words,
	.sort_items = tess_sort_items,
	.sort = tess_sort,
	.table_append_columns = tess_table_append_columns,
	.table_gather_scattered = tess_table_gather_scattered,
	.sort_top_candidates = tess_sort_top_candidates,
	.sort_top_push = tess_sort_top_push,
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
 * The registry accepts a table built for another table format, since
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

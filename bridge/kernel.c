#include "postgres.h"

#include "internal.h"

/* The installed table of the kernels, borrowed from its provider. */
static const TessKernelOps *installed = NULL;

static void set_kernels(const TessKernelOps *ops);
static void clear_kernels(const TessKernelOps *ops);
static const TessKernelOps *get_kernels(void);

const TessKernelRegistryOps tess_kernel_registry_ops = {
	TESS_ABI_INITIALIZER(TESS_KERNEL_REGISTRY_OPS_ABI_VERSION,
		TessKernelRegistryOps),
	.set = set_kernels,
	.clear = clear_kernels,
	.get = get_kernels,
};

static void
validate_kernels(const TessKernelOps *ops)
{
	if (ops == NULL)
		elog(ERROR, "Tessera cannot install null kernels");
	if (ops->abi_version != TESS_KERNEL_OPS_ABI_VERSION ||
		ops->struct_size < TESS_KERNEL_OPS_MIN_SIZE)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("incompatible Tessera kernels ABI"),
				 errdetail("Expected version %u and at least %zu bytes, "
						   "got version %u and %zu bytes.",
						   TESS_KERNEL_OPS_ABI_VERSION, TESS_KERNEL_OPS_MIN_SIZE,
						   ops->abi_version, ops->struct_size)));
	if (ops->int4_hash == NULL || ops->int4_hash_next == NULL ||
		ops->int8_hash == NULL || ops->int8_hash_next == NULL ||
		ops->table_size == NULL || ops->table_create == NULL ||
		ops->table_stats == NULL || ops->table_chunk_init == NULL ||
		ops->table_append == NULL || ops->table_link == NULL ||
		ops->table_link_grouped == NULL || ops->table_probe == NULL ||
		ops->table_next_match == NULL || ops->table_gather == NULL ||
		ops->table_record == NULL || ops->table_find_or_insert == NULL ||
		ops->table_payload == NULL || ops->table_payloads == NULL ||
		ops->table_scan == NULL ||
		ops->table_regrow == NULL || ops->table_next_in_group == NULL ||
		ops->table_accumulate == NULL || ops->table_gather_key == NULL ||
		ops->table_bloom_words == NULL || ops->table_bloom == NULL ||
		ops->bloom_probe == NULL || ops->bloom_shared_words == NULL ||
		ops->bloom_shared_init == NULL || ops->table_try_build_bloom == NULL ||
		ops->bloom_shared_ready == NULL || ops->bloom_shared_probe == NULL ||
		ops->build_counters_init == NULL || ops->build_report == NULL ||
		ops->build_take_chunk == NULL || ops->build_add_duplicates == NULL ||
		ops->build_totals == NULL ||
		ops->build_step == NULL || ops->table_append_partitioned == NULL ||
		ops->table_split == NULL || ops->bloom_add == NULL ||
		ops->table_find_or_insert_partitioned == NULL ||
		ops->table_combine == NULL || ops->table_spill_words == NULL ||
		ops->table_spill_init == NULL || ops->table_spill_split == NULL ||
		ops->table_spill_partitions == NULL || ops->table_spill_add_bytes == NULL ||
		ops->table_spill_evict == NULL || ops->table_spill_flags == NULL ||
		ops->table_spill_records == NULL || ops->table_spill_start == NULL ||
		ops->table_spill_take_file == NULL || ops->table_spill_take_alone == NULL ||
		ops->round_step == NULL || ops->bloom_shared_add == NULL ||
		ops->table_spill_evictions == NULL ||
		ops->table_fingerprint == NULL ||
		ops->spill_header_write == NULL || ops->spill_header_read == NULL ||
		ops->spill_pack == NULL || ops->spill_unpack == NULL ||
		ops->sort_item_words == NULL || ops->sort_items == NULL ||
		ops->sort == NULL || ops->table_append_columns == NULL ||
		ops->table_gather_scattered == NULL ||
		ops->sort_top_candidates == NULL || ops->sort_top_push == NULL ||
		ops->table_append_partitioned_columns == NULL ||
		ops->spill_columns_init == NULL ||
		ops->spill_columns_append_partitioned == NULL ||
		ops->spill_columns_pack == NULL || ops->spill_columns_unpack == NULL)
		elog(ERROR, "Tessera kernels must provide every operation");
}

static void
set_kernels(const TessKernelOps *ops)
{
	validate_kernels(ops);
	if (installed == ops)
		return;
	if (installed != NULL)
		ereport(ERROR,
				(errcode(ERRCODE_DUPLICATE_OBJECT),
				 errmsg("Tessera kernels are already installed")));
	installed = ops;
}

static void
clear_kernels(const TessKernelOps *ops)
{
	if (ops != NULL && installed == ops)
		installed = NULL;
}

static const TessKernelOps *
get_kernels(void)
{
	return installed;
}

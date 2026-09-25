/*
 * The Rust kernels as a table of operations: the key hashes and the hash
 * table, for node modules that call them through the bridge's kernel
 * registry instead of linking the library. _PG_init installs the table.
 */
#include "postgres.h"

#include "internal.h"

const TessKernelOps tess_kernel_ops = {
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

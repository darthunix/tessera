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
#define TESS_KERNEL_OP_ENTRY(name) .name = tess_##name,
	TESS_KERNEL_OPS(TESS_KERNEL_OP_ENTRY)
#undef TESS_KERNEL_OP_ENTRY
};

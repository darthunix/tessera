/*
 * The Rust kernels as a table of operations, for modules that do not link
 * them.
 *
 * Only the tessera_kernels module links the static library of the kernels.
 * It publishes the entry points other modules need, such as the key hashes
 * and the hash table a join or grouping node keeps its state in, as a
 * TessKernelOps through the bridge's kernel registry; a node reaches them
 * through tess_runtime_kernels() and calls them by pointer. Every
 * operation has the prototype, arguments and contract of the entry point
 * named in its comment (tessera/kernels.h, tessera/decimal.h,
 * tessera/table.h, tessera/spill.h, tessera/sort.h). See
 * docs/bridge.md.
 */
#ifndef TESSERA_KERNEL_OPS_H
#define TESSERA_KERNEL_OPS_H

#include "postgres.h"

#include "tessera/abi.h"
#include "tessera/decimal.h"
#include "tessera/kernel_ops_list.h"
#include "tessera/kernels.h"
#include "tessera/sort.h"
#include "tessera/spill.h"
#include "tessera/table.h"

#define TESS_KERNEL_OPS_ABI_VERSION 0
#define TESS_KERNEL_REGISTRY_OPS_ABI_VERSION 0

/*
 * The operations of one build of the kernels. The provider owns the table
 * and keeps it valid and unchanged from installation until it clears it.
 *
 * The table is the contract of one build: the module that links the
 * kernels and the runtime and node modules that call them come from one
 * source tree. TESS_KERNEL_OPS_MIN_SIZE reaches the last field and the
 * bridge requires every operation, so a caller reads any field without
 * TESS_ABI_HAS_FIELD; a new operation is a new last field, the minimum
 * size moves to it, and the bridge's check of the operations gets it.
 * (A caller handed a table of its own, a test's, checks the operations it
 * calls for NULL.)
 */
typedef struct TessKernelOps
{
	uint32		abi_version;
	Size		struct_size;
	/* TESS_TABLE_FORMAT_VERSION of the kernels' build. */
	uint32		table_format_version;

	/* The operations, a pointer to entry point tess_<name> each. */
#define TESS_KERNEL_OP_FIELD(name) __typeof__(tess_##name) *name;
	TESS_KERNEL_OPS(TESS_KERNEL_OP_FIELD)
#undef TESS_KERNEL_OP_FIELD
} TessKernelOps;

#define TESS_KERNEL_OPS_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessKernelOps, table_accumulate_extremes)

/* The operations' count, and the last of them is the last field. */
#define TESS_KERNEL_OP_COUNT(name) + 1
#define TESS_KERNEL_OPS_COUNT (0 TESS_KERNEL_OPS(TESS_KERNEL_OP_COUNT))
StaticAssertDecl(sizeof(TessKernelOps) ==
				 offsetof(TessKernelOps, int4_hash) + TESS_KERNEL_OPS_COUNT * sizeof(void (*) (void)),
				 "every field after the versions is an operation");
StaticAssertDecl(sizeof(TessKernelOps) == TESS_KERNEL_OPS_MIN_SIZE,
				 "TESS_KERNEL_OPS_MIN_SIZE reaches the last operation");

/*
 * The registry of the kernels: one table per backend, installed by the
 * module that links them.
 */
typedef struct TessKernelRegistryOps
{
	uint32		abi_version;
	Size		struct_size;
	/*
	 * Install without copying. Installing the same table again is safe;
	 * another table while one is installed is an error, as is an invalid
	 * table (a wrong version, a short size, a missing operation).
	 */
	void		(*set) (const TessKernelOps *ops);
	/*
	 * Remove this exact table if it is installed, without freeing it. NULL
	 * and a table that is not installed are ignored.
	 */
	void		(*clear) (const TessKernelOps *ops);
	/*
	 * The installed table, or NULL. The borrowed pointer is valid until the
	 * provider clears it; the consumer must not modify it.
	 */
	const TessKernelOps *(*get) (void);
} TessKernelRegistryOps;

#define TESS_KERNEL_REGISTRY_OPS_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessKernelRegistryOps, get)

#endif							/* TESSERA_KERNEL_OPS_H */

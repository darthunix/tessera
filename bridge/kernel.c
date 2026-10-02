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
#define TESS_KERNEL_OP_MISSING(name) || ops->name == NULL
	if (false TESS_KERNEL_OPS(TESS_KERNEL_OP_MISSING))
#undef TESS_KERNEL_OP_MISSING
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

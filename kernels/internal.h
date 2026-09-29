/* Definitions shared within the tessera_kernels module. */
#ifndef TESSERA_KERNELS_INTERNAL_H
#define TESSERA_KERNELS_INTERNAL_H

#include "tessera/kernel_ops.h"

/* The entry points of the linked Rust kernels, installed in the bridge. */
extern const TessKernelOps tess_kernel_ops;

/* The text functions (text.c), registered with the others. */
struct TessFunctionRegistryOps;
extern void tess_register_text_functions(const struct TessFunctionRegistryOps *functions);

#endif							/* TESSERA_KERNELS_INTERNAL_H */

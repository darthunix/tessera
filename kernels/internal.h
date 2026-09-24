/* Definitions shared within the tessera_kernels module. */
#ifndef TESSERA_KERNELS_INTERNAL_H
#define TESSERA_KERNELS_INTERNAL_H

#include "tessera/kernel_ops.h"

/* The entry points of the linked Rust kernels, installed in the bridge. */
extern const TessKernelOps tess_kernel_ops;

#endif							/* TESSERA_KERNELS_INTERNAL_H */

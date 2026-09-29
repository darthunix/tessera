/* Definitions shared within the tessera_kernels module. */
#ifndef TESSERA_KERNELS_INTERNAL_H
#define TESSERA_KERNELS_INTERNAL_H

#include "tessera/kernel_ops.h"

/* The entry points of the linked Rust kernels, installed in the bridge. */
extern const TessKernelOps tess_kernel_ops;

/* The text, date, numeric and float functions (text.c, date.c, numeric.c, float.c), registered with the others. */
struct TessFunctionRegistryOps;
extern void tess_register_text_functions(const struct TessFunctionRegistryOps *functions);
extern void tess_register_date_functions(const struct TessFunctionRegistryOps *functions);
extern void tess_register_numeric_functions(const struct TessFunctionRegistryOps *functions);
extern void tess_register_float_functions(const struct TessFunctionRegistryOps *functions);

/* A numeric of an integer: a small one from the process's cache, else made in context. */
extern struct NumericData *tess_numeric_from_int64(int64 value, MemoryContext context);

#endif							/* TESSERA_KERNELS_INTERNAL_H */

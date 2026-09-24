/* Definitions shared within the Tessera bridge library. */
#ifndef TESSERA_BRIDGE_INTERNAL_H
#define TESSERA_BRIDGE_INTERNAL_H

#include "tessera/binding.h"
#include "tessera/function.h"
#include "tessera/kernel_ops.h"
#include "tessera/node.h"
#include "tessera/source.h"

extern const TessBindingOps tess_binding_ops;
extern const TessFunctionRegistryOps tess_function_registry_ops;
extern const TessKernelRegistryOps tess_kernel_registry_ops;
extern const TessNodeRegistryOps tess_node_registry_ops;
extern const TessSourceRegistryOps tess_source_registry_ops;

#endif /* TESSERA_BRIDGE_INTERNAL_H */

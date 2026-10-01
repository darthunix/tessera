/* Helpers for append-only C interfaces shared by Tessera extensions. */
#ifndef TESSERA_ABI_H
#define TESSERA_ABI_H

#include "postgres.h"

/*
 * The version of Tessera's modules, which PG_MODULE_MAGIC_EXT records: the
 * workspace's version in Cargo.toml.
 */
#define TESS_VERSION "0.1.0"

/* Size in bytes needed to include field and all preceding fields. */
#define TESS_ABI_SIZE_INCLUDING_FIELD(type, field) \
	(offsetof(type, field) + sizeof(((type *) 0)->field))

/* True when an append-only structure supplied by another module has field. */
#define TESS_ABI_HAS_FIELD(object, type, field) \
	((object) != NULL && (object)->struct_size >= \
	 TESS_ABI_SIZE_INCLUDING_FIELD(type, field))

/* Header for a versioned operation table defined with designated fields. */
#define TESS_ABI_INITIALIZER(version, type) \
	.abi_version = (version), .struct_size = sizeof(type)

/* Initial value for an append-only callback argument or result. */
#define TESS_STRUCT_INITIALIZER(type) \
	{.struct_size = sizeof(type)}

#endif /* TESSERA_ABI_H */

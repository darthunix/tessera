/*
 * Helpers for append-only C interfaces shared by Tessera extensions, and
 * the PostgreSQL versions they build against.
 */
#ifndef TESSERA_ABI_H
#define TESSERA_ABI_H

#include "postgres.h"

/*
 * Tessera follows PostgreSQL master (20devel) and builds against 19 as
 * well, where five interfaces differ: each is read in two ways where it is
 * used, by PG_VERSION_NUM. Older cores differ much more. Every module
 * includes this header, so the headers of an older server stop the build
 * here rather than at the first declaration they lack.
 */
#if PG_VERSION_NUM < 190000
#error "Tessera needs PostgreSQL 19 or master (20devel)"
#endif

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

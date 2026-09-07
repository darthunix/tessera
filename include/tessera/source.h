/* Registry of batch sources provided by independent Tessera extensions. */
#ifndef TESSERA_SOURCE_H
#define TESSERA_SOURCE_H

#include "postgres.h"

#include "tessera/abi.h"

#define TESS_SOURCE_ABI_VERSION 0
#define TESS_SOURCE_REGISTRY_OPS_ABI_VERSION 0

/*
 * Stable identity of one batch source.
 *
 * The provider owns the TessSource structure and the string referenced by
 * name. The registry stores the provider's pointer without copying the
 * structure or name string. The structure and name string must remain valid
 * from add until remove returns. The provider must ensure that all consumers
 * have finished using their borrowed pointers before remove, then may free
 * its allocations.
 * Source capabilities will be added as separate interfaces when there is
 * an executor consumer for them.
 */
typedef struct TessSource
{
	uint32		abi_version;
	Size		struct_size;
	const char *name;
} TessSource;

#define TESS_SOURCE_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessSource, name)

/* Backend-local registry shared by independently built extensions. */
typedef struct TessSourceRegistryOps
{
	uint32		abi_version;
	Size		struct_size;
	/* Register without copying. Repeating the same registration is safe. */
	void		(*add) (const TessSource *source);
	/*
	 * Unregister this exact source without freeing it or waiting for users.
	 * NULL and repeated calls for an object that is still alive are safe.
	 */
	void		(*remove) (const TessSource *source);
	/*
	 * Find by case-sensitive name, or return NULL. The borrowed pointer does
	 * not extend the source's lifetime and must not be used after remove.
	 * The consumer must not modify or free the source or its name.
	 */
	const TessSource *(*find) (const char *name);
} TessSourceRegistryOps;

#define TESS_SOURCE_REGISTRY_OPS_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessSourceRegistryOps, find)

#endif /* TESSERA_SOURCE_H */

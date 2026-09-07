/*
 * Backend-local API shared by independently built Tessera extensions.
 *
 * The table is published by the tessera extension through a PostgreSQL
 * rendezvous variable. Subsystem tables can be appended in later revisions.
 */
#ifndef TESSERA_BRIDGE_H
#define TESSERA_BRIDGE_H

#include "postgres.h"

#include "tessera/abi.h"
#include "tessera/binding.h"
#include "tessera/source.h"

#define TESS_API_RENDEZVOUS "tessera.api.v0"
#define TESS_API_ABI_VERSION 0

/*
 * Root of the backend-local APIs shared by Tessera extensions.
 * Compatible subsystem tables can be appended without changing existing
 * tables or making their consumers depend on unrelated APIs.
 */
typedef struct TessApi
{
	/* Changes when an existing API contract becomes incompatible. */
	uint32		abi_version;
	/* Gates access to fields appended by later compatible versions. */
	Size		struct_size;
	/* Required operations for the connection carried by one tuple slot. */
	const TessBindingOps *binding_ops;
	/* Required registry of batch sources from independent extensions. */
	const TessSourceRegistryOps *sources;
} TessApi;

/* Both subsystem pointers are required in the current root. */
#define TESS_API_MIN_SIZE TESS_ABI_SIZE_INCLUDING_FIELD(TessApi, sources)

#endif /* TESSERA_BRIDGE_H */

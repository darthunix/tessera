/*
 * Backend-local API shared by independently built Tessera extensions.
 *
 * The table is published by the tessera extension through a PostgreSQL
 * rendezvous variable. Operations will be appended in later revisions.
 */
#ifndef TESSERA_BRIDGE_H
#define TESSERA_BRIDGE_H

#include "postgres.h"

#include "tessera/abi.h"

#define TESS_API_RENDEZVOUS "tessera.api.v0"
#define TESS_API_ABI_VERSION 0

typedef struct TessApi
{
	/* Changes when an existing API contract becomes incompatible. */
	uint32		abi_version;
	/* Gates access to fields appended by later compatible versions. */
	Size		struct_size;
} TessApi;

#define TESS_API_MIN_SIZE TESS_ABI_SIZE_THROUGH(TessApi, struct_size)

#endif /* TESSERA_BRIDGE_H */

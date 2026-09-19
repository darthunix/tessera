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
#include "tessera/function.h"
#include "tessera/node.h"
#include "tessera/source.h"

#define TESS_API_RENDEZVOUS "tessera.api.v0"
#define TESS_API_ABI_VERSION 0
#define TESS_SETTINGS_ABI_VERSION 0

/*
 * Borrowed, read-only pointers to the bridge's configuration variables.
 * The bridge defines them, since a GUC can be defined once per backend
 * while several independent modules read it; see docs/node.md.
 */
typedef struct TessSettings
{
	uint32		abi_version;
	Size		struct_size;
	/* tessera.enable: planner hooks add batch paths only while true. */
	const bool *enable;
} TessSettings;

#define TESS_SETTINGS_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessSettings, enable)

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
	/* Required registry of batch-producing node kinds. */
	const TessNodeRegistryOps *nodes;
	/* Required registry of batch implementations of PostgreSQL functions. */
	const TessFunctionRegistryOps *functions;
	/* Required configuration variables shared by every module. */
	const TessSettings *settings;
} TessApi;

/* All subsystem pointers are required in the current root. */
#define TESS_API_MIN_SIZE TESS_ABI_SIZE_INCLUDING_FIELD(TessApi, settings)

#endif /* TESSERA_BRIDGE_H */

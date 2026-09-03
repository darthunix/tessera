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
#include "tessera/layout.h"
#include "tessera/request.h"

#define TESS_API_RENDEZVOUS "tessera.api.v0"
#define TESS_API_ABI_VERSION 0

typedef struct TessBinding TessBinding;
typedef struct TupleTableSlot TupleTableSlot;

/*
 * Backend-local bridge operations.
 *
 * The table remains valid for the lifetime of the backend. Bindings and the
 * objects returned from them remain valid until detach or slot context reset.
 */
typedef struct TessApi
{
	/* Changes when an existing API contract becomes incompatible. */
	uint32		abi_version;
	/* Gates access to fields appended by later compatible versions. */
	Size		struct_size;

	/* Attach a copied layout to a slot; duplicate attachment is an error. */
	TessBinding *(*attach) (TupleTableSlot *slot, const TessLayout *layout);
	/* Find a slot's binding, or return NULL. Cache a successful lookup. */
	TessBinding *(*find_binding) (TupleTableSlot *slot);
	/* Replace the copied request before it is frozen. */
	void		(*set_request) (TessBinding *binding,
							const TessRequest *request);
	/* Return the binding's immutable copied layout. */
	const TessLayout *(*get_layout) (TessBinding *binding);
	/* Freeze and return the request; repeated calls return the same pointer. */
	const TessRequest *(*freeze_request) (TessBinding *binding);
	/* Remove a binding; a NULL binding is ignored. */
	void		(*detach) (TessBinding *binding);
} TessApi;

#define TESS_API_MIN_SIZE TESS_ABI_SIZE_THROUGH(TessApi, detach)

#endif /* TESSERA_BRIDGE_H */

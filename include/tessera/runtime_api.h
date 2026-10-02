/*
 * The bridge's API and the kernels' operations as the runtime library takes
 * them, and the reporting of a kernel's status. Part of tessera/runtime.h.
 */
#ifndef TESSERA_RUNTIME_API_H
#define TESSERA_RUNTIME_API_H

#include "postgres.h"

#include "tessera/bridge.h"
#include "tessera/status.h"

/*
 * The bridge's API, validated once per backend: the root's version and size
 * and the binding operations. The bridge must already be loaded (CREATE
 * EXTENSION tessera); otherwise this raises ERROR.
 */
extern const TessApi *tess_runtime_api(void);

/* Whether tessera.enable is on: the first gate of every planner hook. */
static inline bool
tess_enabled(void)
{
	return *tess_runtime_api()->settings->enable;
}

/*
 * The operations of the Rust kernels (tessera/kernel_ops.h), or NULL when
 * none are installed: the tessera_kernels module is not loaded in this
 * backend, or the bridge has no kernel registry. Raises ERROR when the
 * installed table does not match this build (another ABI version, a short
 * size, another table format). Not cached, since the kernels module may be
 * loaded after the caller; planning and execution each ask again.
 */
extern const TessKernelOps *tess_runtime_kernels(void);

/*
 * Raise the ERROR that a failed kernel call stored in status, with its
 * SQLSTATE and message. Call it after the kernel returned, never from
 * inside one.
 */
pg_noreturn extern void tess_status_report(const TessStatus *status);

/* Raise the ERROR a call stored in status if its code says it failed. */
static inline void
tess_status_check(TessStatusCode code, const TessStatus *status)
{
	if (code != TESS_OK)
		tess_status_report(status);
}

#endif							/* TESSERA_RUNTIME_API_H */

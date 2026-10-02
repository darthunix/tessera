/*
 * The parts every batch node repeats: the shared memory callbacks of its
 * counters, the guard against backward scans and mark/restore, memory in
 * EXPLAIN, the rescan of a child and the projection of computed columns.
 * Part of tessera/runtime.h.
 */
#ifndef TESSERA_RUNTIME_NODE_H
#define TESSERA_RUNTIME_NODE_H

#include "postgres.h"

#include "access/parallel.h"
#include "commands/explain_format.h"
#include "nodes/execnodes.h"

#include "tessera/layout.h"
#include "tessera/runtime_input.h"
#include "tessera/runtime_project.h"
#include "tessera/runtime_shared_stats.h"

/*
 * The five shared memory callbacks of a node whose only shared memory is
 * its counters (runtime_shared_stats.h), as static functions prefix_*: the
 * node's state State keeps them in a field stats, ncounters of them, and
 * counters(state, values) fills this participant's. A macro, since the
 * callbacks get only the CustomScanState and must know the state's type;
 * TESS_NODE_STATS_METHODS names them in the node's CustomExecMethods. A
 * node with shared memory of its own writes its callbacks by hand and calls
 * the same tess_shared_stats_* functions for the counters' part.
 */
#define TESS_NODE_STATS_CALLBACKS(prefix, State, ncounters, counters) \
static Size \
prefix##_estimate_dsm(CustomScanState *css, ParallelContext *pcxt) \
{ \
	return tess_shared_stats_estimate((ncounters), pcxt->nworkers); \
} \
\
static void \
prefix##_initialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate) \
{ \
	State	   *state = (State *) css; \
\
	state->stats = tess_shared_stats_setup(state->stats, css->ss.ps.state->es_query_cxt, \
										   coordinate, (ncounters), pcxt->nworkers, \
										   pcxt->seg); \
} \
\
static void \
prefix##_reinitialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate) \
{ \
	tess_shared_stats_reset(((State *) css)->stats); \
} \
\
static void \
prefix##_initialize_worker(CustomScanState *css, shm_toc *toc, void *coordinate) \
{ \
	State	   *state = (State *) css; \
\
	state->stats = tess_shared_stats_attach(css->ss.ps.state->es_query_cxt, coordinate, \
											ParallelWorkerNumber + 1); \
} \
\
static void \
prefix##_shutdown(CustomScanState *css) \
{ \
	State	   *state = (State *) css; \
	uint64		values[ncounters]; \
\
	if (state->stats == NULL) \
		return; \
	counters(state, values); \
	tess_shared_stats_store(state->stats, values); \
}

/* The callbacks of TESS_NODE_STATS_CALLBACKS, as CustomExecMethods fields. */
#define TESS_NODE_STATS_METHODS(prefix) \
	.EstimateDSMCustomScan = prefix##_estimate_dsm, \
	.InitializeDSMCustomScan = prefix##_initialize_dsm, \
	.ReInitializeDSMCustomScan = prefix##_reinitialize_dsm, \
	.InitializeWorkerCustomScan = prefix##_initialize_worker, \
	.ShutdownCustomScan = prefix##_shutdown

/*
 * ERROR when the executor asks a node, named name, for a backward scan or
 * mark/restore: the planner puts Material above a batch subtree for them.
 */
extern void tess_node_require_forward(int eflags, const char *name);

/* An EXPLAIN property of bytes, in kB rounded up. */
extern void tess_explain_kb(const char *label, uint64 bytes, ExplainState *es);

/*
 * Rescan a node's child with the node: the changed parameters passed to
 * it, which the core passes to outer and inner plans only, the child
 * rescanned, and its batch input, if any, started anew.
 */
extern void tess_rescan_child(PlanState *parent, PlanState *child, TessInput *input);

/*
 * The projection of a node's computed columns (runtime_project.h) over its
 * scan tuple: the tuple's layout, the columns it has before the computed
 * ones, the slot the expressions read, and the computed target entries.
 */
extern TessProjection *tess_node_projection(CustomScanState *css, TupleTableSlot *scan_slot,
											const TessLayout *scan_tuple, int base_columns,
											List *computed);

#endif							/* TESSERA_RUNTIME_NODE_H */

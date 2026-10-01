/*
 * A node's counters summed over the participants of a parallel query. Part
 * of tessera/runtime.h.
 */
#ifndef TESSERA_RUNTIME_SHARED_STATS_H
#define TESSERA_RUNTIME_SHARED_STATS_H

#include "postgres.h"

#include "storage/dsm.h"
#include "utils/palloc.h"

/*
 * Counters of a node summed over the participants of a parallel query:
 * one row of ncounters values per participant in the node's chunk of
 * shared memory, the leader's first. A node with such counters declares
 * its partial path parallel-aware, since PostgreSQL calls the shared
 * memory callbacks of a custom scan only then, and sizes its chunk with
 * the estimate.
 */
typedef struct TessSharedStats TessSharedStats;

/* Bytes for the rows of nworkers workers and the leader. */
extern Size tess_shared_stats_estimate(int ncounters, int nworkers);

/*
 * The leader, in InitializeDSMCustomScan: lay the zeroed rows out in the
 * chunk. With the query's segment, the rows are summed when it is
 * detached, after every worker has finished; without one, the caller sums
 * them with tess_shared_stats_collect. The handle lives in the context.
 */
extern TessSharedStats *tess_shared_stats_init(MemoryContext parent_context,
											   void *coordinate,
											   int ncounters, int nworkers,
											   dsm_segment *segment);

/*
 * tess_shared_stats_init that ends the handle of an earlier call first, if
 * any: a Gather or Gather Merge that a limit above shut down sets the plan
 * up anew when rescanned, calling InitializeDSMCustomScan again.
 */
extern TessSharedStats *tess_shared_stats_setup(TessSharedStats *previous,
												MemoryContext parent_context,
												void *coordinate,
												int ncounters, int nworkers,
												dsm_segment *segment);

/* The bytes the rows laid out in the chunk take: what follows them starts there. */
extern Size tess_shared_stats_size(const void *coordinate);

/*
 * A worker, in InitializeWorkerCustomScan: its row of the leader's chunk,
 * slot ParallelWorkerNumber + 1.
 */
extern TessSharedStats *tess_shared_stats_attach(MemoryContext parent_context,
												 void *coordinate, int slot);

/* The leader, in ReInitializeDSMCustomScan: every row and the totals to zero. */
extern void tess_shared_stats_reset(TessSharedStats *stats);

/*
 * This participant's ncounters values into its row, in ShutdownCustomScan;
 * nothing once the rows are summed or forgotten.
 */
extern void tess_shared_stats_store(TessSharedStats *stats,
									const uint64 *values);

/* Sum the rows into the totals now; the chunk is forgotten afterwards. */
extern void tess_shared_stats_collect(TessSharedStats *stats);

/* The ncounters totals after a collection, or NULL before one. */
extern const uint64 *tess_shared_stats_totals(const TessSharedStats *stats);

/*
 * For EXPLAIN: the totals of every participant once a parallel plan
 * collected them, else own, the node's own counters; stats may be NULL.
 */
extern const uint64 *tess_shared_stats_totals_or(const TessSharedStats *stats,
												 const uint64 *own);

/*
 * EndCustomScan: the leader sums the rows if they are still mapped, since
 * the segment outlives the children of a Gather, and cancels the detach
 * callback; a worker forgets its row.
 */
extern void tess_shared_stats_end(TessSharedStats *stats);

#endif							/* TESSERA_RUNTIME_SHARED_STATS_H */

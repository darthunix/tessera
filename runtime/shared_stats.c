/*
 * Counters of a node summed over the participants of a parallel query.
 *
 * A node's own counters live in its backend; in a parallel query the
 * workers' counts would be lost to EXPLAIN ANALYZE, which reads the
 * leader's node. The helper lays out one row of counters per participant
 * in the node's chunk of the query's shared memory: each participant
 * stores its counters there when its node shuts down, and the leader sums
 * the rows once the segment is detached, after every worker has finished,
 * or when its node ends first. See docs/runtime.md, "Counters over
 * parallel participants".
 */
#include "postgres.h"

#include <string.h>

#include "storage/dsm.h"
#include "utils/memutils.h"

#include "tessera/runtime.h"

/* The chunk: the leader's row first, one per worker after it. */
typedef struct SharedRows
{
	int			nslots;
	int			ncounters;
	uint64		values[FLEXIBLE_ARRAY_MEMBER];
} SharedRows;

struct TessSharedStats
{
	/* NULL once the rows are summed or forgotten. */
	SharedRows *shared;
	/* The leader's segment while the detach callback is registered. */
	dsm_segment *segment;
	int			slot;
	int			ncounters;
	/* Backend-local: valid after a collection. */
	uint64	   *totals;
	bool		collected;
};

static Size
rows_size(int ncounters, int nslots)
{
	return add_size(offsetof(SharedRows, values),
					mul_size(mul_size(ncounters, nslots), sizeof(uint64)));
}

static void
check_stats(const TessSharedStats *stats)
{
	if (stats == NULL)
		elog(ERROR, "Tessera shared counters are missing");
}

Size
tess_shared_stats_estimate(int ncounters, int nworkers)
{
	if (ncounters <= 0 || nworkers < 0)
		elog(ERROR, "Tessera shared counters require counters and workers");
	return MAXALIGN(rows_size(ncounters, nworkers + 1));
}

static TessSharedStats *
make_handle(MemoryContext parent_context, SharedRows *shared, int slot)
{
	TessSharedStats *stats;

	stats = MemoryContextAllocZero(parent_context, sizeof(TessSharedStats));
	stats->shared = shared;
	stats->slot = slot;
	stats->ncounters = shared->ncounters;
	stats->totals = MemoryContextAllocZero(parent_context,
										   mul_size(shared->ncounters,
													sizeof(uint64)));
	return stats;
}

/* The workers have finished by the time the leader detaches the segment. */
static void
collect_on_detach(dsm_segment *segment, Datum arg)
{
	TessSharedStats *stats = (TessSharedStats *) DatumGetPointer(arg);

	stats->segment = NULL;
	tess_shared_stats_collect(stats);
}

TessSharedStats *
tess_shared_stats_init(MemoryContext parent_context, void *coordinate,
					   int ncounters, int nworkers, dsm_segment *segment)
{
	SharedRows *shared = coordinate;
	TessSharedStats *stats;

	if (parent_context == NULL || coordinate == NULL)
		elog(ERROR, "Tessera shared counters require a context and a chunk");
	if (ncounters <= 0 || nworkers < 0)
		elog(ERROR, "Tessera shared counters require counters and workers");
	memset(shared, 0, rows_size(ncounters, nworkers + 1));
	shared->nslots = nworkers + 1;
	shared->ncounters = ncounters;
	stats = make_handle(parent_context, shared, 0);
	if (segment != NULL)
	{
		stats->segment = segment;
		on_dsm_detach(segment, collect_on_detach, PointerGetDatum(stats));
	}
	return stats;
}

TessSharedStats *
tess_shared_stats_attach(MemoryContext parent_context, void *coordinate,
						 int slot)
{
	SharedRows *shared = coordinate;

	if (parent_context == NULL || coordinate == NULL)
		elog(ERROR, "Tessera shared counters require a context and a chunk");
	if (shared->ncounters <= 0 || shared->nslots <= 1)
		elog(ERROR, "Tessera shared counters were not laid out");
	if (slot <= 0 || slot >= shared->nslots)
		elog(ERROR, "Tessera shared counters have no slot %d", slot);
	return make_handle(parent_context, shared, slot);
}

void
tess_shared_stats_reset(TessSharedStats *stats)
{
	check_stats(stats);
	if (stats->shared != NULL)
		memset(stats->shared->values, 0,
			   mul_size(mul_size(stats->ncounters, stats->shared->nslots),
						sizeof(uint64)));
	memset(stats->totals, 0, mul_size(stats->ncounters, sizeof(uint64)));
	stats->collected = false;
}

void
tess_shared_stats_store(TessSharedStats *stats, const uint64 *values)
{
	check_stats(stats);
	if (values == NULL)
		elog(ERROR, "Tessera shared counters require values to store");
	/* Detached: the totals are final. */
	if (stats->shared == NULL)
		return;
	memcpy(&stats->shared->values[stats->slot * stats->ncounters], values,
		   mul_size(stats->ncounters, sizeof(uint64)));
}

void
tess_shared_stats_collect(TessSharedStats *stats)
{
	SharedRows *shared;

	check_stats(stats);
	shared = stats->shared;
	if (shared == NULL)
		return;
	memset(stats->totals, 0, mul_size(stats->ncounters, sizeof(uint64)));
	for (int slot = 0; slot < shared->nslots; slot++)
	{
		const uint64 *row = &shared->values[slot * stats->ncounters];

		for (int counter = 0; counter < stats->ncounters; counter++)
			stats->totals[counter] += row[counter];
	}
	stats->collected = true;
	if (stats->segment != NULL)
	{
		cancel_on_dsm_detach(stats->segment, collect_on_detach,
							 PointerGetDatum(stats));
		stats->segment = NULL;
	}
	stats->shared = NULL;
}

const uint64 *
tess_shared_stats_totals(const TessSharedStats *stats)
{
	check_stats(stats);
	return stats->collected ? stats->totals : NULL;
}

void
tess_shared_stats_end(TessSharedStats *stats)
{
	check_stats(stats);
	if (stats->slot == 0)
		tess_shared_stats_collect(stats);
	else
		stats->shared = NULL;
}

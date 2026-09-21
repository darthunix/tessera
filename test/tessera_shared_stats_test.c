#include "postgres.h"

#include <string.h>

#include "fmgr.h"
#include "storage/dsm.h"
#include "utils/memutils.h"

#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_shared_stats_sum);
PG_FUNCTION_INFO_V1(tessera_test_shared_stats_segment);
PG_FUNCTION_INFO_V1(tessera_test_shared_stats_errors);

#define NCOUNTERS 3
#define NWORKERS 2

static void
expect(bool condition, const char *what)
{
	if (!condition)
		elog(ERROR, "Tessera shared counters test failed: %s", what);
}

static void
store_row(TessSharedStats *stats, uint64 base)
{
	uint64		values[NCOUNTERS] = {base, base * 10, base * 100};

	tess_shared_stats_store(stats, values);
}

static bool
totals_are(const TessSharedStats *stats, uint64 base)
{
	const uint64 *totals = tess_shared_stats_totals(stats);

	return totals != NULL && totals[0] == base && totals[1] == base * 10 &&
		totals[2] == base * 100;
}

/* A chunk without a segment: the leader's row and two workers' rows. */
Datum
tessera_test_shared_stats_sum(PG_FUNCTION_ARGS)
{
	Size		size = tess_shared_stats_estimate(NCOUNTERS, NWORKERS);
	void	   *chunk = palloc(size);
	TessSharedStats *leader;
	TessSharedStats *worker1;
	TessSharedStats *worker2;

	expect(size >= (NWORKERS + 1) * NCOUNTERS * sizeof(uint64), "estimate");
	leader = tess_shared_stats_init(CurrentMemoryContext, chunk, NCOUNTERS,
									NWORKERS, NULL);
	expect(tess_shared_stats_totals(leader) == NULL, "no totals yet");
	worker1 = tess_shared_stats_attach(CurrentMemoryContext, chunk, 1);
	worker2 = tess_shared_stats_attach(CurrentMemoryContext, chunk, 2);
	store_row(leader, 1);
	store_row(worker1, 2);
	store_row(worker2, 4);
	/* A row is replaced, not added to. */
	store_row(worker2, 3);
	tess_shared_stats_collect(leader);
	expect(totals_are(leader, 6), "sum of three rows");
	/* Summed rows are forgotten: a later store changes nothing. */
	store_row(leader, 100);
	expect(totals_are(leader, 6), "totals are final");
	/* A fresh scan starts from zero. */
	worker1 = tess_shared_stats_attach(CurrentMemoryContext, chunk, 1);
	leader = tess_shared_stats_init(CurrentMemoryContext, chunk, NCOUNTERS,
									NWORKERS, NULL);
	store_row(worker1, 5);
	tess_shared_stats_reset(leader);
	expect(tess_shared_stats_totals(leader) == NULL, "reset forgets totals");
	store_row(worker1, 7);
	tess_shared_stats_end(leader);
	expect(totals_are(leader, 7), "end sums the rows");
	tess_shared_stats_end(worker1);
	tess_shared_stats_end(worker2);
	PG_RETURN_BOOL(true);
}

/* A real segment: the rows are summed when it is detached, or at end. */
Datum
tessera_test_shared_stats_segment(PG_FUNCTION_ARGS)
{
	Size		size = tess_shared_stats_estimate(NCOUNTERS, NWORKERS);
	dsm_segment *segment = dsm_create(size, 0);
	void	   *chunk = dsm_segment_address(segment);
	TessSharedStats *leader;
	TessSharedStats *worker;

	leader = tess_shared_stats_init(CurrentMemoryContext, chunk, NCOUNTERS,
									NWORKERS, segment);
	worker = tess_shared_stats_attach(CurrentMemoryContext, chunk, 2);
	store_row(leader, 1);
	store_row(worker, 2);
	expect(tess_shared_stats_totals(leader) == NULL, "not summed before detach");
	dsm_detach(segment);
	expect(totals_are(leader, 3), "summed at detach");
	tess_shared_stats_end(leader);
	expect(totals_are(leader, 3), "end after detach keeps the totals");
	/* Ended first: summed then, and the detach changes nothing. */
	segment = dsm_create(size, 0);
	chunk = dsm_segment_address(segment);
	leader = tess_shared_stats_init(CurrentMemoryContext, chunk, NCOUNTERS,
									NWORKERS, segment);
	worker = tess_shared_stats_attach(CurrentMemoryContext, chunk, 1);
	store_row(worker, 4);
	tess_shared_stats_end(leader);
	expect(totals_are(leader, 4), "end sums before detach");
	store_row(worker, 8);
	dsm_detach(segment);
	expect(totals_are(leader, 4), "detach after end changes nothing");
	PG_RETURN_BOOL(true);
}

Datum
tessera_test_shared_stats_errors(PG_FUNCTION_ARGS)
{
	int			kind = PG_GETARG_INT32(0);
	Size		size = tess_shared_stats_estimate(NCOUNTERS, NWORKERS);
	void	   *chunk = palloc(size);
	TessSharedStats *leader;

	switch (kind)
	{
		case 0:
			tess_shared_stats_estimate(0, NWORKERS);
			break;
		case 1:
			tess_shared_stats_estimate(NCOUNTERS, -1);
			break;
		case 2:
			tess_shared_stats_init(CurrentMemoryContext, NULL, NCOUNTERS,
								   NWORKERS, NULL);
			break;
		case 3:
			/* Not laid out yet. */
			memset(chunk, 0, size);
			tess_shared_stats_attach(CurrentMemoryContext, chunk, 1);
			break;
		case 4:
			tess_shared_stats_init(CurrentMemoryContext, chunk, NCOUNTERS,
								   NWORKERS, NULL);
			tess_shared_stats_attach(CurrentMemoryContext, chunk, 0);
			break;
		case 5:
			tess_shared_stats_init(CurrentMemoryContext, chunk, NCOUNTERS,
								   NWORKERS, NULL);
			tess_shared_stats_attach(CurrentMemoryContext, chunk, NWORKERS + 1);
			break;
		case 6:
			leader = tess_shared_stats_init(CurrentMemoryContext, chunk,
											NCOUNTERS, NWORKERS, NULL);
			tess_shared_stats_store(leader, NULL);
			break;
		case 7:
			tess_shared_stats_collect(NULL);
			break;
		default:
			elog(ERROR, "unknown error case %d", kind);
	}
	elog(ERROR, "Tessera test expected a shared counters error");
}

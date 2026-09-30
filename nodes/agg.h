/* Definitions shared by the planner (agg_planner.c) and the executor (agg.c) of TessAgg. */
#ifndef TESSERA_NODES_AGG_H
#define TESSERA_NODES_AGG_H

#include "postgres.h"

#include "nodes/primnodes.h"

#include "tessera/table.h"

/* A generic sum state's payload words: the kernels' state, then the rest's address. */
#define AGG_SUM_STATE_WORDS (TESS_TABLE_SUM_WORDS + 1)

/* A group's aggregate states have one flag bit each in a payload word. */
#define AGG_MAX_GROUPED 64

/* How the partials of an aggregate combine, and what an empty input gives. */
typedef enum AggKind
{
	AGG_COUNT,					/* int8 sum of the partials, 0 without any */
	AGG_SUM,					/* int8 sum of the partials, NULL without any */
	AGG_MIN,					/* the least partial, NULL without any */
	AGG_MAX,					/* the greatest partial, NULL without any */
	AGG_GENERIC					/* the core's functions, over the batch's rows */
} AggKind;

/*
 * An entry of a key dictionary (agg.c): a value, its hash, its number and
 * the hash table's status; the planner sizes the dictionaries by it.
 */
typedef struct KeyEntry
{
	Datum		value;
	uint32		hash;
	uint32		number;
	char		status;
} KeyEntry;

/* The planner's judgement of an aggregate, which the executor repeats. */
extern int	aggregate_kind(Oid aggfnoid);
extern bool generic_supported(const Aggref *agg);
extern bool batch_aggregate(const Aggref *agg);
extern bool sum_state_aggregate(const Aggref *agg);
extern bool own_partial_aggregate(const Aggref *agg);

#endif							/* TESSERA_NODES_AGG_H */

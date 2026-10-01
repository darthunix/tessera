/*
 * Batches with computed columns over a child's batch. Part of
 * tessera/runtime.h.
 */
#ifndef TESSERA_RUNTIME_PROJECT_H
#define TESSERA_RUNTIME_PROJECT_H

#include "postgres.h"

#include "nodes/execnodes.h"

#include "tessera/abi.h"
#include "tessera/batch.h"
#include "tessera/layout.h"

/*
 * A batch with computed columns: a wrapper around a child's batch whose
 * columns come first, with one more column per computed target, evaluated
 * when a consumer asks for it, for the rows asked for. A target the
 * expression compiler accepts (tessera/expr.h) is computed by its batch
 * chain over the batch's selected rows, once per batch; any other target
 * row by row by the executor over the scan tuple slot, with the rows
 * computed remembered, so that nothing is computed twice and an error in
 * an expression is raised only for a row a consumer asked for. The wrapper
 * shares the child's row mask: narrowing it narrows both. See
 * docs/runtime.md.
 */
typedef struct TessProjection TessProjection;

typedef struct TessProjectionConfig
{
	Size		struct_size;
	/* Owns the projection; by-reference results live in a child context. */
	MemoryContext parent_context;
	/* Compiles the expressions and supplies their Params; NULL in a test. */
	PlanState  *parent;
	/*
	 * Evaluates the chains' scalars. The row-wise expressions get an
	 * expression context of their own, from the parent's executor state,
	 * whose per-tuple memory holds their results until the wrapper is
	 * released.
	 */
	ExprContext *econtext;
	/* The scan tuple slot, virtual, that the row-wise path fills. */
	TupleTableSlot *scan_slot;
	/* Scan tuple attribute to base column, as TessPlanInfo gives it. */
	const TessLayout *scan_tuple;
	/* The child's columns, first in the wrapper. */
	int			base_columns;
	/* TargetEntry list: the computed targets, in the order of their columns. */
	List	   *computed;
} TessProjectionConfig;

#define TESS_PROJECTION_CONFIG_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessProjectionConfig, computed)

typedef struct TessProjectionStats
{
	/* Values computed by chains, over the batches' selected rows. */
	uint64		chain_datums;
	/* Values computed row by row. */
	uint64		row_datums;
} TessProjectionStats;

extern TessProjection *tess_projection_create(const TessProjectionConfig *config);

/*
 * Wrap a batch: the wrapper is valid until it is released, when it forgets
 * the child, which stays the node's to finish; the previous wrapper must
 * have been released.
 */
extern TessBatch *tess_projection_wrap(TessProjection *projection,
									   TessBatch *child);

/* Forget a wrapped batch without releasing it, for a rescan. */
extern void tess_projection_reset(TessProjection *projection);

extern const TessProjectionStats *tess_projection_stats(const TessProjection *projection);

#endif							/* TESSERA_RUNTIME_PROJECT_H */

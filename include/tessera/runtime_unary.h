/*
 * The helper of a node with one batch child that only removes rows. Part of
 * tessera/runtime.h.
 */
#ifndef TESSERA_RUNTIME_UNARY_H
#define TESSERA_RUNTIME_UNARY_H

#include "postgres.h"

#include "executor/tuptable.h"
#include "nodes/execnodes.h"

#include "tessera/abi.h"
#include "tessera/batch.h"
#include "tessera/layout.h"
#include "tessera/request.h"
#include "tessera/runtime_project.h"

/*
 * A unary node has one batch child and only removes rows from its batches:
 * a limit, a filter. The helper joins the output and input sides: it takes
 * the parent's request on the node's result slot, derives the child's
 * request from it, fetches batches, lets the node process each one, skips
 * batches left without rows, and either forwards the child's slot to a
 * batch-aware parent or serves rows to an ordinary one. It adjusts the
 * node's instrumentation, so the node never does. See docs/runtime.md.
 */
typedef struct TessUnary TessUnary;

/*
 * Remove rows from the batch's mask and return the rows that remain. rows
 * is the count on entry. The callback may call tess_unary_stop.
 */
typedef int (*TessUnaryProcess) (void *private_data, TessBatch *batch,
								 int rows);

typedef struct TessUnaryStats
{
	uint64		input_batches;
	uint64		input_rows;
	/* Rows that remained after processing. */
	uint64		output_rows;
} TessUnaryStats;

typedef struct TessUnaryConfig
{
	Size		struct_size;
	MemoryContext parent_context;
	/* The node; its result slot is bound here, its instrumentation read. */
	CustomScanState *node;
	/* The initialized batch child; the node keeps it in custom_ps. */
	PlanState  *child;
	/* The node's output layout, with the child's number of columns. */
	const TessLayout *layout;
	/* Columns the node reads while processing; copied. */
	const Bitmapset *filter_columns;
	/* Columns the node reads after processing; copied. */
	const Bitmapset *projection_columns;
	/* The node's own limit on rows per batch, or zero. */
	int			max_rows;
	/* NULL keeps every row. */
	TessUnaryProcess process;
	void	   *private_data;
	/*
	 * Computed columns after the child's, or NULL: the helper publishes
	 * the projection's wrapper of each batch instead of the child's slot.
	 */
	TessProjection *projection;
	/*
	 * The plan node the executor rescans, when the batch child is reached
	 * through it: a subquery scan whose subplan is the batch child. NULL
	 * rescans the batch child itself.
	 */
	PlanState  *rescan_child;
} TessUnaryConfig;

#define TESS_UNARY_CONFIG_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessUnaryConfig, private_data)

/* Bind the node's result slot and wrap the child; see the config. */
extern TessUnary *tess_unary_create(const TessUnaryConfig *config);

extern PlanState *tess_unary_child(TessUnary *unary);

/* The parent's request, frozen at the first execution; NULL before. */
extern const TessRequest *tess_unary_request(TessUnary *unary);

/* The request sent to the child at the first execution; NULL before. */
extern const TessRequest *tess_unary_child_request(TessUnary *unary);

extern const TessUnaryStats *tess_unary_stats(TessUnary *unary);

/* Read no further batches: execution returns NULL from now on. */
extern void tess_unary_stop(TessUnary *unary);

/*
 * Tell the child how many rows the node needs at most, or -1 for no bound,
 * as ExecSetTupleBound does: a batch child whose node kind takes bounds
 * receives it through its callback and forwards it below, so a sort under
 * a limit stays a top-N sort; any other child goes to ExecSetTupleBound.
 */
extern void tess_unary_set_tuple_bound(TessUnary *unary, int64 tuples_needed);

/*
 * The node's ExecCustomScan: the child's slot with the next batch for a
 * batch-aware parent, the node's slot with the next row for an ordinary
 * one, or NULL at the end.
 */
extern TupleTableSlot *tess_unary_exec(TessUnary *unary);

/* Detach the node's binding; the node ends the child itself. */
extern void tess_unary_end(TessUnary *unary);

/* The whole rescan order of the node contract, including the child's. */
extern void tess_unary_rescan(TessUnary *unary);

#endif							/* TESSERA_RUNTIME_UNARY_H */

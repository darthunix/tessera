/* Runtime helpers for batch nodes, linked as libtessera_runtime.a. */
#ifndef TESSERA_RUNTIME_H
#define TESSERA_RUNTIME_H

#include "postgres.h"

#include "access/tupdesc.h"
#include "executor/tuptable.h"
#include "nodes/execnodes.h"
#include "storage/dsm.h"

#include "tessera/abi.h"
#include "tessera/batch.h"
#include "tessera/binding.h"
#include "tessera/bridge.h"

/*
 * The bridge's API, validated once per backend: the root's version and size
 * and the binding operations. The bridge must already be loaded (CREATE
 * EXTENSION tessera); otherwise this raises ERROR.
 */
extern const TessApi *tess_runtime_api(void);

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

/*
 * A builder collects rows from tuple slots into an owned column-major Datum
 * batch: one Datum and one NULL flag per row for each of the leading
 * ncolumns attributes, capacity rows at most. Pass-by-reference values are
 * copied into the builder's own memory context, so a slot may be reused
 * right after it was appended. The batch the builder returns exposes only
 * get_datum_column, with every row initialized (a NULL row holds 0), and
 * needs no release callback: reset reuses the storage. See docs/runtime.md.
 */
typedef struct TessBuilder TessBuilder;

typedef struct TessBuilderConfig
{
	Size		struct_size;
	/* Owns the builder and its arrays. */
	MemoryContext parent_context;
	/* Borrowed descriptor of the slots appended; outlives the builder. */
	TupleDesc	tuple_desc;
	/* Number of leading slot attributes copied into batch columns. */
	int			ncolumns;
	/* Rows in one batch; more than 64 is allowed. */
	int			capacity;
} TessBuilderConfig;

#define TESS_BUILDER_CONFIG_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessBuilderConfig, capacity)

/* Allocate an empty builder in the configured context. */
extern TessBuilder *tess_builder_create(const TessBuilderConfig *config);

/*
 * Discard copied values and start an empty batch. The caller must first
 * take a previously returned batch off its slot binding.
 */
extern void tess_builder_reset(TessBuilder *builder);

/* True after capacity rows were appended or the batch was finished. */
extern bool tess_builder_is_full(const TessBuilder *builder);

/*
 * Append one slot, materializing its leading ncolumns attributes; the slot
 * may be cleared or reused afterwards.
 */
extern void tess_builder_append_slot(TessBuilder *builder, TupleTableSlot *slot);

/*
 * Finish the batch and return it, or NULL without rows. The batch and its
 * columns stay valid until reset; finishing again returns the same batch.
 */
extern TessBatch *tess_builder_finish(TessBuilder *builder, Oid table_oid);

/*
 * A heap batch keeps the heap tuples of up to capacity rows instead of
 * copying their columns: a row appended from a buffer heap tuple slot is
 * kept as a reference into its page, which stays pinned until the batch
 * is released, and a row from any other slot is copied as a tuple. A
 * column is deformed only when a consumer asks for it and only for the
 * rows it asks for, resuming each row from where an earlier request
 * stopped (see tessera/heap_deform.h); by-reference values point into
 * the tuples. The batch exposes get_datum_column and release. See
 * docs/runtime.md.
 */
typedef struct TessHeapBatch TessHeapBatch;

typedef struct TessHeapBatchConfig
{
	Size		struct_size;
	/* Owns the batch and its arrays. */
	MemoryContext parent_context;
	/* Columns of the batch: the leading attributes of the tuples appended. */
	int			ncolumns;
	/* Rows in one batch; more than 64 is allowed. */
	int			capacity;
	/*
	 * The tuples' descriptor and the number of leading attributes every
	 * tuple has present, non-NULL and by value (the descriptor's
	 * firstNonGuaranteedAttr, or 0). NULL takes both from the first slot
	 * appended; tuples appended directly need them here.
	 */
	TupleDesc	tuple_desc;
	int			first_non_guaranteed_attr;
} TessHeapBatchConfig;

#define TESS_HEAP_BATCH_CONFIG_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessHeapBatchConfig, capacity)

typedef struct TessHeapBatchStats
{
	/* Values deformed on request. */
	uint64		deformed_datums;
	/* Of those, values before a row's cursor, deformed from the row's start. */
	uint64		restarted_datums;
	/* Rows copied as tuples, from slots without a pinned page. */
	uint64		copied_tuples;
} TessHeapBatchStats;

/* Allocate an empty heap batch in the configured context. */
extern TessHeapBatch *tess_heap_batch_create(const TessHeapBatchConfig *config);

/*
 * Drop the previous rows and start an empty batch. The caller must first
 * take a previously returned batch off its slot binding, which releases
 * it; a batch never released is released here.
 */
extern void tess_heap_batch_reset(TessHeapBatch *batch);

/* True after capacity rows were appended or the batch was finished. */
extern bool tess_heap_batch_is_full(const TessHeapBatch *batch);

/*
 * Append one row: a reference into the page of a buffer heap tuple slot,
 * or a copy of any other slot's tuple. The slot's descriptor is the
 * batch's from the first row on; the slot may be reused afterwards.
 */
extern void tess_heap_batch_append_slot(TessHeapBatch *batch,
										TupleTableSlot *slot);

/*
 * Append one tuple by its header: a reference into the page of buffer,
 * which the batch pins, or a copy when buffer is invalid. The descriptor
 * must be known, from the configuration or an earlier slot.
 */
extern void tess_heap_batch_append_tuple(TessHeapBatch *batch,
										 const HeapTupleData *tuple,
										 Buffer buffer);

/*
 * Append n tuples of the page of buffer, which the batch pins once, by
 * their line pointers: the visible tuples of a page as a scan lists them.
 * The descriptor must be known; n rows must fit.
 */
extern void tess_heap_batch_append_page(TessHeapBatch *batch, Buffer buffer,
										BlockNumber block,
										const OffsetNumber *offsets, int n,
										Oid table_oid);

/*
 * Finish the batch and return it, or NULL without rows. The batch stays
 * valid until it is released; finishing again returns the same batch.
 */
extern TessBatch *tess_heap_batch_finish(TessHeapBatch *batch, Oid table_oid);

extern const TessHeapBatchStats *tess_heap_batch_stats(const TessHeapBatch *batch);

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

/*
 * Clauses over a node's batches, applied in the planner's order, each
 * over the rows the ones before it kept: those the expression compiler
 * takes for whole batches (tessera/expr.h) as batch filters, the others
 * row by row through ExecQual, each row shown in the scan slot with the
 * attributes the clauses read taken from the batch's columns. Clauses of
 * one kind in a row form a stage. A clause's Var is an attribute of the
 * scan tuple, which the scan tuple layout maps to a batch column.
 * Applying narrows the batch's selection. See docs/runtime.md.
 */
typedef struct TessQual TessQual;

typedef struct TessQualConfig
{
	Size		struct_size;
	/* Owns the qual and its compiled clauses. */
	MemoryContext parent_context;
	/* The node: supplies Params and compiles the row-wise clauses. */
	PlanState  *parent;
	/* Clauses tess_expr_supports_filter accepted, in evaluation order. */
	List	   *batch_clauses;
	/* The others, in evaluation order. */
	List	   *row_clauses;
	/* A virtual slot of the scan tuple, for the row-wise clauses. */
	TupleTableSlot *scan_slot;
	/* The batch column of each scan tuple attribute. */
	const TessLayout *scan_tuple;
	/*
	 * The evaluation order of all the clauses, an IntList: 1 takes the
	 * next batch clause, 0 the next row-wise one.
	 */
	List	   *order;
} TessQualConfig;

#define TESS_QUAL_CONFIG_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessQualConfig, order)

typedef struct TessQualStats
{
	/* Rows the batch clauses removed, and the row-wise ones. */
	uint64		batch_removed;
	uint64		row_removed;
} TessQualStats;

extern TessQual *tess_qual_create(const TessQualConfig *config);

/* The batch columns the clauses read, for the request to the producer. */
extern const Bitmapset *tess_qual_columns(const TessQual *qual);

/*
 * Keep in the batch's selection, of rows rows, the rows every clause
 * holds for, and return their count. The caller resets econtext.
 */
extern int	tess_qual_apply(TessQual *qual, TessBatch *batch,
							ExprContext *econtext, int rows);

extern const TessQualStats *tess_qual_stats(const TessQual *qual);

/*
 * A step tess_qual_apply takes once per batch right before the first
 * row-wise clause: it may only remove rows of the batch, and returns how
 * many remain.
 */
typedef int (*TessQualPrefilter) (void *arg, TessBatch *batch, int rows);

/* Whether any clause runs row by row. */
extern bool tess_qual_has_row_clauses(const TessQual *qual);

/* Set the step before the row-wise clauses, or remove it with NULL. */
extern void tess_qual_set_row_prefilter(TessQual *qual, TessQualPrefilter prefilter,
										void *arg);

/*
 * The output side of a node: a virtual slot bound to the bridge through
 * which batches are published to a batch-aware parent and rows are served
 * to an ordinary one. Publishing leaves the slot non-empty: in row mode it
 * shows the batch's first selected row, in batch mode an all-NULL row, as
 * a batch-aware parent finds the binding through the slot and reads the
 * batch there; a row-wise one gets further rows with tess_output_select.
 * In batch mode publishing adds the batch's other rows to the node's
 * instrumentation, since one ExecProcNode call returns them all. See
 * docs/runtime.md.
 */
typedef struct TessOutput TessOutput;

/*
 * Bind slot, which must be virtual, with layout: every slot attribute must
 * map to a batch column. ps may be NULL; it supplies the instrumentation
 * the executor allocates after BeginCustomScan.
 */
extern TessOutput *tess_output_create(MemoryContext parent_context,
									  PlanState *ps, TupleTableSlot *slot,
									  const TessLayout *layout);

/* The binding through which a parent configures this node's request. */
extern TessBinding *tess_output_binding(TessOutput *output);

/* Freeze and return the request. */
extern const TessRequest *tess_output_request(TessOutput *output);

/*
 * Return the previous batch to its owner once the parent finished it; an
 * unfinished batch is an error. Call before reusing the batch's storage.
 */
extern void tess_output_release(TessOutput *output);

/*
 * Publish a batch with at least one selected row and return the slot,
 * non-empty: showing the first selected row in row mode, all NULL in
 * batch mode. Releases the previous batch first.
 */
extern TupleTableSlot *tess_output_publish(TessOutput *output, TessBatch *batch);

/* Show another selected row of the active batch for a row-wise parent. */
extern TupleTableSlot *tess_output_select(TessOutput *output, int row);

/* Mark the active batch consumed on behalf of a row-wise parent; repeatable. */
extern void tess_output_finish(TessOutput *output);

/* True when no unconsumed batch remains. */
extern bool tess_output_finished(TessOutput *output);

/* Release any active batch, finished or not, and clear the slot: end, rescan. */
extern void tess_output_clear(TessOutput *output);

/* Clear and detach the binding; the output is unusable afterwards. */
extern void tess_output_end(TessOutput *output);

/*
 * The input side of a node over one batch-producing child. The child's
 * request is configured through the binding of its result slot, which a
 * batch node attaches in BeginCustomScan; batches come from ExecProcNode,
 * through the binding of whatever slot the child returns, so a node that
 * forwards its own child's batch may return that child's slot. The input
 * owns neither the child nor the batches. See docs/runtime.md.
 */
typedef struct TessInput TessInput;

/* Wrap an initialized child; its result slot must carry a binding. */
extern TessInput *tess_input_create(MemoryContext parent_context, PlanState *child);

/* The child's logical layout, owned by the bridge. */
extern const TessLayout *tess_input_layout(TessInput *input);

/* The binding through which the child's request is configured. */
extern TessBinding *tess_input_binding(TessInput *input);

/* Send the request; only before the child publishes its first batch. */
extern void tess_input_set_request(TessInput *input, const TessRequest *request);

/*
 * Fetch the child's next batch, or NULL at the end of its input. The
 * previous batch must have been finished. The batch belongs to the child.
 */
extern TessBatch *tess_input_next(TessInput *input);

/* The slot that carried the active batch, or NULL. */
extern TupleTableSlot *tess_input_slot(TessInput *input);

/* Whether the active batch has been marked consumed; error without one. */
extern bool tess_input_finished(TessInput *input);

/* Mark the active batch consumed, unless a forwarding parent already did. */
extern void tess_input_finish(TessInput *input);

/* Forget cached and active pointers after the caller rescanned the child. */
extern void tess_input_rescan(TessInput *input);

/*
 * Hand the child a key filter (tessera/node.h), or take it back with
 * NULL: true when the child's node kind takes it. The child applies it to
 * the batches it returns from then on; the caller keeps the filter valid
 * until it takes it back.
 */
extern bool tess_input_set_key_filter(TessInput *input,
									  const TessKeyFilter *filter);

/*
 * The node kind of a batch node's execution state built by the plan
 * helpers, or NULL for any other state.
 */
extern const TessNode *tess_batch_node_of(PlanState *state);

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
 * EndCustomScan: the leader sums the rows if they are still mapped, since
 * the segment outlives the children of a Gather, and cancels the detach
 * callback; a worker forgets its row.
 */
extern void tess_shared_stats_end(TessSharedStats *stats);

#endif							/* TESSERA_RUNTIME_H */

/* Runtime helpers for batch nodes, linked as libtessera_runtime.a. */
#ifndef TESSERA_RUNTIME_H
#define TESSERA_RUNTIME_H

#include "postgres.h"

#include "access/tupdesc.h"
#include "executor/tuptable.h"
#include "nodes/execnodes.h"
#include "storage/dsm.h"
#include "storage/sharedfileset.h"

#include "tessera/abi.h"
#include "tessera/batch.h"
#include "tessera/binding.h"
#include "tessera/bridge.h"
#include "tessera/sort.h"
#include "tessera/spill.h"

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

/*
 * Temporary files of a node that spills (docs/spill.md): one set per level
 * of partitioning, a file per partition, created on its first block. A
 * block is a header (tessera/spill.h) and a chunk of the node's table. A
 * serial set writes PostgreSQL's temporary files, deleted when the set
 * frees them or the query's resources are released; a shared set writes
 * the participant's files of a SharedFileSet in the query's shared
 * memory, named "<name>.<participant>.<partition>", which every
 * participant reads once the writer finished them and which are deleted
 * when the last participant detaches the segment. temp_file_limit and
 * temp_tablespaces apply as to every temporary file. A set writes, then,
 * after tess_spill_finish, reads; the files are this module's only calls
 * of PostgreSQL's file layer.
 */
typedef struct TessSpill TessSpill;
typedef struct TessSpillReader TessSpillReader;

typedef struct TessSpillConfig
{
	Size		struct_size;
	/* Owns the set, its readers and the files' buffers. */
	MemoryContext parent_context;
	/* Lay out and check the blocks' headers. */
	const TessKernelOps *kernels;
	int			npartitions;
	/* The level of partitioning, below 32, written into every header. */
	uint32		level;
	/* The table's layout fingerprint (tess_table_fingerprint). */
	uint64		fingerprint;
	/* The longest body a block may have: longer ones are an error. */
	uint64		max_len;
	/* NULL for a serial set; else the query's file set and the names. */
	SharedFileSet *shared;
	/* This participant's number in the shared set's file names. */
	int			participant;
	/* The prefix of the shared set's names, unique in its file set. */
	const char *name;
} TessSpillConfig;

#define TESS_SPILL_CONFIG_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessSpillConfig, name)

/* Where a block starts in its file: a file of segments and a byte in one. */
typedef struct TessSpillPosition
{
	int			segment;
	int64		offset;
} TessSpillPosition;

/*
 * The query's file set of shared sets, in the node's chunk of shared
 * memory: the leader lays it out in InitializeDSMCustomScan, a worker
 * attaches in InitializeWorkerCustomScan; the files are deleted when the
 * last participant detaches the segment.
 */
extern void tess_spill_shared_init(SharedFileSet *shared,
								   dsm_segment *segment);
extern void tess_spill_shared_attach(SharedFileSet *shared,
									 dsm_segment *segment);

/* A set of no files yet. */
extern TessSpill *tess_spill_create(const TessSpillConfig *config);

/*
 * Write a block of len bytes at body to the partition's file, its header
 * naming kind and number; its start into position unless that is NULL.
 * A chunk of records is stored packed when that makes it shorter
 * (tess_spill_pack), and reads back whole. Returns the bytes on disk,
 * header included.
 */
extern Size tess_spill_write(TessSpill *spill, int partition,
							 TessSpillKind kind, uint32 number,
							 const void *body, Size len,
							 TessSpillPosition *position);

/* End the writes: a shared set's files become readable by every participant. */
extern void tess_spill_finish(TessSpill *spill);

/*
 * After tess_spill_finish: a reader of the partition's file that the
 * participant wrote, at its first block, or NULL when that participant
 * wrote no block to it. A serial set reads only its own participant's.
 */
extern TessSpillReader *tess_spill_open(TessSpill *spill, int participant,
										int partition);

/*
 * The next block's header, checked against the set's fingerprint and
 * longest body; false at the end of the file. A damaged header is an
 * ERROR.
 */
extern bool tess_spill_read_header(TessSpillReader *reader,
								   TessSpillHeader *header);

/* The body of the block whose header was just read, header->len bytes. */
extern void tess_spill_read_body(TessSpillReader *reader, void *body,
								 Size len);

/* Move to the block written at position; the next read is its header. */
extern void tess_spill_seek(TessSpillReader *reader,
							TessSpillPosition position);

/* Release the reader; the file stays. */
extern void tess_spill_close(TessSpillReader *reader);

/*
 * Delete this participant's file of the partition, which no reader may
 * be reading any more; its blocks stay counted.
 */
extern void tess_spill_drop(TessSpill *spill, int partition);

/* The blocks and bytes this participant wrote and the files it has open. */
extern void tess_spill_stats(const TessSpill *spill, uint64 *blocks,
							 uint64 *bytes, int *files);

/* Delete this participant's files and release the set. */
extern void tess_spill_free(TessSpill *spill);

/*
 * Release the set, closing this participant's files: a shared set's files
 * stay for the other participants to read until the file set is deleted
 * (SharedFileSetDeleteAll, or its segment's last detach); a serial set's
 * are deleted, as tess_spill_free does.
 */
extern void tess_spill_release(TessSpill *spill);

/*
 * Rows a node keeps, such as the input of a sort: records of the kernels'
 * table format (tessera/table.h) in chunks of the node's memory, never
 * moved, each with the row's keys and a payload of its kept columns, a
 * word of their NULL bits and then a Datum each; a by-reference value is
 * copied into value chunks of its own and its word is a reference to it
 * (the chunk's number plus one, and the byte), so that a chunk means the
 * same wherever it is read, spilled blocks included. A record is named by
 * its 32-bit reference, which tess_rows_append returns and every gather
 * takes. The records are not linked: there is an index only of the
 * layout, which the kernels check on every call. Serial only, in the
 * caller's memory. (TessHashJoin keeps its rows the same way with code of
 * its own, which shares them between processes and partitions.)
 */
typedef struct TessRows TessRows;

typedef struct TessRowsConfig
{
	Size		struct_size;
	/* Owns the rows, their chunks and their values. */
	MemoryContext parent_context;
	/* The table's kernels: size, create, chunk_init, append_columns, gather_scattered. */
	const TessKernelOps *kernels;
	/* The keys every record holds, in their slots: one at least. */
	int			nkeys;
	const TessTableKeyKind *kinds;
	/* The kept columns, at most TESS_ROWS_MAX_COLUMNS, and their types. */
	int			ncolumns;
	const int16 *typlens;
	const bool *typbyvals;
} TessRowsConfig;

#define TESS_ROWS_CONFIG_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessRowsConfig, typbyvals)

/* A word of NULL bits holds the kept columns': at most 64. */
#define TESS_ROWS_MAX_COLUMNS 64

/* Empty rows. */
extern TessRows *tess_rows_create(const TessRowsConfig *config);

/*
 * Append the rows of rows as records: keys are the rows' nkeys keys, as a
 * table takes them, and columns their kept columns, each over the same
 * rows; refs[row] receives the reference of each row's record. Chunks
 * are added as the records need; a by-reference value is copied, an
 * expanded object flattened.
 */
extern void tess_rows_append(TessRows *rows, const TessTableKey *keys,
							 const TessDatumColumn *columns,
							 const TessRowMask *mask, uint32 *refs);

/*
 * Kept column `column` of the records refs[row] for each row of mask into
 * values and isnull: a by-reference value as the address of its copy,
 * valid as long as the rows. Other rows keep their values; their NULL
 * flags may be set false, for all mask->nrows rows at once.
 */
extern void tess_rows_gather(TessRows *rows, int column, const uint32 *refs,
							 const TessRowMask *mask, Datum *values,
							 bool *isnull);

/*
 * The references of every record, ordered by keys, one per key of the
 * rows in key order (tessera/sort.h), into refs, which holds
 * tess_rows_count of them. The items are allocated for the call and
 * freed; the kernels need the sort's operations.
 */
extern void tess_rows_sort(TessRows *rows, const TessSortKey *keys,
						   uint32 *refs);

/*
 * Whether a row of the mask is NULL by its flag in isnull, one per
 * physical row: for a caller that keeps a column's NULLs apart.
 */
extern bool tess_rows_selected_null(const TessRowMask *mask, const bool *isnull);

/* The records appended, and the bytes the rows take: chunks, values and index. */
extern uint64 tess_rows_count(const TessRows *rows);
extern Size tess_rows_memory(const TessRows *rows);

/* Forget every record, keeping nothing but the layout. */
extern void tess_rows_reset(TessRows *rows);

/* Release the rows and their memory. */
extern void tess_rows_free(TessRows *rows);

#endif							/* TESSERA_RUNTIME_H */

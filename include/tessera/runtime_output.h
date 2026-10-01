/* The output side of a batch node. Part of tessera/runtime.h. */
#ifndef TESSERA_RUNTIME_OUTPUT_H
#define TESSERA_RUNTIME_OUTPUT_H

#include "postgres.h"

#include "executor/tuptable.h"
#include "nodes/execnodes.h"

#include "tessera/batch.h"
#include "tessera/binding.h"
#include "tessera/layout.h"
#include "tessera/request.h"

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

/*
 * The column of each of the first ntargets targets of layout for the
 * selected rows of batch, into columns: what a node serving rows one at a
 * time reads once per batch, then copies row by row. A column the batch
 * gives without values, flags or its row count is an error.
 */
extern void tess_batch_target_columns(TessBatch *batch, const TessLayout *layout,
									  int ntargets, TessDatumColumn *columns);

/* Mark the active batch consumed on behalf of a row-wise parent; repeatable. */
extern void tess_output_finish(TessOutput *output);

/* True when no unconsumed batch remains. */
extern bool tess_output_finished(TessOutput *output);

/* Release any active batch, finished or not, and clear the slot: end, rescan. */
extern void tess_output_clear(TessOutput *output);

/* Clear and detach the binding; the output is unusable afterwards. */
extern void tess_output_end(TessOutput *output);

#endif							/* TESSERA_RUNTIME_OUTPUT_H */

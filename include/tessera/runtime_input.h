/*
 * The input side of a batch node over one batch child. Part of
 * tessera/runtime.h.
 */
#ifndef TESSERA_RUNTIME_INPUT_H
#define TESSERA_RUNTIME_INPUT_H

#include "postgres.h"

#include "executor/tuptable.h"
#include "nodes/execnodes.h"

#include "tessera/batch.h"
#include "tessera/binding.h"
#include "tessera/layout.h"
#include "tessera/node.h"
#include "tessera/request.h"

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
 * NULL: true when the child's node kind takes it, false for a child that
 * is not a batch node or whose kind has no callback. The child applies it
 * to the batches it returns from then on; the caller keeps the filter's
 * words valid until it takes it back, the struct and its arrays for the
 * call only. A filter smaller than TESS_KEY_FILTER_MIN_SIZE is an error.
 */
extern bool tess_input_set_key_filter(TessInput *input,
									  const TessKeyFilter *filter);

/*
 * The node kind of a batch node's execution state built by the plan
 * helpers, or NULL for any other state.
 */
extern const TessNode *tess_batch_node_of(PlanState *state);

/*
 * Pass a bound to a child, as ExecSetTupleBound would; a batch node built
 * by the plan helpers takes it through its kind's set_tuple_bound.
 */
extern void tess_set_child_bound(PlanState *child, int64 bound);

#endif							/* TESSERA_RUNTIME_INPUT_H */

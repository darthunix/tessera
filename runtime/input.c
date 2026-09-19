#include "postgres.h"

#include "executor/executor.h"
#include "utils/memutils.h"

#include "tessera/runtime.h"

struct TessInput
{
	const TessBindingOps *ops;
	PlanState  *child;
	/* The child's own binding, where its request is configured. */
	TessBinding *request_binding;
	/* The last slot the child returned and its binding: a forwarding child
	 * returns another node's slot. */
	TupleTableSlot *cached_slot;
	TessBinding *cached_binding;
	TupleTableSlot *active_slot;
	TessBinding *active_binding;
	TessBatch  *active_batch;
};

TessInput *
tess_input_create(MemoryContext parent_context, PlanState *child)
{
	const TessApi *api = tess_runtime_api();
	TessBinding *binding = NULL;
	TessInput  *input;

	if (parent_context == NULL || child == NULL)
		elog(ERROR, "Tessera input requires a context and a child");
	if (child->ps_ResultTupleSlot != NULL)
		binding = api->binding_ops->find(child->ps_ResultTupleSlot);
	if (binding == NULL)
		elog(ERROR, "Tessera child is not a batch node");
	input = MemoryContextAllocZero(parent_context, sizeof(*input));
	input->ops = api->binding_ops;
	input->child = child;
	input->request_binding = binding;
	return input;
}

const TessLayout *
tess_input_layout(TessInput *input)
{
	return input->ops->get_layout(input->request_binding);
}

TessBinding *
tess_input_binding(TessInput *input)
{
	return input->request_binding;
}

void
tess_input_set_request(TessInput *input, const TessRequest *request)
{
	input->ops->set_request(input->request_binding, request);
}

static void
forget_active(TessInput *input)
{
	input->active_slot = NULL;
	input->active_binding = NULL;
	input->active_batch = NULL;
}

TessBatch *
tess_input_next(TessInput *input)
{
	TupleTableSlot *slot;
	TessBatch  *batch;

	if (input->active_batch != NULL)
	{
		/* A forwarding parent may have finished this batch directly. */
		if (!input->ops->is_consumed(input->active_binding))
			elog(ERROR, "Tessera input fetched a new batch before finishing the previous one");
		forget_active(input);
	}
	slot = ExecProcNode(input->child);
	if (TupIsNull(slot))
		return NULL;
	if (slot != input->cached_slot)
	{
		input->cached_slot = slot;
		input->cached_binding = input->ops->find(slot);
	}
	if (input->cached_binding == NULL)
		elog(ERROR, "Tessera child returned a slot without a batch binding");
	batch = input->ops->get_batch(input->cached_binding);
	if (batch == NULL)
		elog(ERROR, "Tessera child returned no batch");
	input->active_slot = slot;
	input->active_binding = input->cached_binding;
	input->active_batch = batch;
	return batch;
}

TupleTableSlot *
tess_input_slot(TessInput *input)
{
	return input->active_slot;
}

bool
tess_input_finished(TessInput *input)
{
	if (input->active_batch == NULL)
		elog(ERROR, "Tessera input has no active batch");
	return input->ops->is_consumed(input->active_binding);
}

void
tess_input_finish(TessInput *input)
{
	if (input->active_batch == NULL)
		elog(ERROR, "Tessera input has no active batch");
	if (!input->ops->is_consumed(input->active_binding))
		input->ops->mark_consumed(input->active_binding);
	forget_active(input);
}

void
tess_input_rescan(TessInput *input)
{
	input->cached_slot = NULL;
	input->cached_binding = NULL;
	forget_active(input);
}

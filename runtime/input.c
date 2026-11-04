#include "postgres.h"

#include "executor/executor.h"
#include "utils/memutils.h"

#include "tessera/plan.h"
#include "tessera/planner.h"
#include "tessera/runtime.h"

#include "gathered.h"

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
	MemoryContext context;
	/* The request's sets of columns, copied, and its rows a batch. */
	Bitmapset  *filter_columns;
	Bitmapset  *projection_columns;
	int			max_batch_rows;
	bool		has_request;
	/*
	 * Gathering (tess_input_set_gather): on for the scans to come, and for
	 * this one, taken at its first batch; whether this scan fetched its
	 * first batch, and saw the child's end; the
	 * gathered batch, active with no binding and consumed by its flag; a
	 * child batch held back, unfinished, to go after the gathered rows or
	 * to give the rest of its rows; the rows copied over the input's life.
	 */
	bool		gather;
	bool		gathering;
	bool		scan_started;
	bool		child_done;
	TessGathered *gathered;
	bool		gathered_consumed;
	TessBatch  *held_batch;
	TessBinding *held_binding;
	TupleTableSlot *held_slot;
	uint64		gathered_rows;
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
	input->context = parent_context;
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
tess_set_child_bound(PlanState *child, int64 bound)
{
	const TessNode *node = tess_batch_node_of(child);

	if (node != NULL && TESS_ABI_HAS_FIELD(node, TessNode, set_tuple_bound) &&
		node->set_tuple_bound != NULL)
		node->set_tuple_bound((CustomScanState *) child, bound);
	else
		ExecSetTupleBound(bound, child);
}

void
tess_input_set_request(TessInput *input, const TessRequest *request)
{
	MemoryContext old;

	input->ops->set_request(input->request_binding, request);
	/* A gathered batch holds the request's columns: the sets are only lent. */
	old = MemoryContextSwitchTo(input->context);
	bms_free(input->filter_columns);
	bms_free(input->projection_columns);
	input->filter_columns = bms_copy(request->filter_columns);
	input->projection_columns = bms_copy(request->projection_columns);
	MemoryContextSwitchTo(old);
	input->max_batch_rows = request->max_batch_rows;
	input->has_request = true;
	input->gathered = NULL;
}

void
tess_input_set_gather(TessInput *input, bool on)
{
	input->gather = on;
}

uint64
tess_input_gathered_rows(TessInput *input)
{
	return input->gathered_rows;
}

const TessNode *
tess_batch_node_of(PlanState *state)
{
	CustomScan *scan;
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	const char *kind;

	if (!IsA(state, CustomScanState) || state->plan == NULL ||
		!IsA(state->plan, CustomScan))
		return NULL;
	scan = (CustomScan *) state->plan;
	kind = tess_plan_data_kind(scan->custom_private);
	if (kind == NULL || strcmp(kind, "tessera.plan") != 0)
		return NULL;
	tess_plan_get_info(scan, &info);
	pfree(info.child_names);
	if (info.layout.target_columns != NULL)
		pfree((void *) info.layout.target_columns);
	return info.node;
}

bool
tess_input_set_key_filter(TessInput *input, const TessKeyFilter *filter)
{
	const TessNode *node = tess_batch_node_of(input->child);

	if (filter != NULL && filter->struct_size < TESS_KEY_FILTER_MIN_SIZE)
		elog(ERROR, "Tessera key filter is smaller than its required fields");
	if (node == NULL || !TESS_ABI_HAS_FIELD(node, TessNode, set_key_filter) ||
		node->set_key_filter == NULL)
		return false;
	return node->set_key_filter((CustomScanState *) input->child, filter);
}

static void
forget_active(TessInput *input)
{
	input->active_slot = NULL;
	input->active_binding = NULL;
	input->active_batch = NULL;
}

/* The child's next batch, its slot and binding, or NULL at its end. */
static TessBatch *
fetch(TessInput *input, TupleTableSlot **slot_out, TessBinding **binding_out)
{
	TupleTableSlot *slot;
	TessBatch  *batch;

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
	*slot_out = slot;
	*binding_out = input->cached_binding;
	return batch;
}

static TessBatch *
activate(TessInput *input, TessBatch *batch, TupleTableSlot *slot, TessBinding *binding)
{
	input->active_slot = slot;
	input->active_binding = binding;
	input->active_batch = batch;
	return batch;
}

static TessBatch *
activate_gathered(TessInput *input)
{
	input->gathered_consumed = false;
	input->active_slot = NULL;
	input->active_binding = NULL;
	input->active_batch = tess_gathered_batch(input->gathered);
	return input->active_batch;
}

/*
 * The next batch while gathering: the scan's first batch as it is; then
 * a child batch at least half full, or one whose columns are not stored
 * values, as it is, after the rows gathered before it; the others' rows
 * copied into the gathered batch until it is full, a batch that does not
 * fit giving the rows that fit and holding back its rest.
 */
static TessBatch *
gather_next(TessInput *input)
{
	if (input->gathered == NULL)
	{
		int			capacity = 64;

		if (!input->has_request)
			elog(ERROR, "Tessera input gathers only after its request is set");

		if (input->max_batch_rows > 0 && input->max_batch_rows < capacity)
			capacity = input->max_batch_rows;
		input->gathered = tess_gathered_create(input->context, tess_input_layout(input),
											   input->child->ps_ResultTupleSlot->tts_tupleDescriptor,
											   input->filter_columns, input->projection_columns,
											   capacity);
	}
	for (;;)
	{
		TessGathered *gathered = input->gathered;
		TupleTableSlot *slot;
		TessBinding *binding;
		TessBatch  *batch;
		int			count;
		int			taken;

		if (input->held_batch != NULL)
		{
			batch = input->held_batch;
			slot = input->held_slot;
			binding = input->held_binding;
			input->held_batch = NULL;
		}
		else
		{
			batch = input->child_done ? NULL : fetch(input, &slot, &binding);
			if (batch == NULL)
			{
				input->child_done = true;
				return tess_gathered_rows(gathered) > 0 ? activate_gathered(input) : NULL;
			}
		}
		count = tess_row_mask_count(&batch->rows);
		if (count == 0)
		{
			input->ops->mark_consumed(binding);
			continue;
		}
		if (!tess_batch_values_stored(batch) ||
			count * 2 >= tess_gathered_capacity(gathered))
		{
			if (tess_gathered_rows(gathered) == 0)
				return activate(input, batch, slot, binding);
			input->held_batch = batch;
			input->held_slot = slot;
			input->held_binding = binding;
			return activate_gathered(input);
		}
		taken = tess_gathered_take(gathered, batch);
		input->gathered_rows += taken;
		if (taken < count)
		{
			/* Split: the rest waits, unfinished, for the next batch. */
			input->held_batch = batch;
			input->held_slot = slot;
			input->held_binding = binding;
			return activate_gathered(input);
		}
		input->ops->mark_consumed(binding);
		if (tess_gathered_rows(gathered) == tess_gathered_capacity(gathered))
			return activate_gathered(input);
	}
}

TessBatch *
tess_input_next(TessInput *input)
{
	TupleTableSlot *slot;
	TessBinding *binding;
	TessBatch  *batch;

	if (input->active_batch != NULL)
	{
		/* A forwarding parent may have finished this batch directly. */
		if (input->active_binding == NULL ? !input->gathered_consumed :
			!input->ops->is_consumed(input->active_binding))
			elog(ERROR, "Tessera input fetched a new batch before finishing the previous one");
		forget_active(input);
	}
	if (input->gathering && input->scan_started)
		return gather_next(input);
	/* A scan's first batch goes as it is, whatever the gathering. */
	input->scan_started = true;
	input->gathering = input->gather;
	batch = fetch(input, &slot, &binding);
	return batch == NULL ? NULL : activate(input, batch, slot, binding);
}

TupleTableSlot *
tess_input_slot(TessInput *input)
{
	return input->gather || input->gathering ? NULL : input->active_slot;
}

bool
tess_input_finished(TessInput *input)
{
	if (input->active_batch == NULL)
		elog(ERROR, "Tessera input has no active batch");
	if (input->active_binding == NULL)
		return input->gathered_consumed;
	return input->ops->is_consumed(input->active_binding);
}

void
tess_input_finish(TessInput *input)
{
	if (input->active_batch == NULL)
		elog(ERROR, "Tessera input has no active batch");
	if (input->active_binding == NULL)
	{
		/* The gathered batch: its rows and values go, for the next one. */
		input->gathered_consumed = true;
		tess_gathered_reset(input->gathered);
	}
	else if (!input->ops->is_consumed(input->active_binding))
		input->ops->mark_consumed(input->active_binding);
	forget_active(input);
}

void
tess_input_rescan(TessInput *input)
{
	input->cached_slot = NULL;
	input->cached_binding = NULL;
	forget_active(input);
	/* A held child batch is void: the child's rescan clears its output. */
	input->held_batch = NULL;
	input->held_slot = NULL;
	input->held_binding = NULL;
	if (input->gathered != NULL)
		tess_gathered_reset(input->gathered);
	input->scan_started = false;
	input->child_done = false;
}

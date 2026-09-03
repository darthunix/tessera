#include "postgres.h"

#include "executor/tuptable.h"
#include "lib/ilist.h"
#include "nodes/bitmapset.h"
#include "utils/memutils.h"

#include "internal.h"

struct TessBinding
{
	/* Entry in the backend-local list searched by find_binding(). */
	dlist_node	link;
	TupleTableSlot *slot;
	MemoryContext context;
	MemoryContextCallback cleanup;
	TessLayout	layout;
	TessRequest request;
	TessBatch   *batch;
	bool		request_frozen;
};

static dlist_head bindings = DLIST_STATIC_INIT(bindings);

static TessBinding *attach(TupleTableSlot *slot, const TessLayout *layout);
static TessBinding *find_binding(TupleTableSlot *slot);
static void set_request(TessBinding *binding, const TessRequest *request);
static const TessLayout *get_layout(TessBinding *binding);
static const TessRequest *freeze_request(TessBinding *binding);
static void publish_batch(TessBinding *binding, TessBatch *batch);
static TessBatch *get_batch(TessBinding *binding);
static void release_batch(TessBinding *binding);
static void detach(TessBinding *binding);

const TessBindingOps tess_binding_ops = {
	TESS_ABI_INITIALIZER(TESS_BINDING_OPS_ABI_VERSION, TessBindingOps),
	.attach = attach,
	.find = find_binding,
	.set_request = set_request,
	.get_layout = get_layout,
	.freeze_request = freeze_request,
	.publish_batch = publish_batch,
	.get_batch = get_batch,
	.release_batch = release_batch,
	.detach = detach,
};

static void
validate_layout(const TessLayout *layout)
{
	int			target;

	if (layout == NULL || layout->struct_size < TESS_LAYOUT_MIN_SIZE ||
		layout->ncolumns < 0 || layout->ntargets < 0)
		elog(ERROR, "Tessera received an invalid layout");
	if (layout->target_columns == NULL)
	{
		if (layout->ntargets > layout->ncolumns)
			elog(ERROR, "Tessera identity layout has too many targets");
		return;
	}
	for (target = 0; target < layout->ntargets; target++)
	{
		int			column = layout->target_columns[target];

		if (column < -1 || column >= layout->ncolumns)
			elog(ERROR, "Tessera layout column %d is out of range", column);
	}
}

static void
validate_request(const TessBinding *binding, const TessRequest *request)
{
	int			column;

	if (request == NULL || request->struct_size < TESS_REQUEST_MIN_SIZE)
		elog(ERROR, "Tessera received an invalid request");
	if (request->output_mode != TESS_OUTPUT_ROWS &&
		request->output_mode != TESS_OUTPUT_BATCH)
		elog(ERROR, "Tessera request has an invalid output mode");
	if (request->max_batch_rows < 0)
		elog(ERROR, "Tessera batch row limit cannot be negative");
	column = bms_next_member(request->filter_columns,
							 binding->layout.ncolumns - 1);
	if (column >= 0)
		elog(ERROR, "Tessera filter column %d is out of range", column);
	column = bms_next_member(request->projection_columns,
							 binding->layout.ncolumns - 1);
	if (column >= 0)
		elog(ERROR, "Tessera projection column %d is out of range", column);
}

static void
validate_batch(const TessBinding *binding, const TessBatch *batch)
{
	int			nwords;
	int			tail_bits;

	if (batch == NULL)
		elog(ERROR, "Tessera cannot publish a null batch");
	if (batch->abi_version != TESS_BATCH_ABI_VERSION ||
		batch->struct_size < TESS_BATCH_MIN_SIZE)
		elog(ERROR, "Tessera received an incompatible batch");
	if (batch->ops == NULL ||
		batch->ops->abi_version != TESS_BATCH_OPS_ABI_VERSION ||
		batch->ops->struct_size < TESS_BATCH_OPS_MIN_SIZE)
		elog(ERROR, "Tessera received incompatible batch operations");
	if (batch->ops->get_datum_column == NULL)
		elog(ERROR, "Tessera batch cannot provide Datum columns");
	if (batch->rows.nrows <= 0 || batch->rows.bits == NULL)
		elog(ERROR, "Tessera batch has an invalid row mask");
	nwords = tess_row_mask_word_count(batch->rows.nrows);
	tail_bits = batch->rows.nrows % 64;
	if (tail_bits != 0 &&
		(batch->rows.bits[nwords - 1] >> tail_bits) != 0)
		elog(ERROR, "Tessera batch selects rows beyond its physical size");
	if (binding->request.max_batch_rows > 0 &&
		batch->rows.nrows > binding->request.max_batch_rows)
		elog(ERROR, "Tessera batch exceeds the requested row limit");
}

static TessBinding *
find_binding(TupleTableSlot *slot)
{
	dlist_iter	iter;

	if (slot == NULL)
		return NULL;
	dlist_foreach(iter, &bindings)
	{
		TessBinding *binding = dlist_container(TessBinding, link, iter.cur);

		if (binding->slot == slot)
			return binding;
	}
	return NULL;
}

static void
binding_reset(void *arg)
{
	TessBinding *binding = arg;

	dlist_delete(&binding->link);
}

static TessBinding *
attach(TupleTableSlot *slot, const TessLayout *layout)
{
	TessBinding *binding;
	int		   *target_columns = NULL;

	if (slot == NULL || slot->tts_mcxt == NULL)
		elog(ERROR, "Tessera cannot attach to an invalid slot");
	validate_layout(layout);
	if (find_binding(slot) != NULL)
		elog(ERROR, "Tessera slot is already attached");

	binding = MemoryContextAllocZero(slot->tts_mcxt, sizeof(*binding));
	if (layout->target_columns != NULL && layout->ntargets > 0)
	{
		target_columns = MemoryContextAlloc(slot->tts_mcxt,
											sizeof(int) * layout->ntargets);
		memcpy(target_columns, layout->target_columns,
			   sizeof(int) * layout->ntargets);
	}
	binding->slot = slot;
	binding->context = slot->tts_mcxt;
	binding->layout = *layout;
	binding->layout.struct_size = sizeof(binding->layout);
	binding->layout.target_columns = target_columns;
	binding->request.struct_size = sizeof(binding->request);
	binding->request.output_mode = TESS_OUTPUT_ROWS;
	binding->cleanup.func = binding_reset;
	binding->cleanup.arg = binding;
	dlist_push_tail(&bindings, &binding->link);
	MemoryContextRegisterResetCallback(binding->context, &binding->cleanup);
	return binding;
}

static void
set_request(TessBinding *binding, const TessRequest *request)
{
	MemoryContext oldcontext;
	Bitmapset  *filter_columns;
	Bitmapset  *projection_columns;

	if (binding == NULL)
		elog(ERROR, "Tessera cannot configure a null binding");
	if (binding->request_frozen)
		elog(ERROR, "Tessera request is already frozen");
	validate_request(binding, request);

	oldcontext = MemoryContextSwitchTo(binding->context);
	filter_columns = bms_copy(request->filter_columns);
	projection_columns = bms_copy(request->projection_columns);
	MemoryContextSwitchTo(oldcontext);

	bms_free((Bitmapset *) binding->request.filter_columns);
	bms_free((Bitmapset *) binding->request.projection_columns);
	binding->request.struct_size = sizeof(binding->request);
	binding->request.filter_columns = filter_columns;
	binding->request.projection_columns = projection_columns;
	binding->request.output_mode = request->output_mode;
	binding->request.max_batch_rows = request->max_batch_rows;
}

static const TessLayout *
get_layout(TessBinding *binding)
{
	if (binding == NULL)
		elog(ERROR, "Tessera cannot inspect a null binding");
	return &binding->layout;
}

static const TessRequest *
freeze_request(TessBinding *binding)
{
	if (binding == NULL)
		elog(ERROR, "Tessera cannot freeze a null binding");
	binding->request_frozen = true;
	return &binding->request;
}

static void
publish_batch(TessBinding *binding, TessBatch *batch)
{
	if (binding == NULL)
		elog(ERROR, "Tessera cannot publish to a null binding");
	if (binding->batch != NULL)
		elog(ERROR, "Tessera binding already has an active batch");
	validate_batch(binding, batch);
	binding->request_frozen = true;
	binding->batch = batch;
}

static TessBatch *
get_batch(TessBinding *binding)
{
	return binding == NULL ? NULL : binding->batch;
}

static void
release_batch(TessBinding *binding)
{
	TessBatch   *batch;

	if (binding == NULL || binding->batch == NULL)
		return;
	batch = binding->batch;
	binding->batch = NULL;
	if (TESS_ABI_HAS_FIELD(batch->ops, TessBatchOps, release) &&
		batch->ops->release != NULL)
		batch->ops->release(batch);
}

static void
detach(TessBinding *binding)
{
	if (binding == NULL)
		return;
	release_batch(binding);
	MemoryContextUnregisterResetCallback(binding->context,
										 &binding->cleanup);
	dlist_delete(&binding->link);
	bms_free((Bitmapset *) binding->request.filter_columns);
	bms_free((Bitmapset *) binding->request.projection_columns);
	if (binding->layout.target_columns != NULL)
		pfree((void *) binding->layout.target_columns);
	pfree(binding);
}

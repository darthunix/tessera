#include "postgres.h"

#include "executor/executor.h"
#include "utils/memutils.h"

#include "tessera/expr.h"
#include "tessera/runtime.h"

/* One computed column: a batch chain over the child's columns. */
typedef struct Computed
{
	TessExpr   *chain;
	bool		chain_done;
} Computed;

struct TessProjection
{
	TessBatch	batch;
	TessBatch  *child;
	/* By-reference results of the wrapped batch; reset when it is released. */
	MemoryContext context;
	ExprContext *econtext;
	TupleTableSlot *scan_slot;
	int			base_columns;
	Computed   *computed;
	int			ncomputed;
	TessProjectionStats stats;
};

/* A Var of the scan tuple names a batch column through the scan layout. */
static int
resolve_scan_var(const Var *var, void *context)
{
	const TessLayout *tuple = context;

	if (var->varattno < 1 || var->varattno > tuple->ntargets)
		return -1;
	return tess_layout_column(tuple, var->varattno - 1);
}

static void
projection_get_datum_column(TessBatch *batch, int column,
							const TessRowMask *rows, TessColumnPurpose purpose,
							TessDatumColumn *result);
static void projection_release(TessBatch *batch);

static const TessBatchOps projection_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = projection_get_datum_column,
	.release = projection_release,
};


/* Compile one target; the executor's row-wise path follows. */
static void
init_computed(Computed *computed, Node *expr, const TessProjectionConfig *config)
{
	if (!tess_expr_supports_value(expr, 0))
		elog(ERROR, "Tessera projection computes batch expressions only");
	computed->chain = tess_expr_compile_value(expr, config->parent,
											  resolve_scan_var,
											  (void *) config->scan_tuple);
}

TessProjection *
tess_projection_create(const TessProjectionConfig *config)
{
	TessProjection *projection;
	int			index = 0;

	if (config == NULL || config->struct_size < TESS_PROJECTION_CONFIG_MIN_SIZE ||
		config->parent_context == NULL || config->econtext == NULL ||
		config->scan_slot == NULL || config->scan_tuple == NULL)
		elog(ERROR, "Tessera projection requires a context, an expression context, a scan slot and a scan tuple layout");
	if (config->base_columns < 0 || config->computed == NIL)
		elog(ERROR, "Tessera projection requires base columns and computed targets");
	projection = MemoryContextAllocZero(config->parent_context, sizeof(*projection));
	projection->context = AllocSetContextCreate(config->parent_context,
												"Tessera projection values",
												ALLOCSET_DEFAULT_SIZES);
	projection->econtext = config->econtext;
	projection->scan_slot = config->scan_slot;
	projection->base_columns = config->base_columns;
	projection->ncomputed = list_length(config->computed);
	projection->computed = MemoryContextAllocZero(config->parent_context,
												  sizeof(Computed) * projection->ncomputed);
	foreach_ptr(TargetEntry, entry, config->computed)
		init_computed(&projection->computed[index++], (Node *) entry->expr, config);
	projection->batch.abi_version = TESS_BATCH_ABI_VERSION;
	projection->batch.struct_size = sizeof(TessBatch);
	projection->batch.ops = &projection_ops;
	projection->batch.private_data = projection;
	return projection;
}


TessBatch *
tess_projection_wrap(TessProjection *projection, TessBatch *child)
{
	if (child == NULL)
		elog(ERROR, "Tessera projection cannot wrap a null batch");
	if (projection->child != NULL)
		elog(ERROR, "Tessera projection still wraps a batch");
	for (int index = 0; index < projection->ncomputed; index++)
		projection->computed[index].chain_done = false;
	projection->child = child;
	/* The rows are the child's: a consumer narrowing them narrows both. */
	projection->batch.rows = child->rows;
	projection->batch.table_oid = child->table_oid;
	return &projection->batch;
}

/* The chain over the child's selected rows, once per batch. */
static void
compute_chain(TessProjection *projection, Computed *computed,
			  TessColumnPurpose purpose, TessDatumColumn *result)
{
	const TessDatumColumn *column;

	if (!computed->chain_done)
	{
		tess_expr_bind(computed->chain, projection->child, projection->econtext,
					   purpose);
		computed->chain_done = true;
		projection->stats.chain_datums +=
			tess_row_mask_count(&projection->child->rows);
	}
	column = tess_expr_get_column(computed->chain);
	result->values = column->values;
	result->isnull = column->isnull;
}


static void
projection_get_datum_column(TessBatch *batch, int column,
							const TessRowMask *rows, TessColumnPurpose purpose,
							TessDatumColumn *result)
{
	TessProjection *projection = batch->private_data;
	Computed   *computed;

	if (result == NULL || result->struct_size < TESS_DATUM_COLUMN_MIN_SIZE)
		elog(ERROR, "Tessera projection received an incompatible column request");
	if (projection->child == NULL)
		elog(ERROR, "Tessera projection wraps no batch");
	if (column < 0 || column >= projection->base_columns + projection->ncomputed)
		elog(ERROR, "Tessera projection column is out of range");
	if (rows == NULL || rows->nrows != batch->rows.nrows)
		elog(ERROR, "Tessera projection received a row mask of another batch");
	if (column < projection->base_columns)
	{
		projection->child->ops->get_datum_column(projection->child, column, rows,
												 purpose, result);
		return;
	}
	computed = &projection->computed[column - projection->base_columns];
	compute_chain(projection, computed, purpose, result);
	result->nrows = batch->rows.nrows;
}

static void
projection_release(TessBatch *batch)
{
	TessProjection *projection = batch->private_data;

	if (projection->child != NULL && projection->child->ops->release != NULL)
		projection->child->ops->release(projection->child);
	projection->child = NULL;
	MemoryContextReset(projection->context);
}

void
tess_projection_reset(TessProjection *projection)
{
	projection->child = NULL;
	MemoryContextReset(projection->context);
}

const TessProjectionStats *
tess_projection_stats(const TessProjection *projection)
{
	return &projection->stats;
}

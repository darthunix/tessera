#include "postgres.h"

#include "executor/executor.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/optimizer.h"
#include "port/pg_bitutils.h"
#include "utils/expandeddatum.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"

#include "tessera/expr.h"
#include "tessera/runtime.h"

/* One computed column: a batch chain, or the executor's expression. */
typedef struct Computed
{
	TessExpr   *chain;
	ExprState  *state;
	/* Scan tuple attributes the expression reads, and their base columns. */
	int		   *atts;
	int		   *columns;
	TessDatumColumn *inputs;
	int			natts;
	int16		typlen;
	/* Results by physical row; done marks the rows computed row by row. */
	Datum	   *values;
	bool	   *isnull;
	uint64	   *done;
	bool		chain_done;
} Computed;

struct TessProjection
{
	TessBatch	batch;
	TessBatch  *child;
	/* Evaluates the chains' scalars. */
	ExprContext *econtext;
	/*
	 * Evaluates the row-wise expressions: its per-tuple memory holds their
	 * results and scratch for the wrapped batch, reset when it is released.
	 */
	ExprContext *row_econtext;
	MemoryContext context;
	TupleTableSlot *scan_slot;
	int			base_columns;
	Computed   *computed;
	int			ncomputed;
	int			capacity;
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

/* Compile one target: a chain when the compiler takes it, else the executor. */
static void
init_computed(Computed *computed, Node *expr, const TessProjectionConfig *config)
{
	List	   *vars = pull_var_clause(expr, 0);
	int			natts = 0;

	if (tess_expr_supports_value(expr, 0))
		computed->chain = tess_expr_compile_value(expr, config->parent,
												  resolve_scan_var,
												  (void *) config->scan_tuple);
	else
		computed->state = ExecInitExpr((Expr *) expr, config->parent);
	computed->atts = palloc_array(int, list_length(vars) + 1);
	computed->columns = palloc_array(int, list_length(vars) + 1);
	foreach_ptr(Var, var, vars)
	{
		int			column = resolve_scan_var(var, (void *) config->scan_tuple);
		int			index;

		if (column < 0)
			elog(ERROR, "Tessera projection reads scan tuple attribute %d without a batch column",
				 var->varattno);
		for (index = 0; index < natts && computed->atts[index] != var->varattno - 1; index++)
			;
		if (index == natts)
		{
			computed->atts[natts] = var->varattno - 1;
			computed->columns[natts++] = column;
		}
	}
	computed->inputs = palloc_array(TessDatumColumn, natts + 1);
	computed->natts = natts;
	computed->typlen = get_typlen(exprType(expr));
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
	projection->econtext = config->econtext;
	if (config->parent != NULL)
		projection->row_econtext = CreateExprContext(config->parent->state);
	else
	{
		MemoryContext oldcontext = MemoryContextSwitchTo(config->parent_context);

		projection->row_econtext = CreateStandaloneExprContext();
		MemoryContextSwitchTo(oldcontext);
	}
	projection->row_econtext->ecxt_scantuple = config->scan_slot;
	projection->context = projection->row_econtext->ecxt_per_tuple_memory;
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

/* Result arrays for nrows rows of every computed column. */
static void
ensure_capacity(TessProjection *projection, int nrows)
{
	MemoryContext parent = GetMemoryChunkContext(projection);

	if (projection->capacity >= nrows)
		return;
	for (int index = 0; index < projection->ncomputed; index++)
	{
		Computed   *computed = &projection->computed[index];

		if (computed->chain != NULL)
			continue;
		if (computed->values != NULL)
		{
			pfree(computed->values);
			pfree(computed->isnull);
			pfree(computed->done);
		}
		computed->values = MemoryContextAllocZero(parent, sizeof(Datum) * nrows);
		computed->isnull = MemoryContextAllocZero(parent, sizeof(bool) * nrows);
		computed->done = MemoryContextAllocZero(parent,
												sizeof(uint64) * tess_row_mask_word_count(nrows));
	}
	projection->capacity = nrows;
}

TessBatch *
tess_projection_wrap(TessProjection *projection, TessBatch *child)
{
	if (child == NULL)
		elog(ERROR, "Tessera projection cannot wrap a null batch");
	if (projection->child != NULL)
		elog(ERROR, "Tessera projection still wraps a batch");
	ensure_capacity(projection, child->rows.nrows);
	for (int index = 0; index < projection->ncomputed; index++)
	{
		Computed   *computed = &projection->computed[index];

		computed->chain_done = false;
		if (computed->done != NULL)
			memset(computed->done, 0,
				   sizeof(uint64) * tess_row_mask_word_count(child->rows.nrows));
	}
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

/*
 * The executor's expression over the requested rows not computed yet. It
 * is evaluated in the projection's per-batch memory, so a by-reference
 * result is handed out as it is, without a copy, and lives until the
 * wrapper is released; a result that is a read-write expanded object is
 * made read-only, as the executor's projection makes its results.
 */
static void
compute_rows(TessProjection *projection, Computed *computed,
			 const TessRowMask *rows, TessDatumColumn *result)
{
	TupleTableSlot *slot = projection->scan_slot;
	ExprContext *econtext = projection->row_econtext;
	TessDatumColumn *inputs = computed->inputs;
	int			nwords = tess_row_mask_word_count(rows->nrows);
	bool		pending = false;

	for (int word = 0; word < nwords; word++)
		pending |= (rows->bits[word] & ~computed->done[word]) != 0;
	if (!pending)
		return;
	for (int index = 0; index < computed->natts; index++)
	{
		inputs[index] = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
		projection->child->ops->get_datum_column(projection->child,
												 computed->columns[index], rows,
												 TESS_COLUMN_FOR_PROJECTION,
												 &inputs[index]);
	}
	ExecClearTuple(slot);
	memset(slot->tts_isnull, true, slot->tts_tupleDescriptor->natts);
	ExecStoreVirtualTuple(slot);
	for (int word = 0; word < nwords; word++)
	{
		uint64		todo = rows->bits[word] & ~computed->done[word];

		while (todo != 0)
		{
			int			row = word * 64 + pg_rightmost_one_pos64(todo);
			Datum		value;
			bool		isnull;

			for (int index = 0; index < computed->natts; index++)
			{
				slot->tts_values[computed->atts[index]] = inputs[index].values[row];
				slot->tts_isnull[computed->atts[index]] = inputs[index].isnull[row];
			}
			value = ExecEvalExprSwitchContext(computed->state, econtext, &isnull);
			if (!isnull && computed->typlen == -1)
				value = MakeExpandedObjectReadOnly(value, isnull, -1);
			computed->values[row] = isnull ? (Datum) 0 : value;
			computed->isnull[row] = isnull;
			projection->stats.row_datums++;
			todo &= todo - 1;
		}
		computed->done[word] |= rows->bits[word];
	}
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
	if (computed->chain != NULL)
		compute_chain(projection, computed, purpose, result);
	else
	{
		compute_rows(projection, computed, rows, result);
		result->values = computed->values;
		result->isnull = computed->isnull;
	}
	result->nrows = batch->rows.nrows;
}

/* The child's batch is the node's to finish: the wrapper only forgets it. */
static void
projection_release(TessBatch *batch)
{
	TessProjection *projection = batch->private_data;

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

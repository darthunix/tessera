#include "postgres.h"

#include "executor/executor.h"
#include "optimizer/optimizer.h"

#include "tessera/expr.h"
#include "tessera/runtime.h"

struct TessQual
{
	/* The batch clauses, in the planner's order. */
	TessExpr  **filters;
	int			nfilters;
	/* The row-wise clauses, and the scan slot they read. */
	ExprState  *row_qual;
	TupleTableSlot *scan_slot;
	/* The scan tuple attributes the row-wise clauses read, and their columns. */
	int		   *atts;
	int		   *att_columns;
	int			natts;
	TessDatumColumn *columns;
	/* Every batch column any clause reads. */
	Bitmapset  *read_columns;
	TessQualStats stats;
};

/* A Var of the scan tuple is a position in the scan tuple's layout. */
static int
resolve_column(const Var *var, void *context)
{
	return tess_layout_column((const TessLayout *) context, var->varattno - 1);
}

TessQual *
tess_qual_create(const TessQualConfig *config)
{
	MemoryContext oldcontext;
	TessQual   *qual;
	Bitmapset  *atts = NULL;
	int			att = -1;
	int			index = 0;

	if (config == NULL || config->struct_size < TESS_QUAL_CONFIG_MIN_SIZE ||
		config->parent_context == NULL || config->parent == NULL ||
		config->scan_tuple == NULL ||
		(config->row_clauses != NIL && config->scan_slot == NULL))
		elog(ERROR, "Tessera qual received an incomplete configuration");
	oldcontext = MemoryContextSwitchTo(config->parent_context);
	qual = palloc0_object(TessQual);
	qual->nfilters = list_length(config->batch_clauses);
	qual->filters = palloc_array(TessExpr *, Max(qual->nfilters, 1));
	foreach_ptr(Node, clause, config->batch_clauses)
	{
		qual->filters[index++] = tess_expr_compile_filter(clause, config->parent,
														  resolve_column,
														  (void *) config->scan_tuple);
		/* Every column the chain reads, an operand of a step included. */
		foreach_ptr(Var, var, pull_var_clause(clause, 0))
			qual->read_columns = bms_add_member(qual->read_columns,
												resolve_column(var, (void *) config->scan_tuple));
	}
	if (config->row_clauses != NIL)
	{
		qual->row_qual = ExecInitQual(config->row_clauses, config->parent);
		qual->scan_slot = config->scan_slot;
		foreach_ptr(Var, var, pull_var_clause((Node *) config->row_clauses, 0))
			atts = bms_add_member(atts, var->varattno - 1);
		qual->natts = bms_num_members(atts);
		qual->atts = palloc_array(int, Max(qual->natts, 1));
		qual->att_columns = palloc_array(int, Max(qual->natts, 1));
		qual->columns = palloc_array(TessDatumColumn, Max(qual->natts, 1));
		index = 0;
		while ((att = bms_next_member(atts, att)) >= 0)
		{
			qual->atts[index] = att;
			qual->att_columns[index] = tess_layout_column(config->scan_tuple, att);
			if (qual->att_columns[index] < 0)
				elog(ERROR, "Tessera qual reads an attribute with no batch column");
			qual->read_columns = bms_add_member(qual->read_columns,
												qual->att_columns[index]);
			index++;
		}
		/* The attributes the clauses do not read are never looked at. */
		memset(qual->scan_slot->tts_isnull, true,
			   qual->scan_slot->tts_tupleDescriptor->natts);
	}
	MemoryContextSwitchTo(oldcontext);
	return qual;
}

const Bitmapset *
tess_qual_columns(const TessQual *qual)
{
	return qual->read_columns;
}

/*
 * The row-wise clauses over the rows the batch clauses kept: each row is
 * shown to ExecQual through the scan tuple slot, whose attributes the
 * clauses read come from the batch's columns; nothing is allocated.
 */
static int
apply_rows(TessQual *qual, TessBatch *batch, ExprContext *econtext, int kept)
{
	TupleTableSlot *slot = qual->scan_slot;
	int			row = -1;

	for (int index = 0; index < qual->natts; index++)
	{
		qual->columns[index] = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
		batch->ops->get_datum_column(batch, qual->att_columns[index],
									 &batch->rows, TESS_COLUMN_FOR_FILTER,
									 &qual->columns[index]);
		if (qual->columns[index].values == NULL ||
			qual->columns[index].isnull == NULL ||
			qual->columns[index].nrows != batch->rows.nrows)
			elog(ERROR, "Tessera batch returned an invalid column");
	}
	ExecClearTuple(slot);
	ExecStoreVirtualTuple(slot);
	econtext->ecxt_scantuple = slot;
	while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
	{
		for (int index = 0; index < qual->natts; index++)
		{
			int			att = qual->atts[index];

			slot->tts_values[att] = qual->columns[index].values[row];
			slot->tts_isnull[att] = qual->columns[index].isnull[row];
		}
		if (!ExecQual(qual->row_qual, econtext))
		{
			tess_row_mask_clear(&batch->rows, row);
			kept--;
		}
	}
	return kept;
}

int
tess_qual_apply(TessQual *qual, TessBatch *batch, ExprContext *econtext,
				int rows)
{
	int			kept = rows;
	int			batch_kept;

	for (int index = 0; index < qual->nfilters && kept > 0; index++)
	{
		tess_expr_bind(qual->filters[index], batch, econtext,
					   TESS_COLUMN_FOR_FILTER);
		tess_expr_apply_filter(qual->filters[index]);
		kept = tess_row_mask_count(&batch->rows);
	}
	batch_kept = kept;
	qual->stats.batch_removed += rows - batch_kept;
	if (qual->row_qual != NULL && kept > 0)
		kept = apply_rows(qual, batch, econtext, kept);
	qual->stats.row_removed += batch_kept - kept;
	return kept;
}

const TessQualStats *
tess_qual_stats(const TessQual *qual)
{
	return &qual->stats;
}

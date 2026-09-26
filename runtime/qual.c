#include "postgres.h"

#include "executor/executor.h"
#include "optimizer/optimizer.h"

#include "tessera/expr.h"
#include "tessera/runtime.h"

/*
 * Clauses of one kind next to each other in the evaluation order: batch
 * filters, or row-wise clauses under one ExprState.
 */
typedef struct QualStage
{
	TessExpr  **filters;
	int			nfilters;
	ExprState  *row_qual;
} QualStage;

struct TessQual
{
	/* The stages, in the planner's order. */
	QualStage  *stages;
	int			nstages;
	/* The scan slot the row-wise clauses read. */
	TupleTableSlot *scan_slot;
	/* The scan tuple attributes the row-wise clauses read, and their columns. */
	int		   *atts;
	int		   *att_columns;
	int			natts;
	TessDatumColumn *columns;
	/* Every batch column any clause reads. */
	Bitmapset  *read_columns;
	TessQualStats stats;
	/* The first stage of row-wise clauses, or -1, and the step before it. */
	int			first_row_stage;
	TessQualPrefilter prefilter;
	void	   *prefilter_arg;
};

/* A Var of the scan tuple is a position in the scan tuple's layout. */
static int
resolve_column(const Var *var, void *context)
{
	return tess_layout_column((const TessLayout *) context, var->varattno - 1);
}

/* The batch clause, compiled, and the columns its chain reads. */
static TessExpr *
compile_filter(TessQual *qual, Node *clause, const TessQualConfig *config)
{
	foreach_ptr(Var, var, pull_var_clause(clause, 0))
		qual->read_columns = bms_add_member(qual->read_columns,
											resolve_column(var, (void *) config->scan_tuple));
	return tess_expr_compile_filter(clause, config->parent, resolve_column,
									(void *) config->scan_tuple);
}

/* The scan tuple attributes every row-wise clause reads, and their columns. */
static void
map_row_attributes(TessQual *qual, const TessQualConfig *config)
{
	Bitmapset  *atts = NULL;
	int			att = -1;
	int			index = 0;

	foreach_ptr(Var, var, pull_var_clause((Node *) config->row_clauses, 0))
		atts = bms_add_member(atts, var->varattno - 1);
	qual->scan_slot = config->scan_slot;
	qual->natts = bms_num_members(atts);
	qual->atts = palloc_array(int, Max(qual->natts, 1));
	qual->att_columns = palloc_array(int, Max(qual->natts, 1));
	qual->columns = palloc_array(TessDatumColumn, Max(qual->natts, 1));
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

TessQual *
tess_qual_create(const TessQualConfig *config)
{
	MemoryContext oldcontext;
	TessQual   *qual;
	int			nclauses;
	int			nbatch = 0;
	int			nrow = 0;

	if (config == NULL || config->struct_size < TESS_QUAL_CONFIG_MIN_SIZE ||
		config->parent_context == NULL || config->parent == NULL ||
		config->scan_tuple == NULL ||
		(config->row_clauses != NIL && config->scan_slot == NULL))
		elog(ERROR, "Tessera qual received an incomplete configuration");
	nclauses = list_length(config->order);
	foreach_int(batch, config->order)
	{
		if (batch != 0)
			nbatch++;
		else
			nrow++;
	}
	if (nbatch != list_length(config->batch_clauses) ||
		nrow != list_length(config->row_clauses))
		elog(ERROR, "Tessera qual order does not match its clauses");
	oldcontext = MemoryContextSwitchTo(config->parent_context);
	qual = palloc0_object(TessQual);
	qual->stages = palloc0_array(QualStage, Max(nclauses, 1));
	qual->first_row_stage = -1;
	if (config->row_clauses != NIL)
		map_row_attributes(qual, config);
	nbatch = 0;
	nrow = 0;
	/* A stage per run of clauses of one kind. */
	for (int first = 0; first < nclauses;)
	{
		int			batch = list_nth_int(config->order, first);
		int			end = first;
		QualStage  *stage = &qual->stages[qual->nstages++];

		while (end < nclauses && list_nth_int(config->order, end) == batch)
			end++;
		if (batch != 0)
		{
			stage->filters = palloc_array(TessExpr *, end - first);
			for (int index = first; index < end; index++)
				stage->filters[stage->nfilters++] =
					compile_filter(qual, list_nth(config->batch_clauses, nbatch++),
								   config);
		}
		else
		{
			List	   *clauses = NIL;

			for (int index = first; index < end; index++)
				clauses = lappend(clauses, list_nth(config->row_clauses, nrow++));
			stage->row_qual = ExecInitQual(clauses, config->parent);
			if (qual->first_row_stage < 0)
				qual->first_row_stage = qual->nstages - 1;
		}
		first = end;
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
 * A stage's row-wise clauses over the rows kept so far: each row is shown
 * to ExecQual through the scan tuple slot, whose attributes the clauses
 * read come from the batch's columns; nothing is allocated.
 */
static int
apply_rows(TessQual *qual, ExprState *row_qual, TessBatch *batch,
		   ExprContext *econtext, int kept)
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
		if (!ExecQual(row_qual, econtext))
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

	for (int index = 0; index < qual->nstages && kept > 0; index++)
	{
		QualStage  *stage = &qual->stages[index];
		int			before = kept;

		if (stage->row_qual != NULL)
		{
			if (qual->prefilter != NULL && index == qual->first_row_stage)
			{
				kept = qual->prefilter(qual->prefilter_arg, batch, kept);
				if (kept == 0)
					break;
				before = kept;
			}
			kept = apply_rows(qual, stage->row_qual, batch, econtext, kept);
			qual->stats.row_removed += before - kept;
			continue;
		}
		for (int filter = 0; filter < stage->nfilters && kept > 0; filter++)
		{
			tess_expr_bind(stage->filters[filter], batch, econtext,
						   TESS_COLUMN_FOR_FILTER);
			tess_expr_apply_filter(stage->filters[filter]);
			kept = tess_row_mask_count(&batch->rows);
		}
		qual->stats.batch_removed += before - kept;
	}
	return kept;
}

bool
tess_qual_has_row_clauses(const TessQual *qual)
{
	return qual->first_row_stage >= 0;
}

void
tess_qual_set_row_prefilter(TessQual *qual, TessQualPrefilter prefilter, void *arg)
{
	qual->prefilter = prefilter;
	qual->prefilter_arg = arg;
}

const TessQualStats *
tess_qual_stats(const TessQual *qual)
{
	return &qual->stats;
}

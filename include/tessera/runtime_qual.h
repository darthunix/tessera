/*
 * A node's clauses over its batches, in batches and row by row. Part of
 * tessera/runtime.h.
 */
#ifndef TESSERA_RUNTIME_QUAL_H
#define TESSERA_RUNTIME_QUAL_H

#include "postgres.h"

#include "nodes/execnodes.h"

#include "tessera/abi.h"
#include "tessera/batch.h"
#include "tessera/layout.h"

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

#endif							/* TESSERA_RUNTIME_QUAL_H */

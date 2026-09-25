/* Batch expressions: one column, scalars, and the function registry. */
#ifndef TESSERA_EXPR_H
#define TESSERA_EXPR_H

#include "postgres.h"

#include "nodes/execnodes.h"
#include "nodes/primnodes.h"

#include "tessera/batch.h"
#include "tessera/row_mask.h"

/*
 * A batch expression is a PostgreSQL expression over columns of a batch:
 * Vars, Const and Param scalars, and calls of functions that the function
 * registry implements (see docs/function.md), nested so that a column
 * flows through a chain of calls, a call's other argument being a scalar
 * or a value over the batch compiled as an expression of its own. A filter
 * may also be a condition: AND, OR, NOT, null and boolean tests and short
 * constant IN lists over such filters, in three-valued logic. Nothing
 * else: no general intermediate representation, no non-strict functions. A
 * value expression yields a column of results with a mask of the non-NULL
 * rows; a filter is a registered predicate applied to such a value and a
 * scalar, narrowing the batch's row mask in place. See docs/expr.md.
 */
typedef struct TessExpr TessExpr;

/* Map a Var of the node's scan tuple to a batch column, or -1. */
typedef int (*TessExprResolveVar) (const Var *var, void *context);

/*
 * Planning time, without executor state. relid 0 accepts a Var of any one
 * relation; otherwise every Var must belong to relid.
 */
extern bool tess_expr_supports_value(Node *node, Index relid);
extern bool tess_expr_supports_filter(Node *node, Index relid);

/*
 * Compile in the caller's memory context; parent supplies the Params. The
 * expression must satisfy tess_expr_supports_value.
 */
extern TessExpr *tess_expr_compile_value(Node *node, PlanState *parent,
										 TessExprResolveVar resolve,
										 void *context);

/*
 * A filter is a value expression under a registered predicate with a
 * scalar or another value; it must satisfy tess_expr_supports_filter.
 */
extern TessExpr *tess_expr_compile_filter(Node *node, PlanState *parent,
										  TessExprResolveVar resolve,
										  void *context);

/* The batch column the expression reads, or -1 for a scalar expression. */
extern int tess_expr_input_column(const TessExpr *expr);

/*
 * Bind to a batch for its selected rows; results are computed on the
 * first request and kept until the next bind. Scalars are evaluated in
 * econtext at every computation, so a changed Param takes effect after a
 * rebind.
 */
extern void tess_expr_bind(TessExpr *expr, TessBatch *batch,
						   ExprContext *econtext, TessColumnPurpose purpose);

/*
 * The results over the batch's selected rows, as the batch contract's
 * Datum column: one slot per batch row, NULL flags set, borrowed until
 * the next bind. A failed function call raises its SQLSTATE and message.
 */
extern const TessDatumColumn *tess_expr_get_column(TessExpr *expr);

/* The selected rows whose result is not NULL. */
extern const TessRowMask *tess_expr_non_nulls(TessExpr *expr);

/*
 * Narrow the bound batch's row mask in place to the selected rows where
 * the filter is true; a NULL value or scalar makes it false. The value
 * is computed over the selection as it was before.
 */
extern void tess_expr_apply_filter(TessExpr *expr);

#endif							/* TESSERA_EXPR_H */

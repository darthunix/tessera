/* Batch expressions: one column, scalars, and the function registry. */
#ifndef TESSERA_EXPR_H
#define TESSERA_EXPR_H

#include "postgres.h"

#include "nodes/execnodes.h"
#include "nodes/primnodes.h"

/*
 * A batch expression is a PostgreSQL expression over at most one column
 * of a batch: a Var, Const and Param scalars, and calls of functions that
 * the function registry implements (see docs/function.md), nested so that
 * one column flows through a chain of calls. Nothing else: no general
 * intermediate representation, no AND or OR, no non-strict functions. A
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

/*
 * Compile in the caller's memory context; parent supplies the Params. The
 * expression must satisfy tess_expr_supports_value.
 */
extern TessExpr *tess_expr_compile_value(Node *node, PlanState *parent,
										 TessExprResolveVar resolve,
										 void *context);

/* The batch column the expression reads, or -1 for a scalar expression. */
extern int tess_expr_input_column(const TessExpr *expr);

#endif							/* TESSERA_EXPR_H */

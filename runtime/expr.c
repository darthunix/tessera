#include "postgres.h"

#include "catalog/pg_type_d.h"
#include "utils/array.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/optimizer.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"

#include "tessera/expr.h"
#include "tessera/function.h"
#include "tessera/runtime.h"

#define MAX_ARGS 2
/* The most elements of an IN list compiled as an OR of comparisons. */
#define MAX_ARRAY_ELEMENTS 32

/* One registered call of the chain; args[column_arg] is the column. */
typedef struct Step
{
	const TessFunction *function;
	Oid			inputcollid;
	int			nargs;
	int			column_arg;
	/*
	 * An argument that is another value over the batch, or -1: a bare
	 * column or an expression of its own, computed over the same rows.
	 */
	int			column_operand;
	TessExpr   *operand;
	/* The other arguments, evaluated per computation. */
	ExprState  *scalars[MAX_ARGS];
} Step;

/* A node of a condition over the batch, in three-valued logic. */
typedef enum CondKind
{
	COND_LEAF,					/* a filter: a predicate over a value */
	COND_AND,
	COND_OR,
	COND_NOT,
	COND_NULL_TEST,				/* IS [NOT] NULL over a value */
	COND_BOOL_TEST				/* IS [NOT] TRUE, FALSE or UNKNOWN */
} CondKind;

typedef struct Cond
{
	CondKind	kind;
	/* LEAF: the filter; NULL_TEST: the value tested. */
	TessExpr   *expr;
	bool		is_null;
	BoolTestType test;
	struct Cond **args;
	int			nargs;
	/*
	 * Per evaluation over a selection: the rows where the condition is
	 * true, those where it is unknown (NULL), and scratch for the rows a
	 * child is evaluated over.
	 */
	TessRowMask truth;
	TessRowMask unknown;
	TessRowMask rest;
	TessRowMask all_true;
	int			capacity;
} Cond;

struct TessExpr
{
	MemoryContext context;
	/* The batch column the chain starts from, or -1 with scalar_value. */
	int			column;
	ExprState  *scalar_value;
	Step	   *steps;
	int			nsteps;
	/* The predicate over the chain's result, when the expression is a filter. */
	bool		filter;
	Step		predicate;
	/* A filter that is a condition of several parts instead: its root. */
	Cond	   *cond;
	/* The bound batch, and the selection computed over: its rows or a subset. */
	TessBatch  *batch;
	TessRowMask *rows;
	ExprContext *econtext;
	TessColumnPurpose purpose;
	/* Two sets of scratch arrays: the steps alternate between them. */
	int			capacity;
	Datum	   *values[2];
	bool	   *isnull[2];
	int32	   *ints[2];
	uint64	   *bits[2];
	/* The results, valid while ready. */
	TessDatumColumn result;
	TessRowMask non_nulls;
	bool		ready;
	/* A bare column's mask is built when asked for: a filter never asks. */
	bool		non_nulls_pending;
	TessStatus	status;
};

static bool analyze_value(Node *node, Index relid, int *nvars);

static Node *
strip_relabel(Node *node)
{
	while (node != NULL && IsA(node, RelabelType))
		node = (Node *) castNode(RelabelType, node)->arg;
	return node;
}

static bool
valid_var(const Var *var, Index relid)
{
	return var->varattno > 0 && var->varlevelsup == 0 &&
		var->varreturningtype == VAR_RETURNING_DEFAULT &&
		(relid == 0 || var->varno == relid);
}

static bool
scalar_leaf(const Node *node)
{
	if (IsA(node, Const))
		return true;
	if (IsA(node, Param))
	{
		const Param *param = (const Param *) node;

		return param->paramkind == PARAM_EXTERN ||
			param->paramkind == PARAM_EXEC;
	}
	return false;
}

/* The registered implementation behind an operator or function call. */
static const TessFunction *
call_of(Node *node, List **args, Oid *opno, Oid *inputcollid)
{
	const TessFunctionRegistryOps *functions = tess_runtime_api()->functions;

	if (IsA(node, OpExpr))
	{
		OpExpr	   *op = (OpExpr *) node;

		set_opfuncid(op);
		*args = op->args;
		*opno = op->opno;
		*inputcollid = op->inputcollid;
		return functions->find(op->opfuncid);
	}
	if (IsA(node, FuncExpr))
	{
		FuncExpr   *func = (FuncExpr *) node;

		*args = func->args;
		*opno = InvalidOid;
		*inputcollid = func->inputcollid;
		return functions->find(func->funcid);
	}
	return NULL;
}

/*
 * x op ANY (array) over a constant array of a few elements as the OR of
 * x op element, and op ALL as the AND: an element NULL makes its
 * comparison unknown, as the array operator's rules say. Anything else,
 * a parameter, a NULL or empty or long array, as it is.
 */
static Node *
array_as_clauses(ScalarArrayOpExpr *array_op)
{
	Const	   *array = (Const *) strip_relabel(lsecond(array_op->args));
	ArrayType  *elements;
	Oid			element_type;
	int16		typlen;
	bool		byval;
	char		align;
	Datum	   *values;
	bool	   *nulls;
	int			count;
	List	   *clauses = NIL;

	if (!IsA(array, Const) || array->constisnull)
		return (Node *) array_op;
	elements = DatumGetArrayTypeP(array->constvalue);
	element_type = ARR_ELEMTYPE(elements);
	get_typlenbyvalalign(element_type, &typlen, &byval, &align);
	deconstruct_array(elements, element_type, typlen, byval, align, &values,
					  &nulls, &count);
	if (count == 0 || count > MAX_ARRAY_ELEMENTS)
		return (Node *) array_op;
	for (int index = 0; index < count; index++)
	{
		Expr	   *element = (Expr *) makeConst(element_type, -1,
												 get_typcollation(element_type),
												 typlen, values[index],
												 nulls[index], byval);
		OpExpr	   *clause = (OpExpr *) make_opclause(array_op->opno, BOOLOID,
													  false,
													  (Expr *) linitial(array_op->args),
													  element, InvalidOid,
													  array_op->inputcollid);

		set_opfuncid(clause);
		clauses = lappend(clauses, clause);
	}
	return (Node *) (array_op->useOr ? make_orclause(clauses) :
					 make_andclause(clauses));
}

/*
 * The node with a call of an equivalent (a cross-type function such as
 * int48pl) replaced by the call it stands for: the equivalent function
 * over the arguments cast as the description says, a cast of a constant
 * folded here, once. Any other node as it is, without RelabelType.
 */
static Node *
expand(Node *node)
{
	const TessFunction *function;
	List	   *args;
	List	   *cast_args = NIL;
	Oid			opno;
	Oid			inputcollid;
	int			index = 0;

	node = strip_relabel(node);
	if (node == NULL)
		return NULL;
	if (IsA(node, ScalarArrayOpExpr))
		return array_as_clauses((ScalarArrayOpExpr *) node);
	function = call_of(node, &args, &opno, &inputcollid);
	if (function == NULL || function->kind != TESS_FUNCTION_EQUIVALENT ||
		function->struct_size < TESS_FUNCTION_EQUIVALENT_MIN_SIZE ||
		list_length(args) > TESS_FUNCTION_MAX_ARGS)
		return node;
	foreach_ptr(Node, arg, args)
	{
		Oid			cast = function->arg_casts[index++];
		Node	   *cast_arg = arg;

		if (OidIsValid(cast))
		{
			cast_arg = (Node *) makeFuncExpr(cast, get_func_rettype(cast),
											 list_make1(arg), InvalidOid,
											 InvalidOid, COERCE_IMPLICIT_CAST);
			if (IsA(strip_relabel(arg), Const))
				cast_arg = eval_const_expressions(NULL, cast_arg);
		}
		cast_args = lappend(cast_args, cast_arg);
	}
	return (Node *) makeFuncExpr(function->equivalent, exprType(node), cast_args,
								 exprCollation(node), inputcollid,
								 COERCE_EXPLICIT_CALL);
}

/* Strict, insensitive to the collation or given none, of the kind wanted. */
static bool
usable(const TessFunction *function, Oid inputcollid, TessFunctionKind kind)
{
	return function != NULL && function->kind == kind &&
		(function->flags & TESS_FUNCTION_STRICT) != 0 &&
		((function->flags & TESS_FUNCTION_COLLATION_INSENSITIVE) != 0 ||
		 !OidIsValid(inputcollid));
}

/* The implementation of the operator's commutator, for the column first. */
static const TessFunction *
commuted(Oid opno, Oid inputcollid, TessFunctionKind kind)
{
	const TessFunction *function;
	Oid			commutator;

	if (!OidIsValid(opno))
		return NULL;
	commutator = get_commutator(opno);
	if (!OidIsValid(commutator))
		return NULL;
	function = tess_runtime_api()->functions->find(get_opcode(commutator));
	return usable(function, inputcollid, kind) ? function : NULL;
}

/*
 * Whether every argument is a supported value, how many of them hold a
 * column, which argument carries the chain's column and which, if any,
 * is the call's other operand over the batch, compiled as an expression
 * of its own: a bare Var when one side is, so that the chain follows the
 * computed side, else the second argument.
 */
static bool
analyze_args(List *args, Index relid, int *nvars, int *column_arg,
			 int *column_operand)
{
	int			index = 0;

	*nvars = 0;
	*column_arg = -1;
	*column_operand = -1;
	if (list_length(args) < 1 || list_length(args) > MAX_ARGS)
		return false;
	foreach_ptr(Node, arg, args)
	{
		int			arg_vars;

		if (!analyze_value(arg, relid, &arg_vars))
			return false;
		if (arg_vars > 0)
		{
			bool		bare = IsA(strip_relabel(arg), Var);

			if (*column_arg < 0)
				*column_arg = index;
			else if (*column_operand >= 0)
				return false;
			else if (bare)
				*column_operand = index;
			else if (IsA(strip_relabel(list_nth(args, *column_arg)), Var))
			{
				*column_operand = *column_arg;
				*column_arg = index;
			}
			else
				*column_operand = index;
			(*nvars)++;
		}
		index++;
	}
	return true;
}

/* Whether the column may sit at column_arg of a call of function. */
static bool
shape_fits(const TessFunction *function, int column_arg, int nargs,
		   Oid opno, Oid inputcollid)
{
	if (column_arg == 0 || (function->flags & TESS_FUNCTION_ANY_SHAPE) != 0)
		return true;
	return nargs == 2 &&
		commuted(opno, inputcollid, function->kind) != NULL;
}

/*
 * Whether node is a supported value expression, and whether it has a
 * column (nvars 1) or is a scalar (0). The column, when there is one,
 * must be the first argument of every call that takes it, unless the
 * implementation accepts any shape or the operator commutes into one
 * that does; an operand over the batch, a column or an expression,
 * needs an implementation of any shape.
 */
static bool
analyze_value(Node *node, Index relid, int *nvars)
{
	const TessFunction *function;
	List	   *args;
	Oid			opno;
	Oid			inputcollid;
	int			column_arg;
	int			column_operand;

	check_stack_depth();
	node = expand(node);
	if (node == NULL)
		return false;
	if (IsA(node, Var))
	{
		*nvars = 1;
		return valid_var((const Var *) node, relid);
	}
	if (scalar_leaf(node))
	{
		*nvars = 0;
		return true;
	}
	function = call_of(node, &args, &opno, &inputcollid);
	if (!usable(function, inputcollid, TESS_FUNCTION_VALUE) ||
		!analyze_args(args, relid, nvars, &column_arg, &column_operand))
		return false;
	if (column_operand >= 0 && (function->flags & TESS_FUNCTION_ANY_SHAPE) == 0)
		return false;
	if (*nvars == 0)
		return true;
	*nvars = 1;
	return shape_fits(function, column_arg, list_length(args), opno,
					  inputcollid);
}

/*
 * Whether node is a supported filter: a boolean call of a registered
 * predicate over one value and either a scalar or another value over the
 * batch, the operand, which the predicate must accept in any shape; with
 * a scalar the column comes first or moves first through the commutator.
 */
static bool
analyze_filter(Node *node, Index relid, int *column_arg, int *column_operand)
{
	const TessFunction *function;
	List	   *args;
	Oid			opno;
	Oid			inputcollid;
	int			nvars;

	node = expand(node);
	if (node == NULL || exprType(node) != BOOLOID)
		return false;
	function = call_of(node, &args, &opno, &inputcollid);
	if (!usable(function, inputcollid, TESS_FUNCTION_PREDICATE) ||
		list_length(args) != 2 ||
		!analyze_args(args, relid, &nvars, column_arg, column_operand) ||
		nvars < 1)
		return false;
	if (*column_operand >= 0 && (function->flags & TESS_FUNCTION_ANY_SHAPE) == 0)
		return false;
	return shape_fits(function, *column_arg, 2, opno, inputcollid);
}

bool
tess_expr_supports_value(Node *node, Index relid)
{
	int			nvars;

	return analyze_value(node, relid, &nvars);
}

/*
 * Whether node is a supported condition: a filter, or AND, OR and NOT
 * over supported conditions, IS [NOT] NULL over a supported value (a bare
 * column of any type among them), IS [NOT] TRUE, FALSE or UNKNOWN over a
 * supported condition, and an ANY or ALL over a short constant array.
 */
static bool
analyze_cond(Node *node, Index relid)
{
	int			column_arg;
	int			column_operand;
	int			nvars;

	check_stack_depth();
	node = expand(node);
	if (node == NULL)
		return false;
	if (IsA(node, BoolExpr))
	{
		foreach_ptr(Node, arg, ((BoolExpr *) node)->args)
		{
			if (!analyze_cond(arg, relid))
				return false;
		}
		return true;
	}
	if (IsA(node, NullTest))
	{
		NullTest   *test = (NullTest *) node;

		return !test->argisrow &&
			analyze_value((Node *) test->arg, relid, &nvars);
	}
	if (IsA(node, BooleanTest))
		return analyze_cond((Node *) ((BooleanTest *) node)->arg, relid);
	return analyze_filter(node, relid, &column_arg, &column_operand);
}

bool
tess_expr_supports_filter(Node *node, Index relid)
{
	return analyze_cond(node, relid);
}

static TessExpr *new_expr(Node *node, TessExprResolveVar resolve);
static void compile_value(TessExpr *expr, Node *node, PlanState *parent,
						  TessExprResolveVar resolve, void *context);
static Cond *compile_cond(Node *node, PlanState *parent,
						  TessExprResolveVar resolve, void *context);
static void bind_selection(TessExpr *expr, TessRowMask *rows);
static void eval_cond(Cond *cond, const TessRowMask *rows, bool want_unknown);

/*
 * Set up the call of node, whose column argument was compiled already;
 * the operand, when there is one, is compiled here as an expression of
 * its own.
 */
static void
init_step(Step *step, Node *node, int column_arg, int column_operand,
		  PlanState *parent, TessExprResolveVar resolve, void *context)
{
	List	   *args;
	Oid			opno;
	Oid			inputcollid;
	const TessFunction *function = call_of(node, &args, &opno, &inputcollid);
	int			index = 0;

	step->function = function;
	step->inputcollid = inputcollid;
	step->nargs = list_length(args);
	step->column_arg = column_arg;
	step->column_operand = column_operand;
	step->operand = NULL;
	if (column_operand >= 0)
	{
		Node	   *operand = list_nth(args, column_operand);

		step->operand = new_expr(operand, resolve);
		compile_value(step->operand, operand, parent, resolve, context);
	}
	if (column_arg != 0 && (function->flags & TESS_FUNCTION_ANY_SHAPE) == 0)
	{
		/* 7 - x has no commutator; 7 < x becomes x > 7 for a predicate. */
		step->function = commuted(opno, inputcollid, function->kind);
		step->column_arg = 0;
	}
	foreach_ptr(Node, arg, args)
	{
		int			position = index++;

		if (position == column_arg || position == column_operand)
			continue;
		/* With the column moved first, the scalar moves to the other side. */
		if (step->column_arg != column_arg)
			position = column_arg;
		step->scalars[position] = ExecInitExpr((Expr *) arg, parent);
	}
}

static void
append_step(TessExpr *expr, Node *node, int column_arg, int column_operand,
			PlanState *parent, TessExprResolveVar resolve, void *context)
{
	expr->steps = expr->steps == NULL ? palloc0_array(Step, 1) :
		repalloc0_array(expr->steps, Step, expr->nsteps, expr->nsteps + 1);
	init_step(&expr->steps[expr->nsteps++], node, column_arg, column_operand,
			  parent, resolve, context);
}

static void
compile_value(TessExpr *expr, Node *node, PlanState *parent,
			  TessExprResolveVar resolve, void *context)
{
	List	   *args;
	Oid			opno;
	Oid			inputcollid;
	int			nvars;
	int			column_arg;
	int			column_operand;

	check_stack_depth();
	node = expand(node);
	if (IsA(node, Var))
	{
		expr->column = resolve((const Var *) node, context);
		if (expr->column < 0)
			elog(ERROR, "Tessera expression input column is unavailable");
		return;
	}
	if (!analyze_value(node, 0, &nvars))
		elog(ERROR, "Tessera received an unsupported batch expression");
	if (nvars == 0)
	{
		expr->scalar_value = ExecInitExpr((Expr *) node, parent);
		return;
	}
	call_of(node, &args, &opno, &inputcollid);
	(void) analyze_args(args, 0, &nvars, &column_arg, &column_operand);
	compile_value(expr, list_nth(args, column_arg), parent, resolve, context);
	append_step(expr, node, column_arg, column_operand, parent, resolve,
				context);
}

static TessExpr *
new_expr(Node *node, TessExprResolveVar resolve)
{
	TessExpr   *expr;

	if (node == NULL || resolve == NULL)
		elog(ERROR, "Tessera expression requires a node and a column resolver");
	expr = palloc0_object(TessExpr);
	expr->context = CurrentMemoryContext;
	expr->column = -1;
	expr->status = (TessStatus) TESS_STRUCT_INITIALIZER(TessStatus);
	return expr;
}

TessExpr *
tess_expr_compile_value(Node *node, PlanState *parent,
						TessExprResolveVar resolve, void *context)
{
	TessExpr   *expr = new_expr(node, resolve);

	if (!tess_expr_supports_value(node, 0))
		elog(ERROR, "Tessera received an unsupported batch expression");
	compile_value(expr, node, parent, resolve, context);
	return expr;
}

/* A supported condition, compiled: its filters and values, its scratch. */
static Cond *
compile_cond(Node *node, PlanState *parent, TessExprResolveVar resolve,
			 void *context)
{
	Cond	   *cond = palloc0_object(Cond);
	int			column_arg;
	int			column_operand;

	check_stack_depth();
	node = expand(node);
	if (IsA(node, BoolExpr))
	{
		BoolExpr   *bool_expr = (BoolExpr *) node;
		int			index = 0;

		cond->kind = bool_expr->boolop == AND_EXPR ? COND_AND :
			bool_expr->boolop == OR_EXPR ? COND_OR : COND_NOT;
		cond->nargs = list_length(bool_expr->args);
		cond->args = palloc_array(Cond *, cond->nargs);
		foreach_ptr(Node, arg, bool_expr->args)
			cond->args[index++] = compile_cond(arg, parent, resolve, context);
	}
	else if (IsA(node, NullTest))
	{
		NullTest   *test = (NullTest *) node;

		cond->kind = COND_NULL_TEST;
		cond->is_null = test->nulltesttype == IS_NULL;
		cond->expr = tess_expr_compile_value((Node *) test->arg, parent,
											 resolve, context);
	}
	else if (IsA(node, BooleanTest))
	{
		BooleanTest *test = (BooleanTest *) node;

		cond->kind = COND_BOOL_TEST;
		cond->test = test->booltesttype;
		cond->nargs = 1;
		cond->args = palloc_array(Cond *, 1);
		cond->args[0] = compile_cond((Node *) test->arg, parent, resolve,
									 context);
	}
	else if (analyze_filter(node, 0, &column_arg, &column_operand))
	{
		cond->kind = COND_LEAF;
		cond->expr = tess_expr_compile_filter(node, parent, resolve, context);
	}
	else
		elog(ERROR, "Tessera received an unsupported batch condition");
	return cond;
}

TessExpr *
tess_expr_compile_filter(Node *node, PlanState *parent,
						 TessExprResolveVar resolve, void *context)
{
	TessExpr   *expr = new_expr(node, resolve);
	List	   *args;
	Oid			opno;
	Oid			inputcollid;
	int			column_arg;
	int			column_operand;

	if (!analyze_filter(node, 0, &column_arg, &column_operand))
	{
		if (!analyze_cond(node, 0))
			elog(ERROR, "Tessera received an unsupported batch filter");
		expr->cond = compile_cond(node, parent, resolve, context);
		expr->filter = true;
		return expr;
	}
	node = expand(node);
	call_of(node, &args, &opno, &inputcollid);
	compile_value(expr, list_nth(args, column_arg), parent, resolve, context);
	init_step(&expr->predicate, node, column_arg, column_operand, parent,
			  resolve, context);
	expr->filter = true;
	return expr;
}

int
tess_expr_input_column(const TessExpr *expr)
{
	return expr->column;
}

/*
 * Compute over rows, the bound batch's selection or a subset of it, from
 * now on; the operands follow, and results are computed anew.
 */
static void
bind_selection(TessExpr *expr, TessRowMask *rows)
{
	expr->rows = rows;
	expr->ready = false;
	for (int index = 0; index < expr->nsteps; index++)
		if (expr->steps[index].operand != NULL)
			bind_selection(expr->steps[index].operand, rows);
	if (expr->filter && expr->predicate.operand != NULL)
		bind_selection(expr->predicate.operand, rows);
}

/* Every filter and value of a condition, bound to the batch. */
static void
bind_cond(Cond *cond, TessBatch *batch, ExprContext *econtext,
		  TessColumnPurpose purpose)
{
	if (cond->expr != NULL)
		tess_expr_bind(cond->expr, batch, econtext, purpose);
	for (int index = 0; index < cond->nargs; index++)
		bind_cond(cond->args[index], batch, econtext, purpose);
}

void
tess_expr_bind(TessExpr *expr, TessBatch *batch, ExprContext *econtext,
			   TessColumnPurpose purpose)
{
	if (batch == NULL || econtext == NULL)
		elog(ERROR, "Tessera expression requires a batch and an expression context");
	expr->batch = batch;
	expr->econtext = econtext;
	expr->purpose = purpose;
	for (int index = 0; index < expr->nsteps; index++)
		if (expr->steps[index].operand != NULL)
			tess_expr_bind(expr->steps[index].operand, batch, econtext, purpose);
	if (expr->filter && expr->predicate.operand != NULL)
		tess_expr_bind(expr->predicate.operand, batch, econtext, purpose);
	if (expr->cond != NULL)
		bind_cond(expr->cond, batch, econtext, purpose);
	bind_selection(expr, &batch->rows);
}

/* Scratch for nrows rows; values stay initialized as placeholders. */
static void
ensure_capacity(TessExpr *expr, int nrows)
{
	MemoryContext oldcontext;
	int			nwords = tess_row_mask_word_count(nrows);

	if (expr->capacity >= nrows)
		return;
	oldcontext = MemoryContextSwitchTo(expr->context);
	for (int set = 0; set < 2; set++)
	{
		if (expr->values[set] != NULL)
		{
			pfree(expr->values[set]);
			pfree(expr->isnull[set]);
			pfree(expr->ints[set]);
			pfree(expr->bits[set]);
		}
		expr->values[set] = palloc0_array(Datum, nrows);
		expr->isnull[set] = palloc0_array(bool, nrows);
		expr->ints[set] = palloc0_array(int32, nrows);
		expr->bits[set] = palloc0_array(uint64, nwords);
	}
	expr->capacity = nrows;
	MemoryContextSwitchTo(oldcontext);
}

/*
 * Broadcast one scalar, or its NULL, into a set: every row of the batch is
 * written, as finish_step writes every row of a word, so that the fill is
 * a plain loop and two block copies; the non-NULL mask is the batch's
 * selected rows, or empty for a NULL.
 */
static void
fill_scalar(TessExpr *expr, int set, Datum value, bool isnull)
{
	const TessRowMask *rows = expr->rows;
	int			nrows = rows->nrows;
	Size		mask_size = sizeof(uint64) * tess_row_mask_word_count(nrows);
	Datum	   *values = expr->values[set];

	if (isnull)
		value = (Datum) 0;
	for (int row = 0; row < nrows; row++)
		values[row] = value;
	memset(expr->isnull[set], isnull, nrows);
	if (isnull || mask_size == 0)
		memset(expr->bits[set], 0, mask_size);
	else
		memcpy(expr->bits[set], rows->bits, mask_size);
}

/*
 * Turn a step's results into the next column: Datums and NULL flags, word
 * by word over the batch's rows rather than row by row over the selected
 * ones. Every row of a word with selected rows is written: the value is
 * widened without a condition and the flag comes from the mask's bit, so
 * a row the step left out gets a placeholder and NULL, which is allowed in
 * the chain's own scratch arrays and lets the compiler vectorize.
 */
static void
finish_step(TessExpr *expr, int set, TessResultFormat format)
{
	const TessRowMask *rows = expr->rows;
	int			nrows = rows->nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	const uint64 *present = expr->bits[set];
	Datum	   *values = expr->values[set];
	bool	   *isnull = expr->isnull[set];
	const int32 *ints = expr->ints[set];

	for (int word = 0; word < nwords; word++)
	{
		int			first = word * 64;
		int			count = Min(64, nrows - first);
		uint64		bits = present[word];

		if (rows->bits[word] == 0)
			continue;
		if (format == TESS_RESULT_INT32)
			for (int i = 0; i < count; i++)
				values[first + i] = Int32GetDatum(ints[first + i]);
		for (int i = 0; i < count; i++)
			isnull[first + i] = ((bits >> i) & 1) == 0;
	}
}

/*
 * The call's arguments: the column, the operand computed over the current
 * selection, and the scalars; true when a scalar is NULL.
 */
static bool
build_args(TessExpr *expr, Step *step, const TessDatumColumn *column,
		   TessFunctionArg *args)
{
	bool		scalar_null = false;

	for (int position = 0; position < step->nargs; position++)
	{
		args[position] = (TessFunctionArg)
			TESS_STRUCT_INITIALIZER(TessFunctionArg);
		if (position == step->column_arg)
			args[position].column = column;
		else if (position == step->column_operand)
		{
			/* The selection may have narrowed since the operand was computed. */
			step->operand->ready = false;
			args[position].column = tess_expr_get_column(step->operand);
		}
		else
		{
			bool		isnull;

			args[position].scalar =
				ExecEvalExprSwitchContext(step->scalars[position],
										  expr->econtext, &isnull);
			scalar_null |= isnull;
		}
	}
	return scalar_null;
}

/* Run one step over the selected rows; a failure is raised here. */
static void
call_step(TessExpr *expr, const Step *step, const TessFunctionArg *args,
		  void *values, TessRowMask *non_nulls)
{
	TessFunctionCall call = TESS_STRUCT_INITIALIZER(TessFunctionCall);

	/* A shorter batch than the last leaves stale bits past its rows. */
	if (non_nulls != NULL)
		memset(non_nulls->bits, 0,
			   sizeof(uint64) * tess_row_mask_word_count(non_nulls->nrows));
	call.function = step->function;
	call.nargs = step->nargs;
	call.args = args;
	call.inputcollid = step->inputcollid;
	call.rows = expr->rows;
	call.values = values;
	call.non_nulls = non_nulls;
	call.context = expr->context;
	call.status = &expr->status;
	if (step->function->evaluate(&call) != TESS_OK)
		tess_status_report(&expr->status);
}

const TessDatumColumn *
tess_expr_get_column(TessExpr *expr)
{
	TessBatch  *batch = expr->batch;
	TessDatumColumn current = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	uint64	   *current_bits;
	int			nrows;

	if (batch == NULL)
		elog(ERROR, "Tessera expression is not bound to a batch");
	if (expr->cond != NULL)
		elog(ERROR, "Tessera condition has no value column");
	if (expr->ready)
		return &expr->result;
	nrows = expr->rows->nrows;
	ensure_capacity(expr, nrows);
	if (expr->column >= 0)
	{
		batch->ops->get_datum_column(batch, expr->column, expr->rows,
									 expr->purpose, &current);
		if (current.values == NULL || current.isnull == NULL ||
			current.nrows != nrows)
			elog(ERROR, "Tessera batch returned an invalid column");
		/* The input's non-NULL rows are not counted unless asked for. */
		current_bits = NULL;
	}
	else
	{
		bool		isnull;
		Datum		value = ExecEvalExprSwitchContext(expr->scalar_value,
													  expr->econtext, &isnull);

		fill_scalar(expr, 1, value, isnull);
		current.values = expr->values[1];
		current.isnull = expr->isnull[1];
		current.nrows = nrows;
		current_bits = expr->bits[1];
	}
	for (int index = 0; index < expr->nsteps; index++)
	{
		Step	   *step = &expr->steps[index];
		int			set = index % 2;
		TessFunctionArg args[MAX_ARGS];
		TessRowMask non_nulls = {nrows, expr->bits[set]};

		if (build_args(expr, step, &current, args))
			fill_scalar(expr, set, (Datum) 0, true);
		else
		{
			call_step(expr, step, args,
					  step->function->result_format == TESS_RESULT_INT32 ?
					  (void *) expr->ints[set] : (void *) expr->values[set],
					  &non_nulls);
			finish_step(expr, set, step->function->result_format);
		}
		current.values = expr->values[set];
		current.isnull = expr->isnull[set];
		current.nrows = nrows;
		current_bits = expr->bits[set];
	}
	expr->result = current;
	expr->non_nulls.nrows = nrows;
	expr->non_nulls.bits = current_bits;
	expr->non_nulls_pending = current_bits == NULL;
	expr->ready = true;
	return &expr->result;
}

const TessRowMask *
tess_expr_non_nulls(TessExpr *expr)
{
	(void) tess_expr_get_column(expr);
	if (expr->non_nulls_pending)
	{
		const TessRowMask *rows = expr->rows;
		uint64	   *bits = expr->bits[1];
		int			row = -1;

		memset(bits, 0, sizeof(uint64) * tess_row_mask_word_count(rows->nrows));
		while ((row = tess_row_mask_next(rows, row)) >= 0)
			if (!expr->result.isnull[row])
				bits[row / 64] |= UINT64CONST(1) << (row % 64);
		expr->non_nulls.bits = bits;
		expr->non_nulls_pending = false;
	}
	return &expr->non_nulls;
}

/* The masks of a condition node sized for nrows rows, cleared. */
static void
cond_masks(Cond *cond, int nrows)
{
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask *masks[] = {&cond->truth, &cond->unknown, &cond->rest,
	&cond->all_true};

	if (cond->capacity < nwords)
	{
		MemoryContext oldcontext = MemoryContextSwitchTo(GetMemoryChunkContext(cond));

		for (int index = 0; index < lengthof(masks); index++)
		{
			if (masks[index]->bits != NULL)
				pfree(masks[index]->bits);
			masks[index]->bits = palloc_array(uint64, Max(nwords, 1));
		}
		cond->capacity = nwords;
		MemoryContextSwitchTo(oldcontext);
	}
	for (int index = 0; index < lengthof(masks); index++)
	{
		masks[index]->nrows = nrows;
		memset(masks[index]->bits, 0, sizeof(uint64) * Max(nwords, 1));
	}
}

/* Whether no row of the mask is set. */
static bool
mask_empty(const TessRowMask *mask)
{
	int			nwords = tess_row_mask_word_count(mask->nrows);

	for (int word = 0; word < nwords; word++)
		if (mask->bits[word] != 0)
			return false;
	return true;
}

/* The leaf's scalar arguments: whether one of them is NULL. */
static bool
scalar_null(TessExpr *expr)
{
	const Step *step = &expr->predicate;

	for (int position = 0; position < step->nargs; position++)
	{
		bool		isnull;

		if (position == step->column_arg || position == step->column_operand)
			continue;
		(void) ExecEvalExprSwitchContext(step->scalars[position],
										 expr->econtext, &isnull);
		if (isnull)
			return true;
	}
	return false;
}

/*
 * Evaluate a condition over the selection rows, a subset of the bound
 * batch's: cond->truth receives the rows where it is true and, when
 * unknown is asked for, cond->unknown those where it is NULL; the others
 * are false. A child runs only over the rows the executor would evaluate
 * it for: the right side of an OR where the left is not true, of an AND
 * where the left is not false, so a failure falls on the same rows.
 */
static void
eval_cond(Cond *cond, const TessRowMask *rows, bool want_unknown)
{
	int			nrows = rows->nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	uint64	   *truth;
	uint64	   *unknown;
	uint64	   *rest;

	check_stack_depth();
	cond_masks(cond, nrows);
	truth = cond->truth.bits;
	unknown = cond->unknown.bits;
	rest = cond->rest.bits;
	switch (cond->kind)
	{
		case COND_LEAF:
			memcpy(truth, rows->bits, sizeof(uint64) * nwords);
			bind_selection(cond->expr, &cond->truth);
			if (want_unknown)
			{
				/* NULL in the value, the operand or a scalar. */
				if (scalar_null(cond->expr))
					memcpy(unknown, rows->bits, sizeof(uint64) * nwords);
				else
				{
					const TessRowMask *value = tess_expr_non_nulls(cond->expr);
					const Step *predicate = &cond->expr->predicate;
					const TessRowMask *operand = predicate->operand != NULL ?
						tess_expr_non_nulls(predicate->operand) : NULL;

					for (int word = 0; word < nwords; word++)
						unknown[word] = rows->bits[word] &
							~(value->bits[word] &
							  (operand != NULL ? operand->bits[word] : ~UINT64CONST(0)));
				}
			}
			tess_expr_apply_filter(cond->expr);
			break;
		case COND_OR:
			memcpy(rest, rows->bits, sizeof(uint64) * nwords);
			for (int index = 0; index < cond->nargs && !mask_empty(&cond->rest); index++)
			{
				Cond	   *arg = cond->args[index];

				eval_cond(arg, &cond->rest, want_unknown);
				for (int word = 0; word < nwords; word++)
				{
					truth[word] |= arg->truth.bits[word];
					if (want_unknown)
						unknown[word] |= arg->unknown.bits[word];
					rest[word] &= ~arg->truth.bits[word];
				}
			}
			for (int word = 0; word < nwords; word++)
				unknown[word] &= ~truth[word];
			break;
		case COND_AND:
			memcpy(rest, rows->bits, sizeof(uint64) * nwords);
			memcpy(cond->all_true.bits, rows->bits, sizeof(uint64) * nwords);
			for (int index = 0; index < cond->nargs && !mask_empty(&cond->rest); index++)
			{
				Cond	   *arg = cond->args[index];
				bool		last = index == cond->nargs - 1;

				/* The next child runs where this one is true or unknown. */
				eval_cond(arg, &cond->rest, !last || want_unknown);
				for (int word = 0; word < nwords; word++)
				{
					cond->all_true.bits[word] &= arg->truth.bits[word];
					rest[word] = arg->truth.bits[word] |
						(!last || want_unknown ? arg->unknown.bits[word] : 0);
				}
			}
			for (int word = 0; word < nwords; word++)
			{
				truth[word] = cond->all_true.bits[word];
				if (want_unknown)
					unknown[word] = rest[word] & ~truth[word];
			}
			break;
		case COND_NOT:
			eval_cond(cond->args[0], rows, true);
			for (int word = 0; word < nwords; word++)
			{
				truth[word] = rows->bits[word] & ~cond->args[0]->truth.bits[word] &
					~cond->args[0]->unknown.bits[word];
				unknown[word] = cond->args[0]->unknown.bits[word];
			}
			break;
		case COND_NULL_TEST:
			{
				const TessRowMask *present;

				memcpy(rest, rows->bits, sizeof(uint64) * nwords);
				bind_selection(cond->expr, &cond->rest);
				present = tess_expr_non_nulls(cond->expr);
				for (int word = 0; word < nwords; word++)
					truth[word] = rows->bits[word] &
						(cond->is_null ? ~present->bits[word] : present->bits[word]);
				break;
			}
		case COND_BOOL_TEST:
			{
				Cond	   *arg = cond->args[0];

				eval_cond(arg, rows, true);
				for (int word = 0; word < nwords; word++)
				{
					uint64		yes = arg->truth.bits[word];
					uint64		maybe = arg->unknown.bits[word];
					uint64		no = rows->bits[word] & ~yes & ~maybe;
					uint64		result;

					switch (cond->test)
					{
						case IS_TRUE:
							result = yes;
							break;
						case IS_NOT_TRUE:
							result = no | maybe;
							break;
						case IS_FALSE:
							result = no;
							break;
						case IS_NOT_FALSE:
							result = yes | maybe;
							break;
						case IS_UNKNOWN:
							result = maybe;
							break;
						default:
							result = yes | no;
							break;
					}
					truth[word] = result;
				}
				break;
			}
	}
}

void
tess_expr_apply_filter(TessExpr *expr)
{
	const TessDatumColumn *column;
	TessFunctionArg args[MAX_ARGS];

	if (!expr->filter)
		elog(ERROR, "Tessera expression is not a filter");
	if (expr->cond != NULL)
	{
		if (expr->batch == NULL)
			elog(ERROR, "Tessera expression is not bound to a batch");
		eval_cond(expr->cond, expr->rows, false);
		if (expr->rows->nrows > 0)
			memcpy(expr->rows->bits, expr->cond->truth.bits,
				   sizeof(uint64) * tess_row_mask_word_count(expr->rows->nrows));
		return;
	}
	column = tess_expr_get_column(expr);
	if (build_args(expr, &expr->predicate, column, args))
	{
		/* A NULL scalar makes the strict predicate false everywhere. */
		TessRowMask *rows = expr->rows;
		int			nwords = tess_row_mask_word_count(rows->nrows);

		if (nwords > 0)
			memset(rows->bits, 0, sizeof(uint64) * nwords);
	}
	else
		call_step(expr, &expr->predicate, args, NULL, NULL);
	/* The value was computed over the wider selection. */
	expr->ready = false;
}

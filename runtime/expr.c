#include "postgres.h"

#include "executor/executor.h"
#include "nodes/nodeFuncs.h"
#include "utils/lsyscache.h"

#include "tessera/expr.h"
#include "tessera/function.h"
#include "tessera/runtime.h"

#define MAX_ARGS 2

/* One registered call of the chain; args[column_arg] is the column. */
typedef struct Step
{
	const TessFunction *function;
	Oid			inputcollid;
	int			nargs;
	int			column_arg;
	/* The other arguments, evaluated per computation. */
	ExprState  *scalars[MAX_ARGS];
} Step;

struct TessExpr
{
	MemoryContext context;
	/* The batch column the chain starts from, or -1 with scalar_value. */
	int			column;
	ExprState  *scalar_value;
	Step	   *steps;
	int			nsteps;
};

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
 * Whether node is a supported value expression, and how many Vars it has.
 * The column, when there is one, must be the first argument of every call
 * that takes it, unless the implementation accepts any shape or the
 * operator commutes into one that does.
 */
static bool
analyze_value(Node *node, Index relid, int *nvars)
{
	const TessFunction *function;
	List	   *args;
	Oid			opno;
	Oid			inputcollid;
	int			vars = 0;
	int			column_arg = -1;
	int			index = 0;

	node = strip_relabel(node);
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
		list_length(args) < 1 || list_length(args) > MAX_ARGS)
		return false;
	foreach_ptr(Node, arg, args)
	{
		int			arg_vars;

		if (!analyze_value(arg, relid, &arg_vars))
			return false;
		if (arg_vars == 1)
		{
			if (vars == 1)
				return false;
			column_arg = index;
		}
		vars += arg_vars;
		index++;
	}
	*nvars = vars;
	if (vars == 1 && column_arg != 0 &&
		(function->flags & TESS_FUNCTION_ANY_SHAPE) == 0)
		return list_length(args) == 2 &&
			commuted(opno, inputcollid, TESS_FUNCTION_VALUE) != NULL;
	return true;
}

bool
tess_expr_supports_value(Node *node, Index relid)
{
	int			nvars;

	return analyze_value(node, relid, &nvars);
}

/* Append the call of node, whose column argument was compiled already. */
static void
append_step(TessExpr *expr, Node *node, int column_arg, PlanState *parent)
{
	Step	   *step;
	List	   *args;
	Oid			opno;
	Oid			inputcollid;
	const TessFunction *function = call_of(node, &args, &opno, &inputcollid);
	int			index = 0;

	expr->steps = expr->steps == NULL ? palloc0_array(Step, 1) :
		repalloc0_array(expr->steps, Step, expr->nsteps, expr->nsteps + 1);
	step = &expr->steps[expr->nsteps++];
	step->function = function;
	step->inputcollid = inputcollid;
	step->nargs = list_length(args);
	step->column_arg = column_arg;
	if (column_arg != 0 && (function->flags & TESS_FUNCTION_ANY_SHAPE) == 0)
	{
		/* 7 - x has no commutator; 7 < x becomes x > 7 for a predicate. */
		step->function = commuted(opno, inputcollid, function->kind);
		step->column_arg = 0;
	}
	foreach_ptr(Node, arg, args)
	{
		int			position = index++;

		if (position == column_arg)
			continue;
		/* With the column moved first, the scalar moves to the other side. */
		if (step->column_arg != column_arg)
			position = column_arg;
		step->scalars[position] = ExecInitExpr((Expr *) arg, parent);
	}
}

static void
compile_value(TessExpr *expr, Node *node, PlanState *parent,
			  TessExprResolveVar resolve, void *context)
{
	List	   *args;
	Oid			opno;
	Oid			inputcollid;
	int			nvars;
	int			column_arg = -1;
	int			index = 0;

	node = strip_relabel(node);
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
	foreach_ptr(Node, arg, args)
	{
		int			arg_vars;

		(void) analyze_value(arg, 0, &arg_vars);
		if (arg_vars == 1)
			column_arg = index;
		index++;
	}
	compile_value(expr, list_nth(args, column_arg), parent, resolve, context);
	append_step(expr, node, column_arg, parent);
}

TessExpr *
tess_expr_compile_value(Node *node, PlanState *parent,
						TessExprResolveVar resolve, void *context)
{
	TessExpr   *expr;

	if (node == NULL || resolve == NULL)
		elog(ERROR, "Tessera expression requires a node and a column resolver");
	if (!tess_expr_supports_value(node, 0))
		elog(ERROR, "Tessera received an unsupported batch expression");
	expr = palloc0_object(TessExpr);
	expr->context = CurrentMemoryContext;
	expr->column = -1;
	compile_value(expr, node, parent, resolve, context);
	return expr;
}

int
tess_expr_input_column(const TessExpr *expr)
{
	return expr->column;
}

#include "postgres.h"

#include "executor/executor.h"
#include "nodes/nodeFuncs.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"

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
	/* The bound batch. */
	TessBatch  *batch;
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
	TessStatus	status;
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
	expr->status = (TessStatus) TESS_STRUCT_INITIALIZER(TessStatus);
	compile_value(expr, node, parent, resolve, context);
	return expr;
}

int
tess_expr_input_column(const TessExpr *expr)
{
	return expr->column;
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
	expr->ready = false;
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

static void
report(const TessExpr *expr)
{
	const char *sqlstate = expr->status.sqlstate;

	ereport(ERROR,
			(errcode(MAKE_SQLSTATE(sqlstate[0], sqlstate[1], sqlstate[2],
								   sqlstate[3], sqlstate[4])),
			 errmsg("%s", expr->status.message)));
}

/* Broadcast one scalar, or its NULL, over the selected rows into a set. */
static void
fill_scalar(TessExpr *expr, int set, Datum value, bool isnull)
{
	const TessRowMask *rows = &expr->batch->rows;
	int			row = -1;

	memset(expr->bits[set], 0,
		   sizeof(uint64) * tess_row_mask_word_count(rows->nrows));
	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		expr->values[set][row] = isnull ? (Datum) 0 : value;
		expr->isnull[set][row] = isnull;
		if (!isnull)
			expr->bits[set][row / 64] |= UINT64CONST(1) << (row % 64);
	}
}

/* Turn a step's results into the next column: Datums and NULL flags. */
static void
finish_step(TessExpr *expr, int set, TessResultFormat format)
{
	const TessRowMask *rows = &expr->batch->rows;
	TessRowMask non_nulls = {rows->nrows, expr->bits[set]};
	int			row = -1;

	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		bool		present = tess_row_mask_contains(&non_nulls, row);

		if (present && format == TESS_RESULT_INT32)
			expr->values[set][row] = Int32GetDatum(expr->ints[set][row]);
		expr->isnull[set][row] = !present;
	}
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
	if (expr->ready)
		return &expr->result;
	nrows = batch->rows.nrows;
	ensure_capacity(expr, nrows);
	if (expr->column >= 0)
	{
		int			row = -1;

		batch->ops->get_datum_column(batch, expr->column, &batch->rows,
									 expr->purpose, &current);
		if (current.values == NULL || current.isnull == NULL ||
			current.nrows != nrows)
			elog(ERROR, "Tessera batch returned an invalid column");
		/* The input's non-NULL rows, for a chain without steps. */
		current_bits = expr->bits[1];
		memset(current_bits, 0,
			   sizeof(uint64) * tess_row_mask_word_count(nrows));
		while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
			if (!current.isnull[row])
				current_bits[row / 64] |= UINT64CONST(1) << (row % 64);
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
		TessFunctionCall call = TESS_STRUCT_INITIALIZER(TessFunctionCall);
		TessRowMask non_nulls = {nrows, expr->bits[set]};
		bool		scalar_null = false;

		for (int position = 0; position < step->nargs; position++)
		{
			args[position] = (TessFunctionArg)
				TESS_STRUCT_INITIALIZER(TessFunctionArg);
			if (position == step->column_arg)
				args[position].column = &current;
			else
			{
				bool		isnull;

				args[position].scalar =
					ExecEvalExprSwitchContext(step->scalars[position],
											  expr->econtext, &isnull);
				scalar_null |= isnull;
			}
		}
		if (scalar_null)
			fill_scalar(expr, set, (Datum) 0, true);
		else
		{
			call.function = step->function;
			call.nargs = step->nargs;
			call.args = args;
			call.inputcollid = step->inputcollid;
			call.rows = &batch->rows;
			call.values = step->function->result_format == TESS_RESULT_INT32 ?
				(void *) expr->ints[set] : (void *) expr->values[set];
			call.non_nulls = &non_nulls;
			call.context = expr->context;
			call.status = &expr->status;
			if (step->function->evaluate(&call) != TESS_OK)
				report(expr);
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
	expr->ready = true;
	return &expr->result;
}

const TessRowMask *
tess_expr_non_nulls(TessExpr *expr)
{
	(void) tess_expr_get_column(expr);
	return &expr->non_nulls;
}

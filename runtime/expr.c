#include "postgres.h"

#include "catalog/pg_type_d.h"
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
	/* An argument that is another column of the batch, or -1; its column. */
	int			column_operand;
	int			operand_column;
	TessDatumColumn operand;
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
	/* The predicate over the chain's result, when the expression is a filter. */
	bool		filter;
	Step		predicate;
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
 * is a bare column of the batch as the call's other operand: of two
 * arguments with columns one must be a bare Var, the operand, and the
 * other continues the chain.
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
				return false;
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
 * that does; a column operand needs an implementation of any shape.
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
 * predicate over one column value and either a scalar or a bare column of
 * the batch, the operand, which the predicate must accept in any shape;
 * with a scalar the column comes first or moves first through the
 * commutator.
 */
static bool
analyze_filter(Node *node, Index relid, int *column_arg, int *column_operand)
{
	const TessFunction *function;
	List	   *args;
	Oid			opno;
	Oid			inputcollid;
	int			nvars;

	node = strip_relabel(node);
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

bool
tess_expr_supports_filter(Node *node, Index relid)
{
	int			column_arg;
	int			column_operand;

	return analyze_filter(node, relid, &column_arg, &column_operand);
}

/*
 * Set up the call of node, whose column argument was compiled already;
 * a column operand is resolved to its batch column here.
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
	step->operand_column = -1;
	if (column_operand >= 0)
	{
		const Var  *var = (const Var *) strip_relabel(list_nth(args, column_operand));

		step->operand_column = resolve(var, context);
		if (step->operand_column < 0)
			elog(ERROR, "Tessera expression operand column is unavailable");
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
		elog(ERROR, "Tessera received an unsupported batch filter");
	node = strip_relabel(node);
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

/*
 * Broadcast one scalar, or its NULL, into a set: every row of the batch is
 * written, as finish_step writes every row of a word, so that the fill is
 * a plain loop and two block copies; the non-NULL mask is the batch's
 * selected rows, or empty for a NULL.
 */
static void
fill_scalar(TessExpr *expr, int set, Datum value, bool isnull)
{
	const TessRowMask *rows = &expr->batch->rows;
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
	const TessRowMask *rows = &expr->batch->rows;
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

/* The call's arguments: the column and the scalars; true when one is NULL. */
static bool
build_args(TessExpr *expr, const Step *step, const TessDatumColumn *column,
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
			args[position].column = &step->operand;
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

/* A step's column operand, read whole; the call leaves its NULLs out. */
static void
fetch_operand(TessExpr *expr, Step *step)
{
	TessBatch  *batch = expr->batch;

	step->operand = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
	batch->ops->get_datum_column(batch, step->operand_column, &batch->rows,
								 expr->purpose, &step->operand);
	if (step->operand.values == NULL || step->operand.isnull == NULL ||
		step->operand.nrows != batch->rows.nrows)
		elog(ERROR, "Tessera batch returned an invalid column");
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
	call.rows = &expr->batch->rows;
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
	if (expr->ready)
		return &expr->result;
	nrows = batch->rows.nrows;
	ensure_capacity(expr, nrows);
	if (expr->column >= 0)
	{
		batch->ops->get_datum_column(batch, expr->column, &batch->rows,
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

		if (step->column_operand >= 0)
			fetch_operand(expr, step);
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
		const TessRowMask *rows = &expr->batch->rows;
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

void
tess_expr_apply_filter(TessExpr *expr)
{
	const TessDatumColumn *column;
	TessFunctionArg args[MAX_ARGS];

	if (!expr->filter)
		elog(ERROR, "Tessera expression is not a filter");
	column = tess_expr_get_column(expr);
	if (expr->predicate.column_operand >= 0)
		fetch_operand(expr, &expr->predicate);
	if (build_args(expr, &expr->predicate, column, args))
	{
		/* A NULL scalar makes the strict predicate false everywhere. */
		TessRowMask *rows = &expr->batch->rows;
		int			nwords = tess_row_mask_word_count(rows->nrows);

		if (nwords > 0)
			memset(rows->bits, 0, sizeof(uint64) * nwords);
	}
	else
		call_step(expr, &expr->predicate, args, NULL, NULL);
	/* The value was computed over the wider selection. */
	expr->ready = false;
}

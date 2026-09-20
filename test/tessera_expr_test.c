#include "postgres.h"

#include <string.h>

#include "catalog/namespace.h"
#include "catalog/pg_collation_d.h"
#include "catalog/pg_type_d.h"
#include "fmgr.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "nodes/value.h"
#include "utils/builtins.h"
#include "utils/lsyscache.h"

#include "tessera/expr.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_expr_supports);
PG_FUNCTION_INFO_V1(tessera_test_expr_errors);

/* Expression trees built by hand, as the planner would hand them over. */
static Node *
var(int attno, Oid type)
{
	return (Node *) makeVar(1, attno, type, -1, InvalidOid, 0);
}

static Node *
int4(int32 value)
{
	return (Node *) makeConst(INT4OID, -1, InvalidOid, 4, Int32GetDatum(value),
							  false, true);
}

static Node *
op(const char *name, Node *left, Node *right)
{
	Oid			opno = OpernameGetOprid(list_make1(makeString(pstrdup(name))),
										left == NULL ? InvalidOid : exprType(left),
										exprType(right));
	Expr	   *expr;

	if (!OidIsValid(opno))
		elog(ERROR, "Tessera test found no operator %s", name);
	expr = make_opclause(opno, get_op_rettype(opno), false,
						 (Expr *) left, (Expr *) right, InvalidOid, InvalidOid);
	/* A prefix operator has one argument. */
	if (left == NULL)
		((OpExpr *) expr)->args = list_make1(right);
	set_opfuncid((OpExpr *) expr);
	return (Node *) expr;
}

static Node *
a(void)
{
	return var(1, INT4OID);
}

static int
resolve(const Var *v, void *context)
{
	return v->varattno - 1;
}

static int
resolve_none(const Var *v, void *context)
{
	return -1;
}

/* An expectation of the supports test, reported by number on failure. */
static bool
check(int number, bool holds)
{
	if (!holds)
		elog(NOTICE, "expectation %d failed", number);
	return holds;
}

Datum
tessera_test_expr_supports(PG_FUNCTION_ARGS)
{
	bool		result = true;
	Var		   *outer = (Var *) var(1, INT4OID);
	Var		   *other = (Var *) var(1, INT4OID);
	CoalesceExpr *coalesce = makeNode(CoalesceExpr);
	NullTest   *null_test = makeNode(NullTest);

	/* Supported values. */
	result &= check(1, tess_expr_supports_value(a(), 0));
	result &= check(2, tess_expr_supports_value(a(), 1));
	result &= check(3, tess_expr_supports_value(op("+", a(), int4(1)), 0));
	result &= check(4, tess_expr_supports_value(op("%", op("*", op("+", a(), int4(1)), int4(3)), int4(7)), 0));
	result &= check(5, tess_expr_supports_value(op("-", NULL, a()), 0));
	result &= check(6, tess_expr_supports_value(op("-", int4(100), a()), 0));
	result &= check(7, tess_expr_supports_value(op("+", int4(1), int4(2)), 0));
	result &= check(8, tess_expr_supports_value(int4(5), 0));
	/* Unsupported values. */
	result &= check(9, !tess_expr_supports_value(op("+", a(), var(2, INT4OID)), 0));
	result &= check(10, !tess_expr_supports_value(op("+", a(), (Node *) makeConst(INT8OID, -1, InvalidOid, 8, Int64GetDatum(1), false, true)), 0));
	result &= check(11, !tess_expr_supports_value(op("=", var(3, TEXTOID), (Node *) makeConst(TEXTOID, -1, DEFAULT_COLLATION_OID, -1, CStringGetTextDatum("x"), false, false)), 0));
	result &= check(12, !tess_expr_supports_value(op(">", a(), int4(5)), 0));
	result &= check(13, !tess_expr_supports_value((Node *) make_andclause(list_make2(op(">", a(), int4(5)), op("<", a(), int4(9)))), 0));
	coalesce->coalescetype = INT4OID;
	coalesce->args = list_make2(a(), int4(0));
	result &= check(14, !tess_expr_supports_value((Node *) coalesce, 0));
	null_test->arg = (Expr *) a();
	null_test->nulltesttype = IS_NULL;
	result &= check(15, !tess_expr_supports_value((Node *) null_test, 0));
	other->varno = 2;
	result &= check(16, !tess_expr_supports_value((Node *) other, 1));
	outer->varlevelsup = 1;
	result &= check(17, !tess_expr_supports_value((Node *) outer, 0));
	PG_RETURN_BOOL(result);
}

Datum
tessera_test_expr_errors(PG_FUNCTION_ARGS)
{
	int32		kind = PG_GETARG_INT32(0);

	switch (kind)
	{
		case 0:
			tess_expr_compile_value(op("+", a(), var(2, INT4OID)), NULL, resolve, NULL);
			break;
		case 1:
			tess_expr_compile_value(a(), NULL, resolve_none, NULL);
			break;
		default:
			elog(ERROR, "unknown error case %d", kind);
	}
	elog(ERROR, "Tessera test expected an expression error");
	PG_RETURN_VOID();
}

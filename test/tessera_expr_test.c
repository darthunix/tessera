#include "postgres.h"

#include <string.h>

#include "catalog/namespace.h"
#include "catalog/pg_collation_d.h"
#include "catalog/pg_type_d.h"
#include "executor/executor.h"
#include "fmgr.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "nodes/value.h"
#include "utils/builtins.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"

#include "tessera/expr.h"
#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_expr_supports);
PG_FUNCTION_INFO_V1(tessera_test_expr_values);
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
null_int4(void)
{
	return (Node *) makeConst(INT4OID, -1, InvalidOid, 4, (Datum) 0, true, true);
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

/* A builder batch of (a int4, b int4, c text): a = 1..n with a NULL every
 * fifth row, b = 2a, c = 'r' || a. */
static TessBatch *
make_batch(int nrows)
{
	TupleDesc	desc = CreateTemplateTupleDesc(3);
	TupleTableSlot *slot;
	TessBuilderConfig config = TESS_STRUCT_INITIALIZER(TessBuilderConfig);
	TessBuilder *builder;

	TupleDescInitEntry(desc, 1, "a", INT4OID, -1, 0);
	TupleDescInitEntry(desc, 2, "b", INT4OID, -1, 0);
	TupleDescInitEntry(desc, 3, "c", TEXTOID, -1, 0);
	slot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	config.parent_context = CurrentMemoryContext;
	config.tuple_desc = desc;
	config.ncolumns = 3;
	config.capacity = nrows;
	builder = tess_builder_create(&config);
	for (int i = 1; i <= nrows; i++)
	{
		char		text[16];

		snprintf(text, sizeof(text), "r%d", i);
		ExecClearTuple(slot);
		slot->tts_values[0] = Int32GetDatum(i);
		slot->tts_isnull[0] = i % 5 == 0;
		slot->tts_values[1] = Int32GetDatum(2 * i);
		slot->tts_isnull[1] = false;
		slot->tts_values[2] = PointerGetDatum(cstring_to_text(text));
		slot->tts_isnull[2] = false;
		ExecStoreVirtualTuple(slot);
		tess_builder_append_slot(builder, slot);
	}
	return tess_builder_finish(builder, InvalidOid);
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

/* The result column of expr over batch, checked at one row. */
static bool
row_is(const TessDatumColumn *column, const TessRowMask *non_nulls, int row,
	   bool isnull, int32 value)
{
	if (isnull)
		return column->isnull[row] && !tess_row_mask_contains(non_nulls, row);
	return !column->isnull[row] && tess_row_mask_contains(non_nulls, row) &&
		DatumGetInt32(column->values[row]) == value;
}

Datum
tessera_test_expr_values(PG_FUNCTION_ARGS)
{
	ExprContext *econtext = CreateStandaloneExprContext();
	TessBatch  *small = make_batch(5);
	TessBatch  *batch = make_batch(70);
	TessExpr   *expr;
	const TessDatumColumn *column;
	const TessRowMask *non_nulls;
	bool		result = true;

	/* (a + 1) * 3 over a small batch, then a larger one. */
	expr = tess_expr_compile_value(op("*", op("+", a(), int4(1)), int4(3)),
								   NULL, resolve, NULL);
	result &= check(101, tess_expr_input_column(expr) == 0);
	tess_expr_bind(expr, small, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	non_nulls = tess_expr_non_nulls(expr);
	result &= check(102, column->nrows == 5 && row_is(column, non_nulls, 0, false, 6) &&
		row_is(column, non_nulls, 3, false, 15) &&
		row_is(column, non_nulls, 4, true, 0) &&
		tess_row_mask_count(non_nulls) == 4 &&
		tess_expr_get_column(expr) == column);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	non_nulls = tess_expr_non_nulls(expr);
	result &= check(103, column->nrows == 70 && row_is(column, non_nulls, 0, false, 6) &&
		row_is(column, non_nulls, 68, false, 210) &&
		row_is(column, non_nulls, 64, true, 0) &&
		tess_row_mask_count(non_nulls) == 56);

	/* The column on the right: 100 - a; unary minus. */
	expr = tess_expr_compile_value(op("-", int4(100), a()), NULL, resolve, NULL);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	result &= check(104, row_is(column, tess_expr_non_nulls(expr), 0, false, 99) &&
		row_is(column, tess_expr_non_nulls(expr), 68, false, 31) &&
		row_is(column, tess_expr_non_nulls(expr), 69, true, 0));
	expr = tess_expr_compile_value(op("-", NULL, a()), NULL, resolve, NULL);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	result &= check(105, row_is(column, tess_expr_non_nulls(expr), 1, false, -2));

	/* A bare column, and a scalar expression broadcast over the rows. */
	expr = tess_expr_compile_value(a(), NULL, resolve, NULL);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_FILTER);
	column = tess_expr_get_column(expr);
	result &= check(106, row_is(column, tess_expr_non_nulls(expr), 2, false, 3) &&
		row_is(column, tess_expr_non_nulls(expr), 4, true, 0) &&
		tess_row_mask_count(tess_expr_non_nulls(expr)) == 56);
	expr = tess_expr_compile_value(op("+", int4(1), int4(2)), NULL, resolve, NULL);
	result &= check(107, tess_expr_input_column(expr) == -1);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	result &= check(108, row_is(column, tess_expr_non_nulls(expr), 0, false, 3) &&
		row_is(column, tess_expr_non_nulls(expr), 69, false, 3) &&
		tess_row_mask_count(tess_expr_non_nulls(expr)) == 70);

	/* A NULL scalar makes every result NULL. */
	expr = tess_expr_compile_value(op("+", a(), null_int4()), NULL, resolve, NULL);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	result &= check(109, row_is(column, tess_expr_non_nulls(expr), 0, true, 0) &&
		tess_row_mask_count(tess_expr_non_nulls(expr)) == 0);

	/* Only the selected rows are computed. */
	tess_row_mask_clear(&batch->rows, 0);
	tess_row_mask_clear(&batch->rows, 1);
	expr = tess_expr_compile_value(op("+", a(), int4(1)), NULL, resolve, NULL);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	non_nulls = tess_expr_non_nulls(expr);
	result &= check(110, !tess_row_mask_contains(non_nulls, 0) &&
		!tess_row_mask_contains(non_nulls, 1) &&
		row_is(column, non_nulls, 2, false, 4) &&
		tess_row_mask_count(non_nulls) == 54);
	PG_RETURN_BOOL(result);
}

Datum
tessera_test_expr_errors(PG_FUNCTION_ARGS)
{
	int32		kind = PG_GETARG_INT32(0);
	ExprContext *econtext = CreateStandaloneExprContext();
	TessBatch  *batch = make_batch(70);
	TessExpr   *expr;

	switch (kind)
	{
		case 0:
			tess_expr_compile_value(op("+", a(), var(2, INT4OID)), NULL, resolve, NULL);
			break;
		case 1:
			tess_expr_compile_value(a(), NULL, resolve_none, NULL);
			break;
		case 2:
			expr = tess_expr_compile_value(a(), NULL, resolve, NULL);
			tess_expr_get_column(expr);
			break;
		case 3:
			expr = tess_expr_compile_value(op("/", a(), int4(0)), NULL, resolve, NULL);
			tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
			tess_expr_get_column(expr);
			break;
		case 4:
			expr = tess_expr_compile_value(op("*", a(), int4(2000000000)), NULL, resolve, NULL);
			tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
			tess_expr_get_column(expr);
			break;
		default:
			elog(ERROR, "unknown error case %d", kind);
	}
	elog(ERROR, "Tessera test expected an expression error");
	PG_RETURN_VOID();
}

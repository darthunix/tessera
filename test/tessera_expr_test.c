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
#include "utils/fmgroids.h"
#include "utils/lsyscache.h"
#include "utils/array.h"
#include "utils/memutils.h"

#include "tessera/expr.h"
#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_expr_supports);
PG_FUNCTION_INFO_V1(tessera_test_expr_values);
PG_FUNCTION_INFO_V1(tessera_test_expr_filters);
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

static Node *
or2(Node *left, Node *right)
{
	return (Node *) make_orclause(list_make2(left, right));
}

static Node *
and2(Node *left, Node *right)
{
	return (Node *) make_andclause(list_make2(left, right));
}

static Node *
not1(Node *arg)
{
	return (Node *) make_notclause((Expr *) arg);
}

static Node *
nulltest(Node *arg, NullTestType type)
{
	NullTest   *test = makeNode(NullTest);

	test->arg = (Expr *) arg;
	test->nulltesttype = type;
	test->location = -1;
	return (Node *) test;
}

static Node *
bool_test(Node *arg, BoolTestType type)
{
	BooleanTest *test = makeNode(BooleanTest);

	test->arg = (Expr *) arg;
	test->booltesttype = type;
	test->location = -1;
	return (Node *) test;
}

/* left op ANY (or ALL) of a constant int4 array; a NULL element when nulls say. */
static Node *
array_op(const char *name, Node *left, bool use_or, const int32 *values,
		 const bool *nulls, int count)
{
	Datum	   *elements = palloc_array(Datum, Max(count, 1));
	int			dims[1] = {count};
	int			lbs[1] = {1};
	ScalarArrayOpExpr *expr = makeNode(ScalarArrayOpExpr);
	ArrayType  *array;

	for (int index = 0; index < count; index++)
		elements[index] = Int32GetDatum(values[index]);
	array = construct_md_array(elements, (bool *) nulls, 1, dims, lbs, INT4OID,
							   4, true, TYPALIGN_INT);
	expr->opno = OpernameGetOprid(list_make1(makeString(pstrdup(name))),
								  INT4OID, INT4OID);
	expr->opfuncid = get_opcode(expr->opno);
	expr->useOr = use_or;
	expr->inputcollid = InvalidOid;
	expr->args = list_make2(left,
							makeConst(INT4ARRAYOID, -1, InvalidOid, -1,
									  PointerGetDatum(array), false, false));
	expr->location = -1;
	return (Node *) expr;
}

/* length(text), which no module registers: unsupported anywhere. */
static Node *
text_length(void)
{
	return (Node *) makeFuncExpr(F_TEXTLEN, INT4OID, list_make1(var(3, TEXTOID)),
								 InvalidOid, DEFAULT_COLLATION_OID,
								 COERCE_EXPLICIT_CALL);
}

/* CASE WHEN condition THEN result ... ELSE otherwise END, pairs in a list. */
static Node *
case_of(List *pairs, Node *otherwise, Oid type)
{
	CaseExpr   *choice = makeNode(CaseExpr);

	choice->casetype = type;
	for (int index = 0; index < list_length(pairs); index += 2)
	{
		CaseWhen   *when = makeNode(CaseWhen);

		when->expr = list_nth(pairs, index);
		when->result = list_nth(pairs, index + 1);
		when->location = -1;
		choice->args = lappend(choice->args, when);
	}
	choice->defresult = (Expr *) (otherwise != NULL ? otherwise :
								  (Node *) makeNullConst(type, -1, InvalidOid));
	choice->location = -1;
	return (Node *) choice;
}

static Node *
coalesce_of(Node *first, Node *second)
{
	CoalesceExpr *coalesce = makeNode(CoalesceExpr);

	coalesce->coalescetype = INT4OID;
	coalesce->args = list_make2(first, second);
	coalesce->location = -1;
	return (Node *) coalesce;
}

static Node *
nullif_of(Node *first, Node *second)
{
	NullIfExpr *nullif = (NullIfExpr *) op("=", first, second);

	nullif->xpr.type = T_NullIfExpr;
	nullif->opresulttype = INT4OID;
	return (Node *) nullif;
}

/* One more element than an IN list the compiler takes. */
#define MAX_ARRAY 33

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

/* Only the first column is available. */
static int
resolve_second_none(const Var *v, void *context)
{
	return v->varattno == 1 ? 0 : -1;
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
	/* A second column as a step's operand, bare; not inside an expression. */
	result &= check(9, tess_expr_supports_value(op("+", a(), var(2, INT4OID)), 0));
	result &= check(28, tess_expr_supports_value(op("*", op("+", a(), int4(1)), var(2, INT4OID)), 0));
	result &= check(29, tess_expr_supports_value(op("-", var(2, INT4OID), a()), 0));
	result &= check(30, tess_expr_supports_value(op("+", op("+", a(), var(2, INT4OID)), var(2, INT4OID)), 0));
	result &= check(31, tess_expr_supports_value(op("-", a(), op("*", var(2, INT4OID), int4(2))), 0));
	/* Two computed sides: the second is an operand of its own. */
	result &= check(33, tess_expr_supports_value(op("+", op("+", a(), int4(1)), op("*", var(2, INT4OID), int4(2))), 0));
	result &= check(40, tess_expr_supports_value(op("*", op("+", a(), int4(1)), op("+", var(2, INT4OID), op("*", a(), int4(2)))), 0));
	result &= check(32, tess_expr_supports_filter(op(">", op("+", a(), var(2, INT4OID)), int4(5)), 0));
	/* A bigint column with a bigint or an integer scalar, and the cast. */
	result &= check(34, tess_expr_supports_value(op("+", var(4, INT8OID), (Node *) makeConst(INT8OID, -1, InvalidOid, 8, Int64GetDatum(1), false, true)), 0));
	result &= check(35, tess_expr_supports_value(op("*", var(4, INT8OID), int4(2)), 0));
	result &= check(36, tess_expr_supports_filter(op("<", var(4, INT8OID), int4(5)), 0));
	result &= check(37, tess_expr_supports_filter(op("<", int4(5), var(4, INT8OID)), 0));
	result &= check(38, tess_expr_supports_filter(op(">", (Node *) makeFuncExpr(F_INT8_INT4, INT8OID, list_make1(a()), InvalidOid, InvalidOid, COERCE_IMPLICIT_CAST), int4(5)), 0));
	/* The integer on the left of a mixed operator: an equivalent, cast. */
	result &= check(10, tess_expr_supports_value(op("+", a(), (Node *) makeConst(INT8OID, -1, InvalidOid, 8, Int64GetDatum(1), false, true)), 0));
	result &= check(39, tess_expr_supports_value(op("+", int4(1), var(4, INT8OID)), 0));
	result &= check(42, tess_expr_supports_value(op("*", a(), var(4, INT8OID)), 0));
	result &= check(43, tess_expr_supports_filter(op("<", a(), var(4, INT8OID)), 0));
	result &= check(11, !tess_expr_supports_value(op("=", var(3, TEXTOID), (Node *) makeConst(TEXTOID, -1, DEFAULT_COLLATION_OID, -1, CStringGetTextDatum("x"), false, false)), 0));
	result &= check(12, !tess_expr_supports_value(op(">", a(), int4(5)), 0));
	result &= check(13, !tess_expr_supports_value((Node *) make_andclause(list_make2(op(">", a(), int4(5)), op("<", a(), int4(9)))), 0));
	coalesce->coalescetype = INT4OID;
	coalesce->args = list_make2(a(), int4(0));
	result &= check(14, tess_expr_supports_value((Node *) coalesce, 0));
	/* Something unsupported inside an operand rejects the whole. */
	result &= check(41, !tess_expr_supports_value(op("*", op("+", a(), int4(1)), text_length()), 0));
	/* A conditional value with an unsupported condition or branch. */
	result &= check(52, !tess_expr_supports_value(case_of(list_make2(op("=", var(3, TEXTOID), (Node *) makeConst(TEXTOID, -1, DEFAULT_COLLATION_OID, -1, CStringGetTextDatum("x"), false, false)), a()), int4(0), INT4OID), 0));
	result &= check(53, !tess_expr_supports_value(case_of(list_make2(op(">", a(), int4(5)), text_length()), int4(0), INT4OID), 0));
	result &= check(54, tess_expr_supports_value(nullif_of(a(), op("/", var(2, INT4OID), int4(2))), 0));
	null_test->arg = (Expr *) a();
	null_test->nulltesttype = IS_NULL;
	result &= check(15, !tess_expr_supports_value((Node *) null_test, 0));
	other->varno = 2;
	result &= check(16, !tess_expr_supports_value((Node *) other, 1));
	outer->varlevelsup = 1;
	result &= check(17, !tess_expr_supports_value((Node *) outer, 0));
	/* Filters: a registered predicate over one column value and a scalar. */
	result &= check(18, tess_expr_supports_filter(op(">", a(), int4(5)), 0));
	result &= check(19, tess_expr_supports_filter(op("<", int4(7), a()), 1));
	result &= check(20, tess_expr_supports_filter(op(">", op("+", a(), int4(1)), int4(5)), 0));
	result &= check(21, !tess_expr_supports_filter(op(">", a(), int4(5)), 2));
	result &= check(22, !tess_expr_supports_filter(a(), 0));
	result &= check(23, !tess_expr_supports_filter(op("<", int4(1), int4(2)), 0));
	result &= check(24, !tess_expr_supports_filter(op("+", a(), int4(1)), 0));
	/* A column operand of any shape, bare or computed. */
	result &= check(25, tess_expr_supports_filter(op("=", a(), var(2, INT4OID)), 0));
	result &= check(208, tess_expr_supports_filter(op(">", var(2, INT4OID), op("*", a(), int4(10))), 0));
	result &= check(209, tess_expr_supports_filter(op(">", op("+", var(2, INT4OID), int4(1)), op("*", a(), int4(10))), 0));
	/* Conditions of several parts. */
	result &= check(26, tess_expr_supports_filter((Node *) make_andclause(list_make2(op(">", a(), int4(5)), op("<", a(), int4(9)))), 0));
	result &= check(44, tess_expr_supports_filter(or2(op(">", a(), int4(5)), not1(op("<", a(), var(2, INT4OID)))), 0));
	result &= check(45, tess_expr_supports_filter(nulltest(var(3, TEXTOID), IS_NOT_NULL), 0));
	result &= check(46, tess_expr_supports_filter(bool_test(op(">", a(), int4(5)), IS_UNKNOWN), 0));
	{
		int32		few[3] = {1, 3, 5};
		int32		many[MAX_ARRAY];
		NullTest   *row_test = (NullTest *) nulltest(a(), IS_NULL);
		ScalarArrayOpExpr *param_array;

		for (int index = 0; index < MAX_ARRAY; index++)
			many[index] = index;
		result &= check(47, tess_expr_supports_filter(array_op("=", a(), true, few, NULL, 3), 0));
		result &= check(48, !tess_expr_supports_filter(array_op("=", a(), true, many, NULL, MAX_ARRAY), 0));
		param_array = (ScalarArrayOpExpr *) array_op("=", a(), true, few, NULL, 3);
		lsecond(param_array->args) = makeNode(Param);
		((Param *) lsecond(param_array->args))->paramkind = PARAM_EXTERN;
		((Param *) lsecond(param_array->args))->paramtype = INT4ARRAYOID;
		result &= check(49, !tess_expr_supports_filter((Node *) param_array, 0));
		row_test->argisrow = true;
		result &= check(50, !tess_expr_supports_filter((Node *) row_test, 0));
		result &= check(51, !tess_expr_supports_filter(or2(op(">", a(), int4(5)), op("=", var(3, TEXTOID), (Node *) makeConst(TEXTOID, -1, DEFAULT_COLLATION_OID, -1, CStringGetTextDatum("x"), false, false))), 0));
	}
	result &= check(27, !tess_expr_supports_filter(op("=", var(3, TEXTOID), (Node *) makeConst(TEXTOID, -1, DEFAULT_COLLATION_OID, -1, CStringGetTextDatum("x"), false, false)), 0));
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
	/* A shorter batch after a longer one: no stale bits past its rows. */
	tess_expr_bind(expr, small, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	non_nulls = tess_expr_non_nulls(expr);
	result &= check(111, column->nrows == 5 && row_is(column, non_nulls, 3, false, 15) &&
		row_is(column, non_nulls, 4, true, 0) && tess_row_mask_count(non_nulls) == 4);

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

	/* A second column as an operand: a + b, b - a, (a + 1) * b; a NULL on
	 * either side makes a NULL. */
	expr = tess_expr_compile_value(op("+", a(), var(2, INT4OID)), NULL, resolve, NULL);
	result &= check(113, tess_expr_input_column(expr) == 0);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	non_nulls = tess_expr_non_nulls(expr);
	result &= check(114, row_is(column, non_nulls, 0, false, 3) &&
		row_is(column, non_nulls, 4, true, 0) &&
		row_is(column, non_nulls, 68, false, 207) &&
		tess_row_mask_count(non_nulls) == 56);
	expr = tess_expr_compile_value(op("-", var(2, INT4OID), a()), NULL, resolve, NULL);
	result &= check(115, tess_expr_input_column(expr) == 1);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	non_nulls = tess_expr_non_nulls(expr);
	result &= check(116, row_is(column, non_nulls, 1, false, 2) &&
		row_is(column, non_nulls, 4, true, 0) &&
		tess_row_mask_count(non_nulls) == 56);
	expr = tess_expr_compile_value(op("*", op("+", a(), int4(1)), var(2, INT4OID)), NULL, resolve, NULL);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	non_nulls = tess_expr_non_nulls(expr);
	result &= check(117, row_is(column, non_nulls, 2, false, 24) &&
		row_is(column, non_nulls, 68, false, 9660) &&
		tess_row_mask_count(non_nulls) == 56);
	/* The bare column becomes the operand when the other side is a chain,
	 * keeping its side: a - b * 2. */
	expr = tess_expr_compile_value(op("-", a(), op("*", var(2, INT4OID), int4(2))), NULL, resolve, NULL);
	result &= check(118, tess_expr_input_column(expr) == 1);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	non_nulls = tess_expr_non_nulls(expr);
	result &= check(119, row_is(column, non_nulls, 1, false, -6) &&
		row_is(column, non_nulls, 4, true, 0) &&
		tess_row_mask_count(non_nulls) == 56);

	/* Two computed sides, (a + 1) * (b + 2); a NULL on the left side. */
	expr = tess_expr_compile_value(op("*", op("+", a(), int4(1)), op("+", var(2, INT4OID), int4(2))), NULL, resolve, NULL);
	result &= check(120, tess_expr_input_column(expr) == 0);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	non_nulls = tess_expr_non_nulls(expr);
	result &= check(121, row_is(column, non_nulls, 0, false, 8) &&
		row_is(column, non_nulls, 4, true, 0) &&
		row_is(column, non_nulls, 68, false, 9800) &&
		tess_row_mask_count(non_nulls) == 56);
	/* The NULL on the right side: (b + 1) * (a + 1). */
	expr = tess_expr_compile_value(op("*", op("+", var(2, INT4OID), int4(1)), op("+", a(), int4(1))), NULL, resolve, NULL);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	non_nulls = tess_expr_non_nulls(expr);
	result &= check(122, row_is(column, non_nulls, 1, false, 15) &&
		row_is(column, non_nulls, 9, true, 0) &&
		tess_row_mask_count(non_nulls) == 56);
	/* An operand with an operand: (a + 1) * (b + a * 2). */
	expr = tess_expr_compile_value(op("*", op("+", a(), int4(1)), op("+", var(2, INT4OID), op("*", a(), int4(2)))), NULL, resolve, NULL);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	non_nulls = tess_expr_non_nulls(expr);
	result &= check(123, row_is(column, non_nulls, 0, false, 8) &&
		row_is(column, non_nulls, 2, false, 48) &&
		row_is(column, non_nulls, 4, true, 0) &&
		tess_row_mask_count(non_nulls) == 56);
	/* The same expression rebound to a shorter batch. */
	tess_expr_bind(expr, small, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	non_nulls = tess_expr_non_nulls(expr);
	result &= check(124, column->nrows == 5 && row_is(column, non_nulls, 3, false, 80) &&
		tess_row_mask_count(non_nulls) == 4);

	/* a + 5::bigint, int48pl: int8pl over a cast to int8, the constant folded. */
	expr = tess_expr_compile_value(op("+", a(), (Node *) makeConst(INT8OID, -1, InvalidOid, 8, Int64GetDatum(5), false, true)), NULL, resolve, NULL);
	result &= check(125, tess_expr_input_column(expr) == 0);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
	column = tess_expr_get_column(expr);
	non_nulls = tess_expr_non_nulls(expr);
	result &= check(126, DatumGetInt64(column->values[0]) == 6 &&
		DatumGetInt64(column->values[68]) == 74 &&
		column->isnull[4] && !tess_row_mask_contains(non_nulls, 4) &&
		tess_row_mask_count(non_nulls) == 56);

	/* Conditional values over the 70 rows. */
	{
		Node	   *branches = case_of(list_make4(op(">", a(), int4(60)), a(),
												  op("<", a(), int4(5)), op("-", NULL, a())),
									   int4(0), INT4OID);
		CaseTestExpr *test = makeNode(CaseTestExpr);

		expr = tess_expr_compile_value(branches, NULL, resolve, NULL);
		tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
		column = tess_expr_get_column(expr);
		non_nulls = tess_expr_non_nulls(expr);
		result &= check(127, row_is(column, non_nulls, 0, false, -1) &&
			row_is(column, non_nulls, 64, false, 0) &&
			row_is(column, non_nulls, 68, false, 69) &&
			row_is(column, non_nulls, 10, false, 0) &&
			tess_row_mask_count(non_nulls) == 70);
		/* Without ELSE, NULL where no condition holds. */
		expr = tess_expr_compile_value(case_of(list_make2(op(">", a(), int4(60)), a()), NULL, INT4OID), NULL, resolve, NULL);
		tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
		column = tess_expr_get_column(expr);
		non_nulls = tess_expr_non_nulls(expr);
		result &= check(128, row_is(column, non_nulls, 68, false, 69) &&
			row_is(column, non_nulls, 0, true, 0) &&
			tess_row_mask_count(non_nulls) == 8);
		/* A text branch. */
		expr = tess_expr_compile_value(case_of(list_make2(op(">", a(), int4(60)), var(3, TEXTOID)), var(3, TEXTOID), TEXTOID), NULL, resolve, NULL);
		tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
		column = tess_expr_get_column(expr);
		result &= check(129, !column->isnull[68] &&
			strcmp(TextDatumGetCString(column->values[68]), "r69") == 0 &&
			tess_row_mask_count(tess_expr_non_nulls(expr)) == 70);
		/* The division runs only where a is not 3. */
		expr = tess_expr_compile_value(case_of(list_make2(op("<>", a(), int4(3)), op("/", int4(10), op("-", a(), int4(3)))), int4(0), INT4OID), NULL, resolve, NULL);
		tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
		column = tess_expr_get_column(expr);
		non_nulls = tess_expr_non_nulls(expr);
		result &= check(130, row_is(column, non_nulls, 2, false, 0) &&
			row_is(column, non_nulls, 3, false, 10));
		/* A simple CASE a WHEN 1 THEN 10 WHEN 2 THEN 20 END. */
		test->typeId = INT4OID;
		test->typeMod = -1;
		{
			CaseExpr   *simple = (CaseExpr *) case_of(list_make4(op("=", (Node *) test, int4(1)), int4(10),
															op("=", (Node *) test, int4(2)), int4(20)),
													  NULL, INT4OID);

			simple->arg = (Expr *) a();
			expr = tess_expr_compile_value((Node *) simple, NULL, resolve, NULL);
		}
		tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
		column = tess_expr_get_column(expr);
		non_nulls = tess_expr_non_nulls(expr);
		result &= check(131, row_is(column, non_nulls, 0, false, 10) &&
			row_is(column, non_nulls, 1, false, 20) &&
			row_is(column, non_nulls, 2, true, 0) &&
			tess_row_mask_count(non_nulls) == 2);
		/* A step over a choice, and a choice as an operand. */
		expr = tess_expr_compile_value(op("+", branches, int4(1)), NULL, resolve, NULL);
		tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
		column = tess_expr_get_column(expr);
		non_nulls = tess_expr_non_nulls(expr);
		result &= check(132, row_is(column, non_nulls, 0, false, 0) &&
			row_is(column, non_nulls, 68, false, 70) &&
			tess_row_mask_count(non_nulls) == 70);
		expr = tess_expr_compile_value(op("+", a(), branches), NULL, resolve, NULL);
		tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
		column = tess_expr_get_column(expr);
		non_nulls = tess_expr_non_nulls(expr);
		result &= check(133, row_is(column, non_nulls, 0, false, 0) &&
			row_is(column, non_nulls, 4, true, 0) &&
			tess_row_mask_count(non_nulls) == 56);
		/* COALESCE(a, b); the second argument only where a is NULL. */
		expr = tess_expr_compile_value(coalesce_of(a(), var(2, INT4OID)), NULL, resolve, NULL);
		tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
		column = tess_expr_get_column(expr);
		non_nulls = tess_expr_non_nulls(expr);
		result &= check(134, row_is(column, non_nulls, 0, false, 1) &&
			row_is(column, non_nulls, 4, false, 10) &&
			tess_row_mask_count(non_nulls) == 70);
		expr = tess_expr_compile_value(coalesce_of(var(2, INT4OID), op("/", int4(10), op("-", var(2, INT4OID), var(2, INT4OID)))), NULL, resolve, NULL);
		tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
		column = tess_expr_get_column(expr);
		result &= check(135, row_is(column, tess_expr_non_nulls(expr), 0, false, 2));
		/* A scalar argument decides every row left; a NULL one none. */
		expr = tess_expr_compile_value(coalesce_of(a(), int4(0)), NULL, resolve, NULL);
		tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
		column = tess_expr_get_column(expr);
		non_nulls = tess_expr_non_nulls(expr);
		result &= check(138, row_is(column, non_nulls, 4, false, 0) &&
			row_is(column, non_nulls, 3, false, 4) &&
			tess_row_mask_count(non_nulls) == 70);
		expr = tess_expr_compile_value(coalesce_of(a(), null_int4()), NULL, resolve, NULL);
		tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
		column = tess_expr_get_column(expr);
		non_nulls = tess_expr_non_nulls(expr);
		result &= check(139, row_is(column, non_nulls, 4, true, 0) &&
			tess_row_mask_count(non_nulls) == 56);
		/* NULLIF(a, 3); NULLIF(a, b / 2), equal everywhere. */
		expr = tess_expr_compile_value(nullif_of(a(), int4(3)), NULL, resolve, NULL);
		tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
		column = tess_expr_get_column(expr);
		non_nulls = tess_expr_non_nulls(expr);
		result &= check(136, row_is(column, non_nulls, 0, false, 1) &&
			row_is(column, non_nulls, 2, true, 0) &&
			tess_row_mask_count(non_nulls) == 55);
		expr = tess_expr_compile_value(nullif_of(a(), op("/", var(2, INT4OID), int4(2))), NULL, resolve, NULL);
		tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
		result &= check(137, tess_row_mask_count(tess_expr_non_nulls(expr)) == 0);
	}

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
	/* A bare column's mask over the narrowed selection, built on request. */
	expr = tess_expr_compile_value(a(), NULL, resolve, NULL);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_FILTER);
	non_nulls = tess_expr_non_nulls(expr);
	result &= check(112, !tess_row_mask_contains(non_nulls, 0) &&
		tess_row_mask_contains(non_nulls, 2) &&
		!tess_row_mask_contains(non_nulls, 4) &&
		tess_row_mask_count(non_nulls) == 54 &&
		tess_expr_non_nulls(expr) == non_nulls);
	PG_RETURN_BOOL(result);
}

/* Apply a filter compiled from node to a fresh batch of 70 rows. */
static TessBatch *
filtered(Node *node, ExprContext *econtext)
{
	TessBatch  *batch = make_batch(70);
	TessExpr   *expr = tess_expr_compile_filter(node, NULL, resolve, NULL);

	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_FILTER);
	tess_expr_apply_filter(expr);
	return batch;
}

Datum
tessera_test_expr_filters(PG_FUNCTION_ARGS)
{
	ExprContext *econtext = CreateStandaloneExprContext();
	TessBatch  *batch;
	TessBatch  *other;
	TessExpr   *expr;
	bool		result = true;

	/* a % 7 = 0 keeps the multiples of 7 that are not NULL. */
	batch = filtered(op("=", op("%", a(), int4(7)), int4(0)), econtext);
	result &= check(201, tess_row_mask_count(&batch->rows) == 8 &&
		tess_row_mask_contains(&batch->rows, 6) &&
		!tess_row_mask_contains(&batch->rows, 0) &&
		!tess_row_mask_contains(&batch->rows, 34));
	/* a + 1 > 5: a value chain under the predicate. */
	batch = filtered(op(">", op("+", a(), int4(1)), int4(5)), econtext);
	result &= check(202, tess_row_mask_count(&batch->rows) == 52 &&
		!tess_row_mask_contains(&batch->rows, 3) &&
		!tess_row_mask_contains(&batch->rows, 4) &&
		tess_row_mask_contains(&batch->rows, 5));
	/* 7 < a through the commutator equals a > 7. */
	batch = filtered(op("<", int4(7), a()), econtext);
	other = filtered(op(">", a(), int4(7)), econtext);
	result &= check(203, tess_row_mask_count(&batch->rows) == 50 &&
		batch->rows.bits[0] == other->rows.bits[0] &&
		batch->rows.bits[1] == other->rows.bits[1]);
	/* A second filter narrows the selection further, in place. */
	expr = tess_expr_compile_filter(op("=", op("%", a(), int4(7)), int4(0)), NULL, resolve, NULL);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_FILTER);
	tess_expr_apply_filter(expr);
	result &= check(204, tess_row_mask_count(&batch->rows) == 7 &&
		!tess_row_mask_contains(&batch->rows, 6) &&
		tess_row_mask_contains(&batch->rows, 13));
	/* A NULL scalar clears the selection. */
	batch = filtered(op(">", a(), null_int4()), econtext);
	result &= check(205, tess_row_mask_count(&batch->rows) == 0);
	/* The value under the filter is available, over the narrowed rows. */
	batch = filtered(op(">", a(), int4(60)), econtext);
	expr = tess_expr_compile_filter(op("<", int4(65), op("+", a(), int4(1))), NULL, resolve, NULL);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_FILTER);
	tess_expr_apply_filter(expr);
	result &= check(206, tess_row_mask_count(&batch->rows) == 4 &&
		tess_row_mask_contains(&batch->rows, 65) &&
		!tess_row_mask_contains(&batch->rows, 63) &&
		tess_row_mask_count(tess_expr_non_nulls(expr)) == 4 &&
		DatumGetInt32(tess_expr_get_column(expr)->values[65]) == 67);
	/* (a + 1) > (b - 60), two computed sides: a < 61; the value is
	 * recomputed over the narrowed rows, the operand with it. */
	batch = make_batch(70);
	expr = tess_expr_compile_filter(op(">", op("+", a(), int4(1)), op("-", var(2, INT4OID), int4(60))), NULL, resolve, NULL);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_FILTER);
	tess_expr_apply_filter(expr);
	result &= check(210, tess_row_mask_count(&batch->rows) == 48 &&
		tess_row_mask_contains(&batch->rows, 58) &&
		!tess_row_mask_contains(&batch->rows, 60) &&
		!tess_row_mask_contains(&batch->rows, 4) &&
		tess_row_mask_count(tess_expr_non_nulls(expr)) == 48);
	tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_FILTER);
	tess_expr_apply_filter(expr);
	result &= check(211, tess_row_mask_count(&batch->rows) == 48);
	/* a < 5000000000::bigint, int48lt beyond the int4 range: every non-NULL
	 * row, through int8lt over a cast. */
	batch = filtered(op("<", a(), (Node *) makeConst(INT8OID, -1, InvalidOid, 8, Int64GetDatum(INT64CONST(5000000000)), false, true)), econtext);
	result &= check(212, tess_row_mask_count(&batch->rows) == 56 &&
		!tess_row_mask_contains(&batch->rows, 4));
	/* Three-valued logic over masks: a is NULL on every fifth row. */
	{
		const int32 odd[3] = {1, 3, 5};
		const int32 one_null[2] = {1, 0};
		const bool	second_null[2] = {false, true};
		const int32 bounds[2] = {3, 7};

		batch = filtered(or2(op(">", a(), int4(60)), op("<", a(), int4(5))), econtext);
		result &= check(213, tess_row_mask_count(&batch->rows) == 12);
		batch = filtered(or2(op(">", a(), int4(60)), nulltest(a(), IS_NULL)), econtext);
		result &= check(214, tess_row_mask_count(&batch->rows) == 22);
		batch = filtered(not1(op(">", a(), int4(5))), econtext);
		result &= check(215, tess_row_mask_count(&batch->rows) == 4 &&
			!tess_row_mask_contains(&batch->rows, 4));
		batch = filtered(not1(or2(op(">", a(), int4(5)), op("<", a(), int4(2)))), econtext);
		result &= check(216, tess_row_mask_count(&batch->rows) == 3);
		batch = filtered(bool_test(and2(op(">", a(), int4(5)), op("<", a(), int4(9))), IS_NOT_TRUE), econtext);
		result &= check(217, tess_row_mask_count(&batch->rows) == 67);
		batch = filtered(bool_test(op(">", a(), int4(5)), IS_UNKNOWN), econtext);
		result &= check(218, tess_row_mask_count(&batch->rows) == 14);
		/* The right side runs only where the left one is not true, or not
		 * false: no division by zero on the row where a is 3. */
		batch = filtered(or2(op("=", a(), int4(3)), op(">", op("/", int4(10), op("-", a(), int4(3))), int4(1))), econtext);
		result &= check(219, tess_row_mask_count(&batch->rows) == 5);
		batch = filtered(and2(op("<>", a(), int4(3)), op(">", op("/", int4(10), op("-", a(), int4(3))), int4(1))), econtext);
		result &= check(220, tess_row_mask_count(&batch->rows) == 4);
		/* Null tests over a value, a chain and a text column. */
		batch = filtered(nulltest(a(), IS_NULL), econtext);
		result &= check(221, tess_row_mask_count(&batch->rows) == 14);
		batch = filtered(nulltest(op("+", a(), int4(1)), IS_NOT_NULL), econtext);
		result &= check(222, tess_row_mask_count(&batch->rows) == 56);
		batch = filtered(nulltest(var(3, TEXTOID), IS_NOT_NULL), econtext);
		result &= check(223, tess_row_mask_count(&batch->rows) == 70);
		/* Short lists: a NULL element leaves a non-match unknown. */
		batch = filtered(array_op("=", a(), true, odd, NULL, 3), econtext);
		result &= check(224, tess_row_mask_count(&batch->rows) == 2);
		batch = filtered(array_op("=", a(), true, one_null, second_null, 2), econtext);
		result &= check(225, tess_row_mask_count(&batch->rows) == 1);
		batch = filtered(array_op("<>", a(), false, one_null, second_null, 2), econtext);
		result &= check(226, tess_row_mask_count(&batch->rows) == 0);
		batch = filtered(not1(array_op("=", a(), true, one_null, second_null, 2)), econtext);
		result &= check(227, tess_row_mask_count(&batch->rows) == 0);
		batch = filtered(array_op("<", a(), true, bounds, NULL, 2), econtext);
		result &= check(228, tess_row_mask_count(&batch->rows) == 5);
		/* x computed once and compared with each element: a chain, a
		 * remainder, a narrowed selection, the unknown rows, ALL. */
		{
			const int32 evens[3] = {2, 4, 6};
			const int32 residues[2] = {0, 3};
			const int32 three_null[2] = {3, 0};
			const int32 threes[2] = {3, 3};
			const int32 three_five[2] = {3, 5};

			batch = filtered(array_op("=", op("+", a(), int4(1)), true, evens, NULL, 3), econtext);
			result &= check(230, tess_row_mask_count(&batch->rows) == 2);
			batch = filtered(array_op("=", op("%", a(), int4(7)), true, residues, NULL, 2), econtext);
			result &= check(231, tess_row_mask_count(&batch->rows) == 16);
			batch = filtered(op(">", a(), int4(60)), econtext);
			expr = tess_expr_compile_filter(array_op("=", a(), true, residues, NULL, 2), NULL, resolve, NULL);
			tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_FILTER);
			tess_expr_apply_filter(expr);
			result &= check(232, tess_row_mask_count(&batch->rows) == 0);
			batch = filtered(bool_test(array_op("=", a(), true, three_null, second_null, 2), IS_UNKNOWN), econtext);
			result &= check(233, tess_row_mask_count(&batch->rows) == 69);
			batch = filtered(array_op("=", a(), false, threes, NULL, 2), econtext);
			result &= check(234, tess_row_mask_count(&batch->rows) == 1 &&
				tess_row_mask_contains(&batch->rows, 2));
			batch = filtered(array_op("<>", a(), false, three_five, NULL, 2), econtext);
			result &= check(235, tess_row_mask_count(&batch->rows) == 55);
		}
		/* A condition over a selection a previous one narrowed. */
		batch = filtered(or2(op(">", a(), int4(60)), op("<", a(), int4(5))), econtext);
		expr = tess_expr_compile_filter(or2(op("<", a(), int4(3)), op(">", a(), int4(68))), NULL, resolve, NULL);
		tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_FILTER);
		tess_expr_apply_filter(expr);
		result &= check(229, tess_row_mask_count(&batch->rows) == 3 &&
			tess_row_mask_contains(&batch->rows, 68));
	}
	/* a + b > 10: the other column as the step's operand. */
	batch = filtered(op(">", op("+", a(), var(2, INT4OID)), int4(10)), econtext);
	result &= check(207, tess_row_mask_count(&batch->rows) == 53 &&
		tess_row_mask_contains(&batch->rows, 3) &&
		!tess_row_mask_contains(&batch->rows, 2) &&
		!tess_row_mask_contains(&batch->rows, 4));
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
			tess_expr_compile_value(op("*", op("+", a(), int4(1)), text_length()), NULL, resolve, NULL);
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
		case 5:
			expr = tess_expr_compile_value(a(), NULL, resolve, NULL);
			tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_FILTER);
			tess_expr_apply_filter(expr);
			break;
		case 6:
			tess_expr_compile_filter(op("+", a(), int4(1)), NULL, resolve, NULL);
			break;
		case 7:
			/* A failure inside an operand: (a + 1) * (b / 0). */
			expr = tess_expr_compile_value(op("*", op("+", a(), int4(1)), op("/", var(2, INT4OID), int4(0))), NULL, resolve, NULL);
			tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
			tess_expr_get_column(expr);
			break;
		case 9:
			/* a = 3 AND 10 / (a - 3) > 1: the right side runs where the
			 * left one is true, on the row where a is 3 too. */
			expr = tess_expr_compile_filter(and2(op("=", a(), int4(3)), op(">", op("/", int4(10), op("-", a(), int4(3))), int4(1))), NULL, resolve, NULL);
			tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_FILTER);
			tess_expr_apply_filter(expr);
			break;
		case 10:
			/* 10 / (a - 3) IN (1, 2): x fails on the row where a is 3. */
			{
				const int32 small_values[2] = {1, 2};

				expr = tess_expr_compile_filter(array_op("=", op("/", int4(10), op("-", a(), int4(3))), true, small_values, NULL, 2), NULL, resolve, NULL);
				tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_FILTER);
				tess_expr_apply_filter(expr);
			}
			break;
		case 11:
			/* A branch runs over its rows: a = 3 takes the division. */
			expr = tess_expr_compile_value(case_of(list_make2(op("=", a(), int4(3)), op("/", int4(10), op("-", a(), int4(3)))), int4(0), INT4OID), NULL, resolve, NULL);
			tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
			tess_expr_get_column(expr);
			break;
		case 12:
			/* The second argument runs where a is NULL: b - b is 0 there. */
			expr = tess_expr_compile_value(coalesce_of(a(), op("/", int4(10), op("-", var(2, INT4OID), var(2, INT4OID)))), NULL, resolve, NULL);
			tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
			tess_expr_get_column(expr);
			break;
		case 8:
			/* An operand whose column is unavailable. */
			tess_expr_compile_value(op("*", op("+", a(), int4(1)), op("+", var(2, INT4OID), int4(1))), NULL, resolve_second_none, NULL);
			break;
		default:
			elog(ERROR, "unknown error case %d", kind);
	}
	elog(ERROR, "Tessera test expected an expression error");
	PG_RETURN_VOID();
}

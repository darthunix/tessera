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
#include "utils/numeric.h"

#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_project_columns);
PG_FUNCTION_INFO_V1(tessera_test_project_errors);

/* Expression trees over the scan tuple (a int4, b int4, c text). */
static Node *
var(int attno, Oid type)
{
	return (Node *) makeVar(INDEX_VAR, attno, type, -1, InvalidOid, 0);
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
	if (left == NULL)
		((OpExpr *) expr)->args = list_make1(right);
	set_opfuncid((OpExpr *) expr);
	return (Node *) expr;
}

static Node *
text_const(const char *value)
{
	return (Node *) makeConst(TEXTOID, -1, DEFAULT_COLLATION_OID, -1,
							  PointerGetDatum(cstring_to_text(value)), false,
							  false);
}

/* 1::numeric / (a - 11): row-wise, and a division by zero at a = 11. */
static Node *
numeric_division(void)
{
	Datum		one = DirectFunctionCall3(numeric_in, CStringGetDatum("1"),
										  ObjectIdGetDatum(InvalidOid),
										  Int32GetDatum(-1));
	Node	   *divisor = (Node *) makeFuncExpr(F_NUMERIC_INT4, NUMERICOID,
												list_make1(op("-", var(1, INT4OID), int4(11))),
												InvalidOid, InvalidOid,
												COERCE_EXPLICIT_CALL);

	return op("/", (Node *) makeConst(NUMERICOID, -1, InvalidOid, -1, one,
									  false, false), divisor);
}

static Node *
text_length(Node *arg)
{
	return (Node *) makeFuncExpr(F_TEXTLEN, INT4OID, list_make1(arg),
								 InvalidOid, InvalidOid, COERCE_EXPLICIT_CALL);
}

static TargetEntry *
target(Node *expr, int position)
{
	return makeTargetEntry((Expr *) expr, position, "computed", false);
}

static TupleDesc
make_desc(void)
{
	TupleDesc	desc = CreateTemplateTupleDesc(3);

	TupleDescInitEntry(desc, 1, "a", INT4OID, -1, 0);
	TupleDescInitEntry(desc, 2, "b", INT4OID, -1, 0);
	TupleDescInitEntry(desc, 3, "c", TEXTOID, -1, 0);
	return desc;
}

/* A builder batch: a = 1..n with a NULL every fifth row, b = 2a, c = 'r' || a. */
static TessBatch *
make_batch(TupleDesc desc, int nrows)
{
	TupleTableSlot *slot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	TessBuilderConfig config = TESS_STRUCT_INITIALIZER(TessBuilderConfig);
	TessBuilder *builder;

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

/* A projection over the scan tuple of the three columns, in order. */
static TessProjection *
make_projection(TupleDesc desc, List *computed)
{
	TessProjectionConfig config = TESS_STRUCT_INITIALIZER(TessProjectionConfig);
	TessLayout *tuple = palloc0_object(TessLayout);

	tuple->struct_size = sizeof(TessLayout);
	tuple->ncolumns = 3;
	tuple->ntargets = 3;
	config.parent_context = CurrentMemoryContext;
	config.econtext = CreateStandaloneExprContext();
	config.scan_slot = MakeSingleTupleTableSlot(desc, &TTSOpsVirtual);
	config.scan_tuple = tuple;
	config.base_columns = 3;
	config.computed = computed;
	return tess_projection_create(&config);
}

static void
get_column(TessBatch *batch, int column, const TessRowMask *rows,
		   TessDatumColumn *result)
{
	*result = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
	batch->ops->get_datum_column(batch, column, rows,
								 TESS_COLUMN_FOR_PROJECTION, result);
	if (result->values == NULL || result->isnull == NULL ||
		result->nrows != batch->rows.nrows)
		elog(ERROR, "the projection returned an invalid column");
}

static bool
check(int number, bool holds)
{
	if (!holds)
		elog(NOTICE, "expectation %d failed", number);
	return holds;
}

Datum
tessera_test_project_columns(PG_FUNCTION_ARGS)
{
	TupleDesc	desc = make_desc();
	TessBatch  *child = make_batch(desc, 70);
	TessProjection *projection;
	const TessProjectionStats *stats;
	TessBatch  *batch;
	TessDatumColumn column;
	bool		result = true;
	bool		holds;

	/* Two chains over a: a + 1 and -a, as columns 3 and 4. */
	projection = make_projection(desc, list_make2(target(op("+", var(1, INT4OID), int4(1)), 1),
												  target(op("-", NULL, var(1, INT4OID)), 2)));
	stats = tess_projection_stats(projection);
	batch = tess_projection_wrap(projection, child);
	result &= check(1, batch != child && batch->rows.nrows == 70 &&
					batch->rows.bits == child->rows.bits &&
					batch->ops != child->ops);
	/* A base column passes through. */
	get_column(batch, 1, &batch->rows, &column);
	holds = true;
	for (int row = 0; row < 70; row++)
		holds &= !column.isnull[row] && DatumGetInt32(column.values[row]) == 2 * (row + 1);
	result &= check(2, holds && stats->chain_datums == 0);
	/* The chains, over the selected rows, computed once. */
	get_column(batch, 3, &batch->rows, &column);
	holds = stats->chain_datums == 70;
	for (int row = 0; row < 70; row++)
		holds &= (row + 1) % 5 == 0 ? column.isnull[row] :
			!column.isnull[row] && DatumGetInt32(column.values[row]) == row + 2;
	result &= check(3, holds);
	get_column(batch, 3, &batch->rows, &column);
	result &= check(4, stats->chain_datums == 70);
	get_column(batch, 4, &batch->rows, &column);
	holds = stats->chain_datums == 140;
	for (int row = 0; row < 70; row++)
		holds &= (row + 1) % 5 == 0 ? column.isnull[row] :
			!column.isnull[row] && DatumGetInt32(column.values[row]) == -(row + 1);
	result &= check(5, holds);
	/* Narrowing the wrapper narrows the child; a new batch computes anew. */
	tess_row_mask_clear(&batch->rows, 0);
	result &= check(6, !tess_row_mask_contains(&child->rows, 0));
	batch->ops->release(batch);
	batch = tess_projection_wrap(projection, make_batch(desc, 8));
	get_column(batch, 3, &batch->rows, &column);
	result &= check(7, batch->rows.nrows == 8 && stats->chain_datums == 148 &&
					DatumGetInt32(column.values[7]) == 9);
	batch->ops->release(batch);

	/*
	 * Row-wise columns: c || 'x', length(c) and a numeric division, as
	 * columns 3, 4 and 5, computed for the rows asked for and remembered.
	 */
	child = make_batch(desc, 70);
	projection = make_projection(desc, list_make3(target(op("||", var(3, TEXTOID), text_const("x")), 1),
												  target(text_length(var(3, TEXTOID)), 2),
												  target(numeric_division(), 3)));
	stats = tess_projection_stats(projection);
	batch = tess_projection_wrap(projection, child);
	{
		uint64		bits[2];
		TessRowMask even = {70, bits};

		memset(bits, 0, sizeof(bits));
		for (int row = 0; row < 70; row += 2)
			bits[row / 64] |= UINT64CONST(1) << (row % 64);
		get_column(batch, 3, &even, &column);
		holds = stats->row_datums == 35;
		for (int row = 0; row < 70; row += 2)
		{
			char		expected[16];

			snprintf(expected, sizeof(expected), "r%dx", row + 1);
			holds &= !column.isnull[row] &&
				strcmp(text_to_cstring(DatumGetTextPP(column.values[row])), expected) == 0;
		}
		result &= check(8, holds);
		/* The rest, then nothing more on a repeated request. */
		get_column(batch, 3, &batch->rows, &column);
		result &= check(9, stats->row_datums == 70 &&
						strcmp(text_to_cstring(DatumGetTextPP(column.values[1])), "r2x") == 0);
		get_column(batch, 3, &batch->rows, &column);
		result &= check(10, stats->row_datums == 70);
		/* Another column's computation resets the per-tuple memory; the
		 * copies of the first survive. */
		get_column(batch, 4, &batch->rows, &column);
		holds = stats->row_datums == 140;
		for (int row = 0; row < 70; row++)
			holds &= !column.isnull[row] &&
				DatumGetInt32(column.values[row]) == (row + 1 < 10 ? 2 : 3);
		result &= check(11, holds);
		get_column(batch, 3, &batch->rows, &column);
		result &= check(12, strcmp(text_to_cstring(DatumGetTextPP(column.values[69])), "r70x") == 0);
		/* The division is computed for the rows asked for only: the first
		 * nine rows never reach a = 11, and the NULL a gives NULL. */
		memset(bits, 0, sizeof(bits));
		bits[0] = UINT64CONST(0x1ff);
		get_column(batch, 5, &even, &column);
		holds = stats->row_datums == 149;
		for (int row = 0; row < 9; row++)
			holds &= column.isnull[row] == (row == 4);
		result &= check(13, holds);
	}
	batch->ops->release(batch);
	PG_RETURN_BOOL(result);
}

Datum
tessera_test_project_errors(PG_FUNCTION_ARGS)
{
	int			kind = PG_GETARG_INT32(0);
	TupleDesc	desc = make_desc();
	TessBatch  *child = make_batch(desc, 8);
	TessProjection *projection;
	TessBatch  *batch;
	TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	TessRowMask foreign = {5, NULL};

	projection = make_projection(desc, list_make1(target(op("+", var(1, INT4OID), int4(1)), 1)));
	switch (kind)
	{
		case 0:
			batch = tess_projection_wrap(projection, child);
			batch->ops->get_datum_column(batch, 4, &batch->rows,
										 TESS_COLUMN_FOR_PROJECTION, &column);
			break;
		case 1:
			batch = tess_projection_wrap(projection, child);
			batch->ops->get_datum_column(batch, 3, &foreign,
										 TESS_COLUMN_FOR_PROJECTION, &column);
			break;
		case 2:
			tess_projection_wrap(projection, child);
			tess_projection_wrap(projection, child);
			break;
		case 3:
			/* A released wrapper wraps nothing. */
			batch = tess_projection_wrap(projection, child);
			batch->ops->release(batch);
			batch->ops->get_datum_column(batch, 3, &child->rows,
										 TESS_COLUMN_FOR_PROJECTION, &column);
			break;
		case 4:
			/* The division fails only when the row with a = 11 is asked for. */
			projection = make_projection(desc, list_make1(target(numeric_division(), 1)));
			batch = tess_projection_wrap(projection, make_batch(desc, 12));
			{
				uint64		bit = UINT64CONST(1) << 10;
				TessRowMask tenth = {12, &bit};

				batch->ops->get_datum_column(batch, 3, &tenth,
											 TESS_COLUMN_FOR_PROJECTION, &column);
			}
			break;
		case 5:
			/* A row-wise expression reading beyond the scan tuple. */
			make_projection(desc, list_make1(target(text_length(var(5, TEXTOID)), 1)));
			break;
		default:
			elog(ERROR, "unknown error case %d", kind);
	}
	elog(ERROR, "Tessera test expected a projection error");
	PG_RETURN_VOID();
}

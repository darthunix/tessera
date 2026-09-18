#include "postgres.h"

#include <string.h>

#include "fmgr.h"

#include "tessera/kernels.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_kernels_layout);
PG_FUNCTION_INFO_V1(tessera_test_kernels_filter);
PG_FUNCTION_INFO_V1(tessera_test_kernels_errors);
PG_FUNCTION_INFO_V1(tessera_test_kernels_aggregates);
PG_FUNCTION_INFO_V1(tessera_test_kernels_arithmetic);
PG_FUNCTION_INFO_V1(tessera_test_kernels_panic);
PG_FUNCTION_INFO_V1(tessera_test_kernels_report);

#define NROWS 200
#define NWORDS 4

/*
 * A column with every fifth row NULL and a selection of every row but each
 * third, spanning three full words and a tail.
 */
static void
fill(Datum *values, bool *isnull, uint64 *words)
{
	int			row;

	memset(words, 0, NWORDS * sizeof(uint64));
	for (row = 0; row < NROWS; row++)
	{
		values[row] = Int32GetDatum((int32) ((row * 7919) % 1000 - 500));
		isnull[row] = row % 5 == 0;
		if (row % 3 != 1)
			words[row / 64] |= UINT64CONST(1) << (row % 64);
	}
}

/* The rows a "> 0" filter keeps, by a scalar loop. */
static void
expected_rows(const Datum *values, const bool *isnull,
			  const uint64 *selected, uint64 *expected)
{
	int			row;

	memset(expected, 0, NWORDS * sizeof(uint64));
	for (row = 0; row < NROWS; row++)
	{
		bool		chosen = (selected[row / 64] &
							  (UINT64CONST(1) << (row % 64))) != 0;

		if (chosen && !isnull[row] && DatumGetInt32(values[row]) > 0)
			expected[row / 64] |= UINT64CONST(1) << (row % 64);
	}
}

static void
init_column(TessDatumColumn *column, const Datum *values, const bool *isnull)
{
	column->struct_size = sizeof(TessDatumColumn);
	column->values = values;
	column->isnull = isnull;
	column->nrows = NROWS;
}

Datum
tessera_test_kernels_layout(PG_FUNCTION_ARGS)
{
	PG_RETURN_BOOL(tess_kernels_abi_version() == TESS_KERNELS_ABI_VERSION &&
				   tess_kernels_layout(TESS_LAYOUT_ROW_MASK_SIZE) ==
				   sizeof(TessRowMask) &&
				   tess_kernels_layout(TESS_LAYOUT_DATUM_COLUMN_SIZE) ==
				   sizeof(TessDatumColumn) &&
				   tess_kernels_layout(TESS_LAYOUT_DATUM_COLUMN_NROWS_OFFSET) ==
				   offsetof(TessDatumColumn, nrows) &&
				   tess_kernels_layout(TESS_LAYOUT_STATUS_SIZE) ==
				   sizeof(TessStatus) &&
				   tess_kernels_layout(TESS_LAYOUT_STATUS_MESSAGE_OFFSET) ==
				   offsetof(TessStatus, message) &&
				   tess_kernels_layout((TessLayoutKind) 99) == 0);
}

Datum
tessera_test_kernels_filter(PG_FUNCTION_ARGS)
{
	Datum		values[NROWS];
	bool		isnull[NROWS];
	uint64		words[NWORDS];
	uint64		expected[NWORDS];
	uint64		prepared_words[NWORDS];
	TessDatumColumn column;
	TessRowMask rows = {NROWS, words};
	TessRowMask prepared = {NROWS, prepared_words};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	fill(values, isnull, words);
	expected_rows(values, isnull, words, expected);
	init_column(&column, values, isnull);

	/* Without a readiness mask: the whole column is initialized. */
	if (tess_int4_filter(&column, NULL, &rows, TESS_CMP_GT, 0,
						 &status) != TESS_OK ||
		status.code != TESS_OK || status.sqlstate[0] != '\0' ||
		status.message[0] != '\0' ||
		memcmp(words, expected, sizeof(words)) != 0)
		PG_RETURN_BOOL(false);

	/* With a readiness mask covering the selection, and no status. */
	fill(values, isnull, words);
	memcpy(prepared_words, words, sizeof(words));
	if (tess_int4_filter(&column, &prepared, &rows, TESS_CMP_GT, 0,
						 NULL) != TESS_OK ||
		memcmp(words, expected, sizeof(words)) != 0)
		PG_RETURN_BOOL(false);

	/* A selected row outside the readiness mask is an error. */
	fill(values, isnull, words);
	memcpy(prepared_words, words, sizeof(words));
	prepared_words[0] &= ~(UINT64CONST(1) << 2);
	if (tess_int4_filter(&column, &prepared, &rows, TESS_CMP_GT, 0,
						 &status) != TESS_ERROR_INVALID_ARGUMENT ||
		status.code != TESS_ERROR_INVALID_ARGUMENT ||
		strstr(status.message, "unprepared") == NULL)
		PG_RETURN_BOOL(false);

	PG_RETURN_BOOL(true);
}

Datum
tessera_test_kernels_errors(PG_FUNCTION_ARGS)
{
	Datum		values[NROWS];
	bool		isnull[NROWS];
	uint64		words[NWORDS];
	uint64		original[NWORDS];
	TessDatumColumn column;
	TessRowMask rows = {NROWS, words};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	fill(values, isnull, words);
	memcpy(original, words, sizeof(words));
	init_column(&column, values, isnull);

	/* Dimensions are checked before any mutation. */
	column.nrows = NROWS - 1;
	if (tess_int4_filter(&column, NULL, &rows, TESS_CMP_GT, 0,
						 &status) != TESS_ERROR_INVALID_ARGUMENT ||
		status.code != TESS_ERROR_INVALID_ARGUMENT ||
		strcmp(status.sqlstate, "XX000") != 0 ||
		strstr(status.message, "row counts") == NULL ||
		memcmp(words, original, sizeof(words)) != 0)
		PG_RETURN_BOOL(false);
	column.nrows = NROWS;

	/* An unknown operation and a null column are rejected the same way. */
	if (tess_int4_filter(&column, NULL, &rows, (TessCompareOp) 9, 0,
						 &status) != TESS_ERROR_INVALID_ARGUMENT ||
		strstr(status.message, "unknown") == NULL ||
		tess_int4_filter(NULL, NULL, &rows, TESS_CMP_GT, 0,
						 &status) != TESS_ERROR_INVALID_ARGUMENT ||
		memcmp(words, original, sizeof(words)) != 0)
		PG_RETURN_BOOL(false);

	/* An undersized status is left alone; the return value still tells. */
	status.struct_size = 8;
	status.code = TESS_ERROR_PANIC;
	if (tess_int4_filter(&column, NULL, &rows, TESS_CMP_GT, 0,
						 &status) != TESS_OK ||
		status.code != TESS_ERROR_PANIC)
		PG_RETURN_BOOL(false);

	PG_RETURN_BOOL(true);
}

Datum
tessera_test_kernels_aggregates(PG_FUNCTION_ARGS)
{
	Datum		values[NROWS];
	bool		isnull[NROWS];
	uint64		words[NWORDS];
	TessDatumColumn column;
	TessRowMask rows = {NROWS, words};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	int64		count = 0;
	int64		sum = 0;
	int32		least = PG_INT32_MAX;
	int32		greatest = PG_INT32_MIN;
	int64		got_count = -1;
	int64		got_sum = -1;
	int32		got_min = -1;
	int32		got_max = -1;
	bool		sum_null = true;
	bool		min_null = true;
	bool		max_null = true;
	int			row;

	fill(values, isnull, words);
	init_column(&column, values, isnull);
	for (row = 0; row < NROWS; row++)
	{
		bool		chosen = (words[row / 64] &
							  (UINT64CONST(1) << (row % 64))) != 0;

		if (chosen && !isnull[row])
		{
			int32		value = DatumGetInt32(values[row]);

			count++;
			sum += value;
			least = Min(least, value);
			greatest = Max(greatest, value);
		}
	}
	if (tess_int4_count(&column, NULL, &rows, &got_count, &status) != TESS_OK ||
		tess_int4_sum(&column, NULL, &rows, &sum_null, &got_sum,
					  &status) != TESS_OK ||
		tess_int4_min(&column, NULL, &rows, &min_null, &got_min,
					  &status) != TESS_OK ||
		tess_int4_max(&column, NULL, &rows, &max_null, &got_max,
					  &status) != TESS_OK ||
		got_count != count || got_sum != sum || got_min != least ||
		got_max != greatest || sum_null || min_null || max_null)
		PG_RETURN_BOOL(false);

	/* Without selected rows: count 0, the rest NULL. */
	memset(words, 0, sizeof(words));
	if (tess_int4_count(&column, NULL, &rows, &got_count, NULL) != TESS_OK ||
		tess_int4_sum(&column, NULL, &rows, &sum_null, &got_sum,
					  NULL) != TESS_OK ||
		tess_int4_min(&column, NULL, &rows, &min_null, &got_min,
					  NULL) != TESS_OK ||
		tess_int4_max(&column, NULL, &rows, &max_null, &got_max,
					  NULL) != TESS_OK ||
		got_count != 0 || !sum_null || !min_null || !max_null)
		PG_RETURN_BOOL(false);

	/* A dimension error writes no result. */
	column.nrows = NROWS - 1;
	got_count = 7;
	if (tess_int4_count(&column, NULL, &rows, &got_count,
						&status) != TESS_ERROR_INVALID_ARGUMENT ||
		got_count != 7)
		PG_RETURN_BOOL(false);

	PG_RETURN_BOOL(true);
}

/* A three-row column for the arithmetic cases. */
static void
init_small(TessDatumColumn *column, Datum *values, bool *isnull,
		   int32 first, int32 second, int32 third, bool second_null)
{
	values[0] = Int32GetDatum(first);
	values[1] = Int32GetDatum(second);
	values[2] = Int32GetDatum(third);
	isnull[0] = false;
	isnull[1] = second_null;
	isnull[2] = false;
	column->struct_size = sizeof(TessDatumColumn);
	column->values = values;
	column->isnull = isnull;
	column->nrows = 3;
}

Datum
tessera_test_kernels_arithmetic(PG_FUNCTION_ARGS)
{
	Datum		values[NROWS];
	bool		isnull[NROWS];
	uint64		words[NWORDS];
	int32		results[NROWS];
	/* Output masks are row masks too: no bits beyond the rows on entry. */
	uint64		result_words[NWORDS] = {0};
	TessDatumColumn column;
	TessRowMask rows = {NROWS, words};
	TessRowMask non_nulls = {NROWS, result_words};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	Datum		small_values[3];
	bool		small_nulls[3];
	uint64		small_selection = 7;
	TessRowMask small_rows = {3, &small_selection};
	int32		small_results[3];
	uint64		small_word = 0;
	TessRowMask small_non_nulls = {3, &small_word};
	int			row;

	/* x + 7 over the fixture, against a scalar loop. */
	fill(values, isnull, words);
	init_column(&column, values, isnull);
	if (tess_int4_arith_scalar(TESS_ARITH_ADD, &column, 7, NULL, &rows,
							   results, &non_nulls, &status) != TESS_OK)
		PG_RETURN_BOOL(false);
	for (row = 0; row < NROWS; row++)
	{
		bool		chosen = (words[row / 64] &
							  (UINT64CONST(1) << (row % 64))) != 0;
		bool		present = (result_words[row / 64] &
							   (UINT64CONST(1) << (row % 64))) != 0;

		if (present != (chosen && !isnull[row]))
			PG_RETURN_BOOL(false);
		if (present && results[row] != DatumGetInt32(values[row]) + 7)
			PG_RETURN_BOOL(false);
	}

	/* 100 - x and x * x with a NULL in the middle row. */
	init_small(&column, small_values, small_nulls, 10, 20, 30, true);
	if (tess_int4_arith_scalar_left(TESS_ARITH_SUB, 100, &column, NULL,
									&small_rows, small_results,
									&small_non_nulls, &status) != TESS_OK ||
		small_word != 5 || small_results[0] != 90 || small_results[2] != 70)
		PG_RETURN_BOOL(false);
	if (tess_int4_arith_columns(TESS_ARITH_MUL, &column, NULL, &column, NULL,
								&small_rows, small_results, &small_non_nulls,
								&status) != TESS_OK ||
		small_word != 5 || small_results[0] != 100 ||
		small_results[2] != 900)
		PG_RETURN_BOOL(false);

	/* PostgreSQL's error codes. */
	init_small(&column, small_values, small_nulls, PG_INT32_MAX,
			   PG_INT32_MIN, 1, false);
	if (tess_int4_arith_scalar(TESS_ARITH_ADD, &column, 1, NULL, &small_rows,
							   small_results, &small_non_nulls,
							   &status) != TESS_ERROR_INTEGER_OUT_OF_RANGE ||
		strcmp(status.sqlstate, "22003") != 0 ||
		tess_int4_arith_scalar(TESS_ARITH_DIV, &column, 0, NULL, &small_rows,
							   small_results, &small_non_nulls,
							   &status) != TESS_ERROR_DIVISION_BY_ZERO ||
		strcmp(status.sqlstate, "22012") != 0 ||
		tess_int4_arith_scalar(TESS_ARITH_DIV, &column, -1, NULL, &small_rows,
							   small_results, &small_non_nulls,
							   &status) != TESS_ERROR_INTEGER_OUT_OF_RANGE ||
		tess_int4_arith_scalar((TessArithOp) 7, &column, 1, NULL, &small_rows,
							   small_results, &small_non_nulls,
							   &status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);

	/* A NULL operand never fails, and x % -1 is 0. */
	init_small(&column, small_values, small_nulls, 1, PG_INT32_MIN,
			   PG_INT32_MAX, true);
	if (tess_int4_arith_scalar(TESS_ARITH_MOD, &column, -1, NULL, &small_rows,
							   small_results, &small_non_nulls,
							   &status) != TESS_OK ||
		small_word != 5 || small_results[0] != 0 || small_results[2] != 0)
		PG_RETURN_BOOL(false);

	PG_RETURN_BOOL(true);
}

Datum
tessera_test_kernels_panic(PG_FUNCTION_ARGS)
{
	Datum		values[NROWS];
	bool		isnull[NROWS];
	uint64		words[NWORDS];
	uint64		expected[NWORDS];
	TessDatumColumn column;
	TessRowMask rows = {NROWS, words};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	/* The panic is caught and reported. */
	if (tess_kernels_test_panic(&status) != TESS_ERROR_PANIC ||
		status.code != TESS_ERROR_PANIC ||
		strcmp(status.sqlstate, "XX000") != 0 ||
		strstr(status.message, "injected") == NULL)
		PG_RETURN_BOOL(false);

	/* The library works afterwards. */
	fill(values, isnull, words);
	expected_rows(values, isnull, words, expected);
	init_column(&column, values, isnull);
	if (tess_int4_filter(&column, NULL, &rows, TESS_CMP_GT, 0,
						 &status) != TESS_OK ||
		status.code != TESS_OK ||
		memcmp(words, expected, sizeof(words)) != 0)
		PG_RETURN_BOOL(false);

	PG_RETURN_BOOL(true);
}

/* How a caller reports a status: ERROR after the entry point returned. */
Datum
tessera_test_kernels_report(PG_FUNCTION_ARGS)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	(void) tess_kernels_test_panic(&status);
	ereport(ERROR,
			(errcode(MAKE_SQLSTATE(status.sqlstate[0], status.sqlstate[1],
								   status.sqlstate[2], status.sqlstate[3],
								   status.sqlstate[4])),
			 errmsg("%s", status.message)));
	PG_RETURN_BOOL(false);
}

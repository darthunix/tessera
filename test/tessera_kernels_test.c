#include "postgres.h"

#include <string.h>

#include "fmgr.h"

#include "tessera/kernels.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_kernels_layout);
PG_FUNCTION_INFO_V1(tessera_test_kernels_filter);
PG_FUNCTION_INFO_V1(tessera_test_kernels_filter_int8);
PG_FUNCTION_INFO_V1(tessera_test_kernels_errors);
PG_FUNCTION_INFO_V1(tessera_test_kernels_aggregates);
PG_FUNCTION_INFO_V1(tessera_test_kernels_aggregates_int8);
PG_FUNCTION_INFO_V1(tessera_test_kernels_arithmetic);
PG_FUNCTION_INFO_V1(tessera_test_kernels_arithmetic_int8);
PG_FUNCTION_INFO_V1(tessera_test_kernels_cast);
PG_FUNCTION_INFO_V1(tessera_test_kernels_hashes);
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

/*
 * The int4 column's values shifted past the int4 range as int8 Datums: a
 * 32-bit read would see zeros and keep no row.
 */
static void
fill_int8(Datum *values, bool *isnull, uint64 *words)
{
	int			row;

	fill(values, isnull, words);
	for (row = 0; row < NROWS; row++)
		values[row] = Int64GetDatum(((int64) DatumGetInt32(values[row])) << 33);
}

/* The rows a "> scalar" filter keeps over int8 values, by a scalar loop. */
static void
expected_rows_int8(const Datum *values, const bool *isnull,
				   const uint64 *selected, int64 scalar, uint64 *expected)
{
	int			row;

	memset(expected, 0, NWORDS * sizeof(uint64));
	for (row = 0; row < NROWS; row++)
	{
		bool		chosen = (selected[row / 64] &
							  (UINT64CONST(1) << (row % 64))) != 0;

		if (chosen && !isnull[row] && DatumGetInt64(values[row]) > scalar)
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
tessera_test_kernels_filter_int8(PG_FUNCTION_ARGS)
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

	fill_int8(values, isnull, words);
	init_column(&column, values, isnull);

	/* Whole Datums: the same rows as the int4 column's "> 0". */
	expected_rows_int8(values, isnull, words, 0, expected);
	if (tess_int8_filter(&column, NULL, &rows, TESS_CMP_GT, 0,
						 &status) != TESS_OK ||
		status.code != TESS_OK ||
		memcmp(words, expected, sizeof(words)) != 0)
		PG_RETURN_BOOL(false);

	/* A scalar beyond the int4 range, with a readiness mask. */
	fill_int8(values, isnull, words);
	memcpy(prepared_words, words, sizeof(words));
	expected_rows_int8(values, isnull, words, ((int64) 400) << 33, expected);
	if (tess_int8_filter(&column, &prepared, &rows, TESS_CMP_GT,
						 ((int64) 400) << 33, NULL) != TESS_OK ||
		memcmp(words, expected, sizeof(words)) != 0)
		PG_RETURN_BOOL(false);

	/* A selected row outside the readiness mask is an error. */
	fill_int8(values, isnull, words);
	memcpy(prepared_words, words, sizeof(words));
	prepared_words[0] &= ~(UINT64CONST(1) << 2);
	if (tess_int8_filter(&column, &prepared, &rows, TESS_CMP_GT, 0,
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
	if (tess_count(&column, NULL, &rows, &got_count, &status) != TESS_OK ||
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
	if (tess_count(&column, NULL, &rows, &got_count, NULL) != TESS_OK ||
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
	if (tess_count(&column, NULL, &rows, &got_count,
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

/* Three int8 Datums, the middle one NULL on request. */
static void
init_small_int8(TessDatumColumn *column, Datum *values, bool *isnull,
				int64 first, int64 second, int64 third, bool second_null)
{
	values[0] = Int64GetDatum(first);
	values[1] = Int64GetDatum(second);
	values[2] = Int64GetDatum(third);
	isnull[0] = false;
	isnull[1] = second_null;
	isnull[2] = false;
	column->struct_size = sizeof(TessDatumColumn);
	column->values = values;
	column->isnull = isnull;
	column->nrows = 3;
}

Datum
tessera_test_kernels_aggregates_int8(PG_FUNCTION_ARGS)
{
	Datum		values[NROWS];
	bool		isnull[NROWS];
	uint64		words[NWORDS];
	TessDatumColumn column;
	TessRowMask rows = {NROWS, words};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	int64		count = 0;
	int64		least = PG_INT64_MAX;
	int64		greatest = PG_INT64_MIN;
	int64		got_count = -1;
	int64		got_min = -1;
	int64		got_max = -1;
	bool		min_null = true;
	bool		max_null = true;
	int			row;

	fill_int8(values, isnull, words);
	init_column(&column, values, isnull);
	for (row = 0; row < NROWS; row++)
	{
		bool		chosen = (words[row / 64] &
							  (UINT64CONST(1) << (row % 64))) != 0;

		if (chosen && !isnull[row])
		{
			int64		value = DatumGetInt64(values[row]);

			count++;
			least = Min(least, value);
			greatest = Max(greatest, value);
		}
	}
	if (tess_count(&column, NULL, &rows, &got_count, &status) != TESS_OK ||
		tess_int8_min(&column, NULL, &rows, &min_null, &got_min,
					  &status) != TESS_OK ||
		tess_int8_max(&column, NULL, &rows, &max_null, &got_max,
					  &status) != TESS_OK ||
		got_count != count || got_min != least || got_max != greatest ||
		min_null || max_null)
		PG_RETURN_BOOL(false);

	/* The count reads flags alone: the same over the int4 Datums. */
	fill(values, isnull, words);
	if (tess_count(&column, NULL, &rows, &got_count, &status) != TESS_OK ||
		got_count != count)
		PG_RETURN_BOOL(false);

	/* Without selected rows: count 0, the extremes NULL. */
	memset(words, 0, sizeof(words));
	if (tess_count(&column, NULL, &rows, &got_count, NULL) != TESS_OK ||
		tess_int8_min(&column, NULL, &rows, &min_null, &got_min,
					  NULL) != TESS_OK ||
		tess_int8_max(&column, NULL, &rows, &max_null, &got_max,
					  NULL) != TESS_OK ||
		got_count != 0 || !min_null || !max_null)
		PG_RETURN_BOOL(false);

	/* A dimension error writes no result. */
	column.nrows = NROWS - 1;
	got_count = 7;
	if (tess_count(&column, NULL, &rows, &got_count,
				   &status) != TESS_ERROR_INVALID_ARGUMENT ||
		got_count != 7)
		PG_RETURN_BOOL(false);

	PG_RETURN_BOOL(true);
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

/* PostgreSQL's hash_combine, for the expected two-key hashes. */
static uint32
combine(uint32 a, uint32 b)
{
	a ^= b + 0x9e3779b9 + (a << 6) + (a >> 2);
	return a;
}

Datum
tessera_test_kernels_arithmetic_int8(PG_FUNCTION_ARGS)
{
	Datum		values[NROWS];
	bool		isnull[NROWS];
	uint64		words[NWORDS];
	int64		results[NROWS];
	uint64		result_words[NWORDS] = {0};
	TessDatumColumn column;
	TessRowMask rows = {NROWS, words};
	TessRowMask non_nulls = {NROWS, result_words};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	Datum		small_values[3];
	bool		small_nulls[3];
	uint64		small_selection = 7;
	TessRowMask small_rows = {3, &small_selection};
	int64		small_results[3];
	uint64		small_word = 0;
	TessRowMask small_non_nulls = {3, &small_word};
	const int64 big = ((int64) 1) << 40;
	int			row;

	/* x + 2^40 over int8 Datums, against a scalar loop. */
	fill_int8(values, isnull, words);
	init_column(&column, values, isnull);
	if (tess_int8_arith_scalar(TESS_ARITH_ADD, &column, big, NULL, &rows,
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
		if (present && results[row] != DatumGetInt64(values[row]) + big)
			PG_RETURN_BOOL(false);
	}

	/* 100 * 2^20 - x and x * x with a NULL in the middle row. */
	init_small_int8(&column, small_values, small_nulls, 10 << 20, 20 << 20,
					30 << 20, true);
	if (tess_int8_arith_scalar_left(TESS_ARITH_SUB, 100 << 20, &column, NULL,
									&small_rows, small_results,
									&small_non_nulls, &status) != TESS_OK ||
		small_word != 5 || small_results[0] != (90 << 20) ||
		small_results[2] != (70 << 20))
		PG_RETURN_BOOL(false);
	if (tess_int8_arith_columns(TESS_ARITH_MUL, &column, NULL, &column, NULL,
								&small_rows, small_results, &small_non_nulls,
								&status) != TESS_OK ||
		small_word != 5 || small_results[0] != (((int64) 100) << 40) ||
		small_results[2] != (((int64) 900) << 40))
		PG_RETURN_BOOL(false);

	/* PostgreSQL's error codes and message for bigint. */
	init_small_int8(&column, small_values, small_nulls, PG_INT64_MAX,
					PG_INT64_MIN, 1, false);
	if (tess_int8_arith_scalar(TESS_ARITH_ADD, &column, 1, NULL, &small_rows,
							   small_results, &small_non_nulls,
							   &status) != TESS_ERROR_INTEGER_OUT_OF_RANGE ||
		strcmp(status.sqlstate, "22003") != 0 ||
		strcmp(status.message, "bigint out of range") != 0 ||
		tess_int8_arith_scalar(TESS_ARITH_DIV, &column, 0, NULL, &small_rows,
							   small_results, &small_non_nulls,
							   &status) != TESS_ERROR_DIVISION_BY_ZERO ||
		strcmp(status.sqlstate, "22012") != 0 ||
		tess_int8_arith_scalar(TESS_ARITH_DIV, &column, -1, NULL, &small_rows,
							   small_results, &small_non_nulls,
							   &status) != TESS_ERROR_INTEGER_OUT_OF_RANGE ||
		tess_int8_arith_scalar((TessArithOp) 7, &column, 1, NULL, &small_rows,
							   small_results, &small_non_nulls,
							   &status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);

	/* A NULL operand never fails, and x % -1 is 0. */
	init_small_int8(&column, small_values, small_nulls, 1, PG_INT64_MIN,
					PG_INT64_MAX, true);
	if (tess_int8_arith_scalar(TESS_ARITH_MOD, &column, -1, NULL, &small_rows,
							   small_results, &small_non_nulls,
							   &status) != TESS_OK ||
		small_word != 5 || small_results[0] != 0 || small_results[2] != 0)
		PG_RETURN_BOOL(false);

	PG_RETURN_BOOL(true);
}

Datum
tessera_test_kernels_cast(PG_FUNCTION_ARGS)
{
	Datum		values[NROWS];
	bool		isnull[NROWS];
	uint64		words[NWORDS];
	Datum		results[NROWS];
	uint64		result_words[NWORDS] = {0};
	TessDatumColumn column;
	TessRowMask rows = {NROWS, words};
	TessRowMask non_nulls = {NROWS, result_words};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	int			row;

	/* Every selected non-NULL int4 becomes its int8 Datum. */
	fill(values, isnull, words);
	init_column(&column, values, isnull);
	if (tess_int4_to_int8(&column, NULL, &rows, results, &non_nulls,
						  &status) != TESS_OK)
		PG_RETURN_BOOL(false);
	for (row = 0; row < NROWS; row++)
	{
		bool		chosen = (words[row / 64] &
							  (UINT64CONST(1) << (row % 64))) != 0;
		bool		present = (result_words[row / 64] &
							   (UINT64CONST(1) << (row % 64))) != 0;

		if (present != (chosen && !isnull[row]))
			PG_RETURN_BOOL(false);
		if (present &&
			DatumGetInt64(results[row]) != (int64) DatumGetInt32(values[row]))
			PG_RETURN_BOOL(false);
	}

	/* A dimension error writes nothing. */
	column.nrows = NROWS - 1;
	if (tess_int4_to_int8(&column, NULL, &rows, results, &non_nulls,
						  &status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);

	/* Narrowed back, every selected non-NULL int8 is its int4 again. */
	{
		int32		narrow[NROWS];

		for (row = 0; row < NROWS; row++)
			results[row] = Int64GetDatum((int64) DatumGetInt32(values[row]));
		init_column(&column, results, isnull);
		memset(result_words, 0, sizeof(result_words));
		if (tess_int8_to_int4(&column, NULL, &rows, narrow, &non_nulls,
							  &status) != TESS_OK)
			PG_RETURN_BOOL(false);
		for (row = 0; row < NROWS; row++)
		{
			bool		chosen = (words[row / 64] &
								  (UINT64CONST(1) << (row % 64))) != 0;
			bool		present = (result_words[row / 64] &
								   (UINT64CONST(1) << (row % 64))) != 0;

			if (present != (chosen && !isnull[row]) ||
				(present && narrow[row] != DatumGetInt32(values[row])))
				PG_RETURN_BOOL(false);
		}
		/* A selected value past the int4 range fails as the cast does. */
		for (row = 0; row < NROWS; row++)
		{
			if ((words[row / 64] & (UINT64CONST(1) << (row % 64))) != 0 &&
				!isnull[row])
				break;
		}
		results[row] = Int64GetDatum(((int64) PG_INT32_MAX) + 1);
		if (tess_int8_to_int4(&column, NULL, &rows, narrow, &non_nulls,
							  &status) != TESS_ERROR_INTEGER_OUT_OF_RANGE ||
			strcmp(status.sqlstate, "22003") != 0 ||
			strcmp(status.message, "integer out of range") != 0)
			PG_RETURN_BOOL(false);
	}

	PG_RETURN_BOOL(true);
}

Datum
tessera_test_kernels_hashes(PG_FUNCTION_ARGS)
{
	Datum		values[3];
	bool		isnull[3];
	Datum		values8[3];
	bool		isnull8[3];
	TessDatumColumn column;
	TessDatumColumn column8;
	uint64		selection = 7;
	TessRowMask rows = {3, &selection};
	uint32		hashes[3] = {0xdeadbeef, 0xdeadbeef, 0xdeadbeef};
	uint64		valid_word = 0;
	TessRowMask valid = {3, &valid_word};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	const uint32 hash_of_1 = 0x514e28b7;
	const uint32 hash_of_42 = 0x087fcd5c;
	const uint32 null_hash = 0x92ca2f0e;

	/* Keys 1, NULL, 42: known murmurhash32 values from PostgreSQL. */
	init_small(&column, values, isnull, 1, 7, 42, true);
	if (tess_int4_hash(&column, NULL, &rows, TESS_NULL_KEYS_REJECT, hashes,
					   &valid, &status) != TESS_OK ||
		valid_word != 5 || hashes[0] != hash_of_1 || hashes[2] != hash_of_42 ||
		tess_int4_hash_next(&column, NULL, TESS_NULL_KEYS_REJECT, hashes,
							&valid, &status) != TESS_OK ||
		valid_word != 5 || hashes[0] != combine(hash_of_1, hash_of_1) ||
		hashes[2] != combine(hash_of_42, hash_of_42))
		PG_RETURN_BOOL(false);

	/* Under the group policy NULL is a key with a fixed hash. */
	if (tess_int4_hash(&column, NULL, &rows, TESS_NULL_KEYS_GROUP, hashes,
					   &valid, &status) != TESS_OK ||
		valid_word != 7 || hashes[1] != null_hash ||
		tess_int4_hash_next(&column, NULL, TESS_NULL_KEYS_GROUP, hashes,
							&valid, &status) != TESS_OK ||
		valid_word != 7 || hashes[1] != combine(null_hash, null_hash))
		PG_RETURN_BOOL(false);

	/* An unknown policy is rejected. */
	if (tess_int4_hash(&column, NULL, &rows, (TessNullKeys) 2, hashes,
					   &valid, &status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);

	/*
	 * int8 keys fold as hashint8 before murmurhash32: 2^40 folds to 256,
	 * -5000000000 to 0xd5fa0e01.
	 */
	init_small_int8(&column8, values8, isnull8, 1, INT64CONST(1) << 40,
					INT64CONST(-5000000000), false);
	if (tess_int8_hash(&column8, NULL, &rows, TESS_NULL_KEYS_REJECT, hashes,
					   &valid, &status) != TESS_OK ||
		valid_word != 7 || hashes[0] != hash_of_1 ||
		hashes[1] != 0x4570315f || hashes[2] != 0x548638f0)
		PG_RETURN_BOOL(false);

	/*
	 * An int8 inside the int4 range hashes as the int4, so the families
	 * chain: int4 keys 1, NULL, 42 then int8 keys 1, NULL, 42 give the
	 * hashes of two int4 keys.
	 */
	init_small_int8(&column8, values8, isnull8, 1, 7, 42, true);
	if (tess_int4_hash(&column, NULL, &rows, TESS_NULL_KEYS_REJECT, hashes,
					   &valid, &status) != TESS_OK ||
		tess_int8_hash_next(&column8, NULL, TESS_NULL_KEYS_REJECT, hashes,
							&valid, &status) != TESS_OK ||
		valid_word != 5 || hashes[0] != combine(hash_of_1, hash_of_1) ||
		hashes[2] != combine(hash_of_42, hash_of_42))
		PG_RETURN_BOOL(false);
	if (tess_int8_hash(&column8, NULL, &rows, TESS_NULL_KEYS_GROUP, hashes,
					   &valid, &status) != TESS_OK ||
		valid_word != 7 || hashes[1] != null_hash ||
		tess_int8_hash_next(&column8, NULL, (TessNullKeys) 2, hashes,
							&valid, &status) != TESS_ERROR_INVALID_ARGUMENT)
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

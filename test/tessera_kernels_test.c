#include "postgres.h"

#include <string.h>

#include "fmgr.h"

#include "tessera/kernels.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_kernels_layout);
PG_FUNCTION_INFO_V1(tessera_test_kernels_filter);
PG_FUNCTION_INFO_V1(tessera_test_kernels_errors);
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

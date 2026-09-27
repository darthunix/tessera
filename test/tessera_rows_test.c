#include "postgres.h"

#include "fmgr.h"
#include "utils/builtins.h"
#include "utils/memutils.h"
#include "varatt.h"

#include "tessera/kernel_ops.h"
#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_rows_cycle);
PG_FUNCTION_INFO_V1(tessera_test_rows_error);
PG_FUNCTION_INFO_V1(tessera_test_rows_sort);

/* The kernels of the table, which this module links. */
static const TessKernelOps kernels = {
	TESS_ABI_INITIALIZER(TESS_KERNEL_OPS_ABI_VERSION, TessKernelOps),
	.table_format_version = TESS_TABLE_FORMAT_VERSION,
	.table_size = tess_table_size,
	.table_create = tess_table_create,
	.table_chunk_init = tess_table_chunk_init,
	.table_append = tess_table_append,
	.table_gather = tess_table_gather,
	.sort_item_words = tess_sort_item_words,
	.sort_items = tess_sort_items,
	.sort = tess_sort,
	.table_append_columns = tess_table_append_columns,
};

#define BATCH 64
#define NCOLUMNS 3
/* A value past a quarter of a value chunk, which gets a chunk of its own. */
#define LARGE_LEN (300 * 1024)

static const int16 typlens[NCOLUMNS] = {4, -1, 8};
static const bool typbyvals[NCOLUMNS] = {true, false, true};

/*
 * Row i's columns: an int4 i; a text "v<i>", NULL in every 5th row and
 * LARGE_LEN bytes of 'x' in every 10007th; an int8 i << 33, NULL in every
 * 3rd row. Its key is i * 7919 modulo the rows.
 */
static bool
text_null(int i)
{
	return i % 5 == 0;
}

static bool
int8_null(int i)
{
	return i % 3 == 0;
}

static text *
text_value(int i)
{
	if (i % 10007 == 1)
	{
		char	   *large = palloc(LARGE_LEN + 1);

		memset(large, 'x', LARGE_LEN);
		large[LARGE_LEN] = '\0';
		return cstring_to_text(large);
	}
	return cstring_to_text(psprintf("v%d", i));
}

static TessRows *
make_rows(void)
{
	TessRowsConfig config = TESS_STRUCT_INITIALIZER(TessRowsConfig);
	TessTableKeyKind kind = TESS_TABLE_KEY_INT4;

	config.parent_context = CurrentMemoryContext;
	config.kernels = &kernels;
	config.nkeys = 1;
	config.kinds = &kind;
	config.ncolumns = NCOLUMNS;
	config.typlens = typlens;
	config.typbyvals = typbyvals;
	return tess_rows_create(&config);
}

/*
 * Append nrows rows in batches of BATCH, leaving out every 7th row of a
 * batch, and keep each appended row's reference; the rows left out get
 * 0.
 */
static void
append_all(TessRows *rows, int nrows, uint32 *refs)
{
	Datum		key_values[BATCH];
	bool		key_nulls[BATCH];
	Datum		values[NCOLUMNS][BATCH];
	bool		nulls[NCOLUMNS][BATCH];
	TessDatumColumn key_column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	TessDatumColumn columns[NCOLUMNS];
	TessTableKey key;
	uint64		bits[1];
	uint32		batch_refs[BATCH];
	MemoryContext scratch = AllocSetContextCreate(CurrentMemoryContext,
												  "rows test batch",
												  ALLOCSET_DEFAULT_SIZES);

	for (int first = 0; first < nrows; first += BATCH)
	{
		int			n = Min(BATCH, nrows - first);
		TessRowMask mask = {n, bits};
		MemoryContext old = MemoryContextSwitchTo(scratch);

		bits[0] = 0;
		for (int row = 0; row < n; row++)
		{
			int			i = first + row;

			key_values[row] = Int32GetDatum((int32) ((int64) i * 7919 % nrows));
			key_nulls[row] = false;
			values[0][row] = Int32GetDatum(i);
			nulls[0][row] = false;
			nulls[1][row] = text_null(i);
			values[1][row] = nulls[1][row] ? (Datum) 0 : PointerGetDatum(text_value(i));
			nulls[2][row] = int8_null(i);
			values[2][row] = nulls[2][row] ? (Datum) 0 : Int64GetDatum((int64) i << 33);
			if (row % 7 != 3)
				bits[0] |= UINT64CONST(1) << row;
			refs[i] = 0;
		}
		key_column.values = key_values;
		key_column.isnull = key_nulls;
		key_column.nrows = n;
		key = (TessTableKey) {TESS_TABLE_KEY_INT4, &key_column, NULL};
		for (int column = 0; column < NCOLUMNS; column++)
		{
			columns[column] = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
			columns[column].values = values[column];
			columns[column].isnull = nulls[column];
			columns[column].nrows = n;
		}
		tess_rows_append(rows, &key, columns, &mask, batch_refs);
		for (int row = 0; row < n; row++)
			if ((bits[0] >> row) & 1)
				refs[first + row] = batch_refs[row];
		MemoryContextSwitchTo(old);
		MemoryContextReset(scratch);
	}
	MemoryContextDelete(scratch);
}

/*
 * Read every appended row back, BATCH at a time from the last one down, a
 * gather's rows in an order other than their appending, and compare.
 */
static bool
check_all(TessRows *rows, int nrows, const uint32 *refs)
{
	uint32		batch_refs[BATCH];
	int			index[BATCH];
	Datum		values[BATCH];
	bool		nulls[BATCH];
	uint64		bits[1];
	int			i = nrows - 1;

	while (i >= 0)
	{
		int			n = 0;
		TessRowMask mask = {BATCH, bits};

		bits[0] = 0;
		for (; i >= 0 && n < BATCH; i--)
		{
			if (refs[i] == 0)
				continue;
			batch_refs[n] = refs[i];
			index[n] = i;
			bits[0] |= UINT64CONST(1) << n;
			n++;
		}
		if (n == 0)
			break;
		mask.nrows = n;
		for (int column = 0; column < NCOLUMNS; column++)
		{
			memset(nulls, true, sizeof(nulls));
			tess_rows_gather(rows, column, batch_refs, &mask, values, nulls);
			for (int row = 0; row < n; row++)
			{
				int			j = index[row];

				if (column == 0 &&
					(nulls[row] || DatumGetInt32(values[row]) != j))
					return false;
				if (column == 1)
				{
					text	   *expected;

					if (nulls[row] != text_null(j))
						return false;
					if (nulls[row])
						continue;
					expected = text_value(j);
					if (VARSIZE_ANY(DatumGetPointer(values[row])) != VARSIZE_ANY(expected) ||
						memcmp(DatumGetPointer(values[row]), expected,
							   VARSIZE_ANY(expected)) != 0)
						return false;
					pfree(expected);
				}
				if (column == 2 &&
					(nulls[row] != int8_null(j) ||
					 (!nulls[row] && DatumGetInt64(values[row]) != ((int64) j << 33))))
					return false;
			}
		}
	}
	return true;
}

/*
 * Rows over several chunks of records and of values, a value of a chunk of
 * its own, NULLs in both kinds of columns, then the same after a reset.
 */
Datum
tessera_test_rows_cycle(PG_FUNCTION_ARGS)
{
	int			nrows = PG_GETARG_INT32(0);
	TessRows   *rows = make_rows();
	uint32	   *refs = palloc(sizeof(uint32) * nrows);
	uint64		appended = 0;
	Size		memory;
	bool		ok;

	for (int i = 0; i < nrows; i++)
		if (i % BATCH % 7 != 3)
			appended++;
	append_all(rows, nrows, refs);
	ok = tess_rows_count(rows) == appended && check_all(rows, nrows, refs);
	memory = tess_rows_memory(rows);
	tess_rows_reset(rows);
	ok = ok && tess_rows_count(rows) == 0 && tess_rows_memory(rows) < memory;
	append_all(rows, nrows, refs);
	ok = ok && tess_rows_count(rows) == appended && check_all(rows, nrows, refs);
	tess_rows_free(rows);
	PG_RETURN_BOOL(ok);
}

/* 1: a column past the kept ones; 2: a config without kernels. */
Datum
tessera_test_rows_error(PG_FUNCTION_ARGS)
{
	int			which = PG_GETARG_INT32(0);

	if (which == 1)
	{
		TessRows   *rows = make_rows();
		uint32		ref = 0;
		uint64		bits[1] = {0};
		TessRowMask mask = {1, bits};
		Datum		value;
		bool		isnull;

		tess_rows_gather(rows, NCOLUMNS, &ref, &mask, &value, &isnull);
	}
	else
	{
		TessRowsConfig config = TESS_STRUCT_INITIALIZER(TessRowsConfig);

		config.parent_context = CurrentMemoryContext;
		(void) tess_rows_create(&config);
	}
	PG_RETURN_VOID();
}

/*
 * Sort the rows by their key, ascending and then descending, and check
 * the gathered rows come in the key's order: row i's key is
 * i * 7919 modulo the rows, a permutation, so the order is strict.
 */
Datum
tessera_test_rows_sort(PG_FUNCTION_ARGS)
{
	int			nrows = PG_GETARG_INT32(0);
	TessRows   *rows = make_rows();
	uint32	   *refs = palloc(sizeof(uint32) * nrows);
	uint32	   *sorted;
	uint64		count;
	bool		ok = true;

	append_all(rows, nrows, refs);
	count = tess_rows_count(rows);
	sorted = palloc(sizeof(uint32) * Max(count, 1));
	for (int direction = 0; direction < 2 && ok; direction++)
	{
		TessSortKey key = {TESS_TABLE_KEY_INT4,
		direction == 1 ? TESS_SORT_DESCENDING : 0};
		int64		last = direction == 1 ? PG_INT64_MAX : -1;

		tess_rows_sort(rows, &key, sorted);
		for (uint64 first = 0; first < count && ok; first += BATCH)
		{
			int			n = (int) Min((uint64) BATCH, count - first);
			uint64		bits[1] = {n == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << n) - 1};
			TessRowMask mask = {n, bits};
			Datum		values[BATCH];
			bool		nulls[BATCH];

			tess_rows_gather(rows, 0, &sorted[first], &mask, values, nulls);
			for (int row = 0; row < n; row++)
			{
				int64		key_value = (int64) DatumGetInt32(values[row]) * 7919 % nrows;

				if (nulls[row] ||
					(direction == 0 ? key_value <= last : key_value >= last))
					ok = false;
				last = key_value;
			}
		}
	}
	/* Rows without records sort to nothing. */
	tess_rows_reset(rows);
	{
		TessSortKey key = {TESS_TABLE_KEY_INT4, 0};

		tess_rows_sort(rows, &key, sorted);
	}
	tess_rows_free(rows);
	PG_RETURN_BOOL(ok && count > 0);
}

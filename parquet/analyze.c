/*
 * ANALYZE of a Parquet table: rows sampled at random over all the rows of
 * its files, uniformly, then found in their groups, and only those rows
 * read, every column of the table not dropped made into its values as a
 * query's scan makes them.
 */
#include "postgres.h"

#include <math.h>

#include "access/htup_details.h"
#include "common/pg_prng.h"
#include "foreign/fdwapi.h"
#include "miscadmin.h"
#include "port/pg_bitutils.h"
#include "utils/memutils.h"
#include "utils/rel.h"

#include "tessera_parquet.h"

#define WINDOW_WORDS (PARQUET_WINDOW_ROWS / 64)

/* What a sample reads of a column in a window: the values of the rows
 * selected, in their order. */
typedef struct SampleColumn
{
	uint64		values[PARQUET_WINDOW_ROWS];
	uint64		nulls[WINDOW_WORDS];
} SampleColumn;

/* The columns of the relation not dropped, by their numbers. */
static List *
columns_of(Relation rel)
{
	TupleDesc	desc = RelationGetDescr(rel);
	List	   *attnums = NIL;

	for (int a = 1; a <= desc->natts; a++)
	{
		if (!TupleDescAttr(desc, a - 1)->attisdropped)
			attnums = lappend_int(attnums, a);
	}
	return attnums;
}

static int
compare_positions(const void *a, const void *b)
{
	uint64		x = *(const uint64 *) a;
	uint64		y = *(const uint64 *) b;

	return x < y ? -1 : x > y;
}

/*
 * `k` distinct positions among `total` rows, `k` at most half of them,
 * each set as likely as another, in ascending order: drawn at random
 * until `k` distinct ones are found, which a few rounds do.
 */
static uint64 *
distinct_draws(uint64 total, int k)
{
	uint64	   *positions = palloc_array(uint64, Max(k, 1));
	int			found = 0;

	while (found < k)
	{
		int			kept = 0;

		CHECK_FOR_INTERRUPTS();
		for (int i = found; i < k; i++)
			positions[i] = pg_prng_uint64_range(&pg_global_prng_state, 0, total - 1);
		qsort(positions, k, sizeof(uint64), compare_positions);
		for (int i = 0; i < k; i++)
		{
			if (kept == 0 || positions[i] != positions[kept - 1])
				positions[kept++] = positions[i];
		}
		found = kept;
	}
	return positions;
}

/*
 * `k` distinct positions among `total` rows, `k` below `total`, each set
 * as likely as another, in ascending order: drawn, or, where they are
 * more than half of the rows, the rows left out drawn and the others
 * taken, so that the draws stay few.
 */
static uint64 *
sample_positions(uint64 total, int k)
{
	uint64	   *positions;
	uint64	   *left;
	int			out;
	int			next = 0;
	int			taken = 0;

	if ((uint64) k <= total / 2)
		return distinct_draws(total, k);
	/* Fewer than 2k rows: the complement of total - k drawn ones. */
	out = (int) (total - (uint64) k);
	left = distinct_draws(total, out);
	positions = palloc_array(uint64, k);
	for (uint64 r = 0; r < total; r++)
	{
		if (next < out && left[next] == r)
			next++;
		else
			positions[taken++] = r;
	}
	pfree(left);
	return positions;
}

/* Reads the window of the group read from row `start`, of `len` rows, for
 * the rows selected: their values in `columns`; returns their count. */
static size_t
read_window(ParquetScan *scan, uint64 start, size_t len, SampleColumn *columns)
{
	size_t		words = (len + 63) / 64;
	size_t		count = 0;

	for (int c = 0; c < scan->ncolumns; c++)
	{
		TessParquetReader *reader = parquet_scan_reader(scan, c);
		uint64		rows[WINDOW_WORDS] = {0};
		uint64		held[WINDOW_WORDS] = {0};
		uint64		asked[WINDOW_WORDS] = {0};
		TessParquetStatus status;
		size_t		held_words;

		if (!tess_parquet_read(reader, start, len, rows, words, held, words, &count, &status))
			parquet_raise(&status);
		if (count == 0)
			continue;
		held_words = (count + 63) / 64;
		for (size_t p = 0; p < count; p++)
			asked[p / 64] |= UINT64CONST(1) << (p % 64);
		memset(columns[c].nulls, 0, sizeof(columns[c].nulls));
		if (scan->info[c].gives_datums)
		{
			if (!tess_parquet_values(reader, asked, held_words, columns[c].values, count,
									 columns[c].nulls, held_words, &status))
				parquet_raise(&status);
		}
		else
		{
			size_t		bytes = 0;
			uint8	   *arena;

			if (!tess_parquet_text_size(reader, asked, held_words, columns[c].nulls, held_words,
										&bytes, &status))
				parquet_raise(&status);
			arena = MemoryContextAlloc(scan->window_cxt, Max(bytes, 1));
			if (!tess_parquet_text(reader, arena, bytes, columns[c].values, count, &status))
				parquet_raise(&status);
		}
	}
	return count;
}

static int
sample_rows(Relation rel, int elevel, HeapTuple *rows, int targrows, double *totalrows,
			double *totaldeadrows)
{
	TupleDesc	desc = RelationGetDescr(rel);
	MemoryContext caller = CurrentMemoryContext;
	MemoryContext work = AllocSetContextCreate(caller, "tessera_parquet analyze",
											   ALLOCSET_DEFAULT_SIZES);
	Datum	   *values = palloc_array(Datum, Max(desc->natts, 1));
	bool	   *nulls = palloc_array(bool, Max(desc->natts, 1));
	ParquetScan *scan;
	SampleColumn *columns;
	uint64		total = 0;
	uint64		seen = 0;
	uint64	   *positions = NULL;
	int			wanted;
	int			next = 0;
	int			taken = 0;

	MemoryContextSwitchTo(work);
	scan = parquet_scan_begin(RelationGetRelid(rel), parquet_table_files(RelationGetRelid(rel)),
							  columns_of(rel));
	/* Zeroed: the reader writes into initialized words alone. */
	columns = palloc0_array(SampleColumn, Max(scan->ncolumns, 1));
	for (int f = 0; f < list_length(scan->paths); f++)
	{
		for (size_t g = 0; g < scan->groups[f]; g++)
			total += scan->group_rows[f][g];
	}
	*totalrows = (double) total;
	*totaldeadrows = 0;
	/* Every row where the files hold no more than the sample. */
	wanted = (int) Min(total, (uint64) targrows);
	if (total > (uint64) targrows)
		positions = sample_positions(total, targrows);
	while (taken < wanted && parquet_scan_next_group(scan))
	{
		uint64		group = parquet_scan_group_rows(scan);
		uint64	   *mask;
		bool		any = false;

		/* The readers of the group read the rows of the sample alone. */
		MemoryContextSwitchTo(scan->group_cxt);
		mask = palloc0_array(uint64, Max((group + 63) / 64, 1));
		if (positions == NULL)
		{
			for (uint64 r = 0; r < group; r++)
				mask[r / 64] |= UINT64CONST(1) << (r % 64);
			any = group > 0;
		}
		/* The positions taken in this group, the next ones in order. */
		for (; positions != NULL && next < targrows && positions[next] < seen + group; next++)
		{
			uint64		r = positions[next] - seen;

			mask[r / 64] |= UINT64CONST(1) << (r % 64);
			any = true;
		}
		seen += group;
		MemoryContextSwitchTo(work);
		if (!any)
			continue;
		scan->selection = mask;
		for (uint64 start = 0; start < group && taken < wanted; start += PARQUET_WINDOW_ROWS)
		{
			size_t		len = (size_t) Min((uint64) PARQUET_WINDOW_ROWS, group - start);
			size_t		count;

			CHECK_FOR_INTERRUPTS();
			count = read_window(scan, start, len, columns);
			MemoryContextSwitchTo(caller);
			for (size_t p = 0; p < count && taken < wanted; p++)
			{
				for (int a = 0; a < desc->natts; a++)
				{
					values[a] = (Datum) 0;
					nulls[a] = true;
				}
				for (int c = 0; c < scan->ncolumns; c++)
				{
					int			a = scan->columns[c].attnum - 1;

					nulls[a] = (columns[c].nulls[p / 64] >> (p % 64)) & 1;
					values[a] = nulls[a] ? (Datum) 0 : (Datum) columns[c].values[p];
				}
				rows[taken++] = heap_form_tuple(desc, values, nulls);
			}
			MemoryContextSwitchTo(work);
			MemoryContextReset(scan->window_cxt);
		}
	}
	parquet_scan_end(scan);
	MemoryContextSwitchTo(caller);
	MemoryContextDelete(work);
	ereport(elevel,
			(errmsg("\"%s\": %d rows sampled of %.0f in its Parquet files",
					RelationGetRelationName(rel), taken, *totalrows)));
	return taken;
}

/* ANALYZE of the table: its sample, and its files' pages, a page a
 * BLCKSZ bytes. */
bool
parquet_analyze(Relation relation, AcquireSampleRowsFunc *func, BlockNumber *totalpages)
{
	ParquetFound found = {0};
	List	   *paths = parquet_expand(parquet_table_files(RelationGetRelid(relation)));

	(void) parquet_check_files(paths, RelationGetRelid(relation), NIL, NULL, NULL, NULL, &found);
	*func = sample_rows;
	*totalpages = (BlockNumber) Min(ceil(found.file_bytes / BLCKSZ), (double) MaxBlockNumber);
	return true;
}

/*
 * The rows of the wrapper's scan: the files read one by one, each checked
 * against what fstat said of it when the scan began, each group of rows
 * read window by window, every column the query reads made into its
 * PostgreSQL values through the reader, and the rows given out one by
 * one. A query that reads no column gets empty rows, counted from the
 * footers, and reads no data page.
 */
#include "postgres.h"

#include "executor/tuptable.h"
#include "miscadmin.h"
#include "utils/injection_point.h"
#include "utils/memutils.h"

#include "tessera_parquet.h"

/* The rows a window holds at most. */
#define WINDOW_ROWS 1024

static size_t
words_of(size_t rows)
{
	return (rows + 63) / 64;
}

/* A mask of `rows` rows, every bit set. */
static uint64 *
every_row(size_t rows)
{
	size_t		words = words_of(rows);
	uint64	   *mask = palloc_array(uint64, Max(words, 1));

	for (size_t w = 0; w < words; w++)
		mask[w] = ~UINT64CONST(0);
	if (rows % 64 != 0)
		mask[words - 1] = (UINT64CONST(1) << (rows % 64)) - 1;
	return mask;
}

ParquetScan *
parquet_scan_begin(Oid relid, List *patterns, List *attnums)
{
	ParquetScan *scan = palloc0_object(ParquetScan);
	int			i = 0;

	scan->relid = relid;
	scan->paths = parquet_expand(patterns);
	scan->identities = palloc0_array(TessParquetIdentity, Max(list_length(scan->paths), 1));
	scan->groups = palloc0_array(size_t, Max(list_length(scan->paths), 1));
	scan->group_rows = palloc0_array(uint64 *, Max(list_length(scan->paths), 1));
	/* The files as they are now, their columns checked again. */
	(void) parquet_check_files(scan->paths, relid, attnums, scan->identities, scan->groups,
							   scan->group_rows);
	scan->ncolumns = list_length(attnums);
	scan->columns = palloc_array(ParquetColumn, Max(scan->ncolumns, 1));
	foreach_int(attnum, attnums)
		scan->columns[i++] = parquet_column(relid, attnum);
	scan->file_cxt = AllocSetContextCreate(CurrentMemoryContext, "tessera_parquet file",
										   ALLOCSET_DEFAULT_SIZES);
	/* A sibling of the file's, not its child: a reset deletes children. */
	scan->group_cxt = AllocSetContextCreate(CurrentMemoryContext, "tessera_parquet group",
											ALLOCSET_DEFAULT_SIZES);
	scan->window_cxt = AllocSetContextCreate(CurrentMemoryContext, "tessera_parquet window",
											 ALLOCSET_DEFAULT_SIZES);
	parquet_scan_rescan(scan);
	return scan;
}

void
parquet_scan_rescan(ParquetScan *scan)
{
	MemoryContextReset(scan->window_cxt);
	MemoryContextReset(scan->group_cxt);
	MemoryContextReset(scan->file_cxt);
	scan->file_index = -1;
	scan->file = NULL;
	scan->group = 0;
	scan->window_start = 0;
	scan->window_len = 0;
	scan->row = 0;
	scan->group_started = false;
}

/* Ends the scan: its file closed, its descriptor given back, now rather
 * than with the query's memory. */
void
parquet_scan_end(ParquetScan *scan)
{
	MemoryContextDelete(scan->window_cxt);
	MemoryContextDelete(scan->group_cxt);
	MemoryContextDelete(scan->file_cxt);
}

/* Opens the next file, its columns mapped; false past the last. */
static bool
next_file(ParquetScan *scan)
{
	MemoryContext outer;

	MemoryContextReset(scan->window_cxt);
	MemoryContextReset(scan->group_cxt);
	MemoryContextReset(scan->file_cxt);
	scan->file = NULL;
	scan->file_index++;
	if (scan->file_index >= list_length(scan->paths))
		return false;
	scan->group = 0;
	scan->group_started = false;
	/* Without a column read, the footer's counts give the rows. */
	if (scan->ncolumns == 0)
		return true;
	outer = MemoryContextSwitchTo(scan->file_cxt);
	scan->file = parquet_open_file(list_nth(scan->paths, scan->file_index),
								   &scan->identities[scan->file_index]);
	scan->mapped = palloc_array(TessParquetColumn *, scan->ncolumns);
	scan->info = palloc_array(TessParquetColumnInfo, scan->ncolumns);
	scan->readers = palloc0_array(TessParquetReader *, scan->ncolumns);
	for (int c = 0; c < scan->ncolumns; c++)
	{
		TessParquetStatus status;

		scan->mapped[c] = parquet_map_column(scan->file, &scan->columns[c]);
		if (!tess_parquet_column_info(scan->mapped[c], &scan->info[c], &status))
			parquet_raise(&status);
	}
	MemoryContextSwitchTo(outer);
	return true;
}

static void
free_reader(void *arg)
{
	tess_parquet_reader_free((TessParquetReader *) arg);
}

/* The rows of the current file's group `group`. */
static uint64
rows_of_group(ParquetScan *scan)
{
	return scan->group_rows[scan->file_index][scan->group];
}

/* Starts the current group: a reader of each column, every row selected,
 * owned by the group's context, which the next group resets. */
static void
start_group(ParquetScan *scan)
{
	uint64		rows = rows_of_group(scan);
	MemoryContext outer;

	MemoryContextReset(scan->window_cxt);
	MemoryContextReset(scan->group_cxt);
	outer = MemoryContextSwitchTo(scan->group_cxt);
	scan->window_start = 0;
	scan->window_len = 0;
	scan->row = 0;
	scan->group_started = true;
	/*
	 * shortcut: a group's selection is one palloc'd mask, which the reader
	 * copies, so a group of more rows than it holds bits is refused; lift
	 * it with huge allocations here and a fallible copy in the reader if
	 * such groups are met.
	 */
	if (words_of(rows) > MaxAllocSize / sizeof(uint64))
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("a group of rows of Parquet file \"%s\" holds " UINT64_FORMAT
						" rows, more than a scan selects",
						(char *) list_nth(scan->paths, scan->file_index), rows)));
	for (int c = 0; c < scan->ncolumns; c++)
	{
		uint64	   *selection = every_row(rows);
		MemoryContextCallback *callback = palloc0_object(MemoryContextCallback);
		TessParquetStatus status;

		scan->readers[c] = NULL;
		if (!tess_parquet_reader(scan->file->file, scan->mapped[c], scan->group, selection,
								 words_of(rows), WINDOW_ROWS, &scan->readers[c], &status))
			parquet_raise(&status);
		callback->func = free_reader;
		callback->arg = scan->readers[c];
		MemoryContextRegisterResetCallback(scan->group_cxt, callback);
	}
	MemoryContextSwitchTo(outer);
}

/* Reads the next window of the current group into the window's arrays. */
static void
read_window(ParquetScan *scan)
{
	uint64		rows = rows_of_group(scan);
	size_t		len = (size_t) Min((uint64) WINDOW_ROWS, rows - scan->window_start);
	size_t		words = words_of(len);
	MemoryContext outer;

	MemoryContextReset(scan->window_cxt);
	outer = MemoryContextSwitchTo(scan->window_cxt);
	scan->values = palloc_array(uint64 *, Max(scan->ncolumns, 1));
	scan->nulls = palloc_array(uint64 *, Max(scan->ncolumns, 1));
	for (int c = 0; c < scan->ncolumns; c++)
	{
		uint64	   *asked = every_row(len);
		uint64	   *held = palloc0_array(uint64, Max(words, 1));
		size_t		count = 0;
		TessParquetStatus status;

		scan->values[c] = palloc0_array(uint64, Max(len, 1));
		scan->nulls[c] = palloc0_array(uint64, Max(words, 1));
		if (!tess_parquet_read(scan->readers[c], scan->window_start, len, asked, words, held,
							   words, &count, &status))
			parquet_raise(&status);
		if (count != len)
			ereport(ERROR,
					(errcode(ERRCODE_DATA_CORRUPTED),
					 errmsg("Parquet column holds fewer rows than its group")));
		if (scan->info[c].gives_datums)
		{
			if (!tess_parquet_values(scan->readers[c], asked, words, scan->values[c], len,
									 scan->nulls[c], words, &status))
				parquet_raise(&status);
		}
		else
		{
			size_t		bytes = 0;
			uint8	   *arena;

			if (!tess_parquet_text_size(scan->readers[c], asked, words, scan->nulls[c], words,
										&bytes, &status))
				parquet_raise(&status);
			arena = palloc(Max(bytes, 1));
			if (!tess_parquet_text(scan->readers[c], arena, bytes, scan->values[c], len,
								   &status))
				parquet_raise(&status);
		}
	}
	MemoryContextSwitchTo(outer);
	scan->window_len = len;
	scan->row = 0;
	INJECTION_POINT("tessera-parquet-window", NULL);
}

/*
 * The next row into `slot`, or the slot cleared past the last row. A
 * cancel is checked between windows.
 */
TupleTableSlot *
parquet_scan_next(ParquetScan *scan, TupleTableSlot *slot)
{
	ExecClearTuple(slot);
	/* Past the last file, until a rescan. */
	if (scan->file_index >= list_length(scan->paths))
		return slot;
	while (scan->row >= scan->window_len)
	{
		CHECK_FOR_INTERRUPTS();
		/* The next window of the group read. */
		if (scan->group_started &&
			scan->window_start + scan->window_len < rows_of_group(scan))
		{
			scan->window_start += scan->window_len;
			read_window(scan);
			continue;
		}
		/* The next group, of this file or of the next. */
		if (scan->group_started)
		{
			scan->group++;
			scan->group_started = false;
		}
		if (scan->file_index < 0 ||
			scan->group >= scan->groups[scan->file_index])
		{
			if (!next_file(scan))
				return slot;
			continue;
		}
		if (scan->ncolumns == 0)
		{
			/* Empty rows, as many as the group holds, no page read. */
			scan->group_started = true;
			scan->window_start = 0;
			scan->window_len = (size_t) rows_of_group(scan);
			scan->row = 0;
			continue;
		}
		start_group(scan);
		if (rows_of_group(scan) > 0)
			read_window(scan);
	}
	for (int a = 0; a < slot->tts_tupleDescriptor->natts; a++)
	{
		slot->tts_values[a] = (Datum) 0;
		slot->tts_isnull[a] = true;
	}
	for (int c = 0; c < scan->ncolumns; c++)
	{
		int			a = scan->columns[c].attnum - 1;
		size_t		r = scan->row;

		slot->tts_isnull[a] = (scan->nulls[c][r / 64] >> (r % 64)) & 1;
		slot->tts_values[a] = slot->tts_isnull[a] ? (Datum) 0 : (Datum) scan->values[c][r];
	}
	scan->row++;
	return ExecStoreVirtualTuple(slot);
}

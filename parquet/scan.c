/*
 * The rows of the wrapper's scan: the files read one by one, each checked
 * against what fstat said of it when the scan began, each group of rows
 * read window by window, every column the query reads made into its
 * PostgreSQL values through the reader, and the rows given out one by
 * one. A query that reads no column gets empty rows, counted from the
 * footers, and reads no data page.
 */
#include "postgres.h"

#include "access/parallel.h"
#include "executor/tuptable.h"
#include "miscadmin.h"
#include "port/atomics.h"
#include "utils/injection_point.h"
#include "utils/memutils.h"

#include "tessera/runtime.h"

#include "tessera_parquet.h"

/*
 * The list a parallel scan's leader shares: the files it found when its
 * scan began, what fstat said of each, the rows of each group, and the
 * counter of the groups taken, over the files in their order, so that the
 * participants take the groups in turn. After the header come the
 * identities, the groups of each file, the rows of each group, the paths,
 * each ended by a zero byte, and the participants' counters.
 */
typedef struct ParquetShared
{
	pg_atomic_uint64 next;		/* the next group to take */
	int			nfiles;
	uint64		ngroups;
	Size		stats_offset;	/* where the counters start */
} ParquetShared;

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

static void scan_contexts(ParquetScan *scan);

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
							   scan->group_rows, NULL);
	scan->ncolumns = list_length(attnums);
	scan->columns = palloc_array(ParquetColumn, Max(scan->ncolumns, 1));
	foreach_int(attnum, attnums)
		scan->columns[i++] = parquet_column(relid, attnum);
	scan_contexts(scan);
	return scan;
}

/* The scan's contexts and the first group of each file, its state reset. */
static void
scan_contexts(ParquetScan *scan)
{
	uint64		first = 0;

	scan->first_group = palloc_array(uint64, Max(list_length(scan->paths), 1));
	for (int f = 0; f < list_length(scan->paths); f++)
	{
		scan->first_group[f] = first;
		first += scan->groups[f];
	}
	scan->file_cxt = AllocSetContextCreate(CurrentMemoryContext, "tessera_parquet file",
										   ALLOCSET_DEFAULT_SIZES);
	/* A sibling of the file's, not its child: a reset deletes children. */
	scan->group_cxt = AllocSetContextCreate(CurrentMemoryContext, "tessera_parquet group",
											ALLOCSET_DEFAULT_SIZES);
	scan->window_cxt = AllocSetContextCreate(CurrentMemoryContext, "tessera_parquet window",
											 ALLOCSET_DEFAULT_SIZES);
	parquet_scan_rescan(scan);
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
	if (scan->stats != NULL)
		tess_shared_stats_end(scan->stats);
	MemoryContextDelete(scan->window_cxt);
	MemoryContextDelete(scan->group_cxt);
	MemoryContextDelete(scan->file_cxt);
}

/* Opens the file `f`, its columns mapped. */
static void
open_file(ParquetScan *scan, int f)
{
	MemoryContext outer;

	MemoryContextReset(scan->window_cxt);
	MemoryContextReset(scan->group_cxt);
	MemoryContextReset(scan->file_cxt);
	scan->file = NULL;
	scan->file_index = f;
	scan->group = 0;
	scan->group_started = false;
	/* Without a column read, the footer's counts give the rows. */
	if (scan->ncolumns == 0)
		return;
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
}

/* Opens the next file; false past the last. */
static bool
next_file(ParquetScan *scan)
{
	if (scan->file_index + 1 >= list_length(scan->paths))
	{
		MemoryContextReset(scan->window_cxt);
		MemoryContextReset(scan->group_cxt);
		MemoryContextReset(scan->file_cxt);
		scan->file = NULL;
		scan->file_index = list_length(scan->paths);
		return false;
	}
	open_file(scan, scan->file_index + 1);
	return true;
}

/*
 * A parallel scan's next group: the one the shared counter gives, its
 * file opened where it is another than the one read; false past the
 * last. A participant's groups come in the list's order.
 */
static bool
take_group(ParquetScan *scan)
{
	uint64		g = pg_atomic_fetch_add_u64(&scan->shared->next, 1);
	int			f = Max(scan->file_index, 0);

	if (g >= scan->shared->ngroups)
	{
		scan->file_index = list_length(scan->paths);
		return false;
	}
	while (g >= scan->first_group[f] + scan->groups[f])
		f++;
	if (f != scan->file_index)
		open_file(scan, f);
	scan->group = (size_t) (g - scan->first_group[f]);
	return true;
}

static TessParquetReader *make_reader(ParquetScan *scan, int c, MemoryContext cxt,
									  uint64 *chosen);

static void
free_reader(void *arg)
{
	tess_parquet_reader_free((TessParquetReader *) arg);
}

/* The rows of the group read. */
uint64
parquet_scan_group_rows(const ParquetScan *scan)
{
	return scan->group_rows[scan->file_index][scan->group];
}

/* Starts the current group, its readers made when first asked for. */
static void
start_group(ParquetScan *scan)
{
	MemoryContextReset(scan->window_cxt);
	MemoryContextReset(scan->group_cxt);
	scan->window_start = 0;
	scan->window_len = 0;
	scan->row = 0;
	scan->group_started = true;
	scan->selection = NULL;
	scan->own[PARQUET_GROUPS_READ]++;
	if (scan->readers != NULL)
		memset(scan->readers, 0, sizeof(TessParquetReader *) * scan->ncolumns);
}

/*
 * Moves to the next group of rows, of the file read or of the next one;
 * false past the last, until a rescan.
 */
bool
parquet_scan_next_group(ParquetScan *scan)
{
	if (scan->file_index >= list_length(scan->paths))
		return false;
	if (scan->shared != NULL)
	{
		scan->group_started = false;
		if (!take_group(scan))
			return false;
		start_group(scan);
		return true;
	}
	if (scan->group_started)
	{
		scan->group++;
		scan->group_started = false;
	}
	while (scan->file_index < 0 || scan->group >= scan->groups[scan->file_index])
	{
		if (!next_file(scan))
			return false;
	}
	start_group(scan);
	return true;
}

/*
 * The reader of the scan's column `c` over the group read, the rows of
 * `selection` selected, or every row, made at its first ask and owned by
 * the group's context, which the next group resets.
 */
TessParquetReader *
parquet_scan_reader(ParquetScan *scan, int c)
{
	if (scan->readers[c] == NULL)
		scan->readers[c] = make_reader(scan, c, scan->group_cxt, scan->selection);
	return scan->readers[c];
}

/*
 * A reader of column c of the group read, for every row of it, freed with
 * cxt, a context the group's outlives: the reader of the pass over the
 * columns of the clauses the source checks, before the group's selection
 * is known.
 */
TessParquetReader *
parquet_scan_every_row_reader(ParquetScan *scan, int c, MemoryContext cxt)
{
	return make_reader(scan, c, cxt, NULL);
}

/*
 * The group's selection, a bit a row, palloc'd in the group's context: the
 * readers made from now on read its rows alone.
 */
void
parquet_scan_select(ParquetScan *scan, uint64 *selection)
{
	scan->selection = selection;
}

/*
 * A reader of column c of the group read for the rows of selection, every
 * row where it is NULL, freed when cxt is reset or deleted.
 */
static TessParquetReader *
make_reader(ParquetScan *scan, int c, MemoryContext cxt, uint64 *chosen)
{
	uint64		rows = parquet_scan_group_rows(scan);
	MemoryContext outer;
	uint64	   *selection;
	MemoryContextCallback *callback;
	TessParquetStatus status;
	TessParquetReader *reader;

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
	outer = MemoryContextSwitchTo(cxt);
	selection = chosen != NULL ? chosen : every_row(rows);
	callback = palloc0_object(MemoryContextCallback);
	if (!tess_parquet_reader(scan->file->file, scan->mapped[c], scan->group, selection,
							 words_of(rows), PARQUET_WINDOW_ROWS, &reader, &status))
		parquet_raise(&status);
	callback->func = free_reader;
	callback->arg = reader;
	MemoryContextRegisterResetCallback(cxt, callback);
	if (selection != chosen)
		pfree(selection);
	MemoryContextSwitchTo(outer);
	return reader;
}

/* Reads the next window of the current group into the window's arrays. */
static void
read_window(ParquetScan *scan)
{
	uint64		rows = parquet_scan_group_rows(scan);
	size_t		len = (size_t) Min((uint64) PARQUET_WINDOW_ROWS, rows - scan->window_start);
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
		if (!tess_parquet_read(parquet_scan_reader(scan, c), scan->window_start, len, asked, words, held,
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
			scan->window_start + scan->window_len < parquet_scan_group_rows(scan))
		{
			scan->window_start += scan->window_len;
			read_window(scan);
			continue;
		}
		if (!parquet_scan_next_group(scan))
			return slot;
		if (scan->ncolumns == 0)
		{
			/* Empty rows, as many as the group holds, no page read. */
			scan->window_len = (size_t) parquet_scan_group_rows(scan);
			continue;
		}
		if (parquet_scan_group_rows(scan) > 0)
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

/* ---- a parallel scan ---- */

static uint64
total_groups(const ParquetScan *scan)
{
	uint64		total = 0;

	for (int f = 0; f < list_length(scan->paths); f++)
		total += scan->groups[f];
	return total;
}

/* The bytes of the list before the counters. */
static Size
list_size(const ParquetScan *scan)
{
	Size		size = MAXALIGN(sizeof(ParquetShared));
	int			nfiles = list_length(scan->paths);

	size = add_size(size, mul_size(nfiles, sizeof(TessParquetIdentity)));
	size = add_size(size, mul_size(nfiles, sizeof(uint64)));
	size = add_size(size, mul_size(total_groups(scan), sizeof(uint64)));
	foreach_ptr(char, path, scan->paths)
		size = add_size(size, strlen(path) + 1);
	return MAXALIGN(size);
}

/* The shared memory of the leader's scan: its list and the counters. */
Size
parquet_scan_estimate(const ParquetScan *scan, int nworkers)
{
	return add_size(list_size(scan), tess_shared_stats_estimate(PARQUET_COUNTERS, nworkers));
}

/* The leader's list of files and groups, laid out in `coordinate`. */
void
parquet_scan_share(ParquetScan *scan, void *coordinate, ParallelContext *pcxt,
				   MemoryContext cxt)
{
	ParquetShared *shared = coordinate;
	int			nfiles = list_length(scan->paths);
	char	   *at = (char *) coordinate + MAXALIGN(sizeof(ParquetShared));

	pg_atomic_init_u64(&shared->next, 0);
	shared->nfiles = nfiles;
	shared->ngroups = total_groups(scan);
	shared->stats_offset = list_size(scan);
	memcpy(at, scan->identities, sizeof(TessParquetIdentity) * nfiles);
	at += sizeof(TessParquetIdentity) * nfiles;
	for (int f = 0; f < nfiles; f++)
	{
		((uint64 *) at)[f] = scan->groups[f];
	}
	at += sizeof(uint64) * nfiles;
	for (int f = 0; f < nfiles; f++)
	{
		memcpy(at, scan->group_rows[f], sizeof(uint64) * scan->groups[f]);
		at += sizeof(uint64) * scan->groups[f];
	}
	foreach_ptr(char, path, scan->paths)
	{
		strcpy(at, path);
		at += strlen(path) + 1;
	}
	scan->shared = shared;
	/* Without a segment the core puts the list in private memory, which it
	 * frees before the scan ends. */
	scan->private_shared = pcxt->seg == NULL;
	scan->stats = tess_shared_stats_setup(scan->stats, cxt,
										  (char *) coordinate + shared->stats_offset,
										  PARQUET_COUNTERS, pcxt->nworkers, pcxt->seg);
}

/*
 * The leader, at a rescan of the parallel plan: the groups from the
 * first. The workers of the scan that ended have stored their counters
 * and exited, and the leader stores its own only when the plan shuts
 * down: their counters join the leader's, and the rows are laid out
 * anew, so that EXPLAIN totals every scan. The handle of the ended scan,
 * some hundred bytes, stays in the query's context until the query ends,
 * since the runtime frees none.
 */
void
parquet_scan_reshare(ParquetScan *scan, ParallelContext *pcxt, MemoryContext cxt)
{
	const uint64 *ended;

	pg_atomic_write_u64(&scan->shared->next, 0);
	tess_shared_stats_collect(scan->stats);
	ended = tess_shared_stats_totals(scan->stats);
	for (int c = 0; c < PARQUET_COUNTERS && ended != NULL; c++)
		scan->own[c] += ended[c];
	scan->stats = tess_shared_stats_setup(scan->stats, cxt,
										  (char *) scan->shared + scan->shared->stats_offset,
										  PARQUET_COUNTERS, pcxt->nworkers, pcxt->seg);
}

/*
 * A worker's scan of the leader's list: the files the leader found, each
 * checked when the worker opens it against what fstat said of it then.
 */
ParquetScan *
parquet_scan_attach(Oid relid, List *attnums, void *coordinate, MemoryContext cxt)
{
	ParquetShared *shared = coordinate;
	MemoryContext outer = MemoryContextSwitchTo(cxt);
	ParquetScan *scan = palloc0_object(ParquetScan);
	int			nfiles = shared->nfiles;
	const char *at = (const char *) coordinate + MAXALIGN(sizeof(ParquetShared));
	const uint64 *groups;
	int			i = 0;

	scan->relid = relid;
	scan->identities = palloc_array(TessParquetIdentity, Max(nfiles, 1));
	memcpy(scan->identities, at, sizeof(TessParquetIdentity) * nfiles);
	at += sizeof(TessParquetIdentity) * nfiles;
	groups = (const uint64 *) at;
	at += sizeof(uint64) * nfiles;
	scan->groups = palloc0_array(size_t, Max(nfiles, 1));
	scan->group_rows = palloc0_array(uint64 *, Max(nfiles, 1));
	for (int f = 0; f < nfiles; f++)
	{
		scan->groups[f] = (size_t) groups[f];
		scan->group_rows[f] = palloc_array(uint64, Max(groups[f], 1));
		memcpy(scan->group_rows[f], at, sizeof(uint64) * groups[f]);
		at += sizeof(uint64) * groups[f];
	}
	for (int f = 0; f < nfiles; f++)
	{
		scan->paths = lappend(scan->paths, pstrdup(at));
		at += strlen(at) + 1;
	}
	scan->ncolumns = list_length(attnums);
	scan->columns = palloc_array(ParquetColumn, Max(scan->ncolumns, 1));
	foreach_int(attnum, attnums)
		scan->columns[i++] = parquet_column(relid, attnum);
	scan_contexts(scan);
	scan->shared = shared;
	scan->stats = tess_shared_stats_attach(cxt, (char *) coordinate + shared->stats_offset,
										   ParallelWorkerNumber + 1);
	MemoryContextSwitchTo(outer);
	return scan;
}

/* At the parallel plan's shutdown: this participant's counters stored. */
void
parquet_scan_unshare(ParquetScan *scan)
{
	/* A serial plan shuts its scan down too, without shared memory. */
	if (scan->stats != NULL)
		tess_shared_stats_store(scan->stats, scan->own);
	/* Private memory is gone before the scan ends: summed now. */
	if (scan->stats != NULL && scan->private_shared)
		tess_shared_stats_collect(scan->stats);
	scan->shared = NULL;
}

/* The group started is skipped, not read: counted so. */
void
parquet_scan_skip_group(ParquetScan *scan)
{
	scan->own[PARQUET_GROUPS_READ]--;
	scan->own[PARQUET_GROUPS_SKIPPED]++;
}

/* For EXPLAIN: the counters of every participant, or this scan's own. */
const uint64 *
parquet_scan_counts(const ParquetScan *scan)
{
	return tess_shared_stats_totals_or(scan->stats, scan->own);
}

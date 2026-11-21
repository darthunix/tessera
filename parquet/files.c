/*
 * The files of a Parquet table: its patterns expanded, each file opened
 * through the reader, its footer read through the backend's cache, and its
 * columns mapped. Every object of the reader is freed when the memory
 * context current when it was made is reset or deleted, after an error
 * too.
 */
#include "postgres.h"

#include <glob.h>

#include "mb/pg_wchar.h"
#include "storage/fd.h"
#include "utils/injection_point.h"
#include "utils/memutils.h"

#include "tessera_parquet.h"

/* Raises the error of a call into the reader that failed: its message,
 * UTF-8, in the database's encoding, the names it quotes the catalog's. */
void
parquet_raise(const TessParquetStatus *status)
{
	const char *s = status->sqlstate;
	int			code = MAKE_SQLSTATE(s[0], s[1], s[2], s[3], s[4]);
	char	   *message = pg_any_to_server(status->message, strlen(status->message), PG_UTF8);

	ereport(ERROR, (errcode(code), errmsg("%s", message)));
	pg_unreachable();
}

/* The order of the names of a pattern's matches: their bytes, whatever
 * the database's collation or the C library. */
static int
compare_paths(const void *a, const void *b)
{
	return strcmp(*(char *const *) a, *(char *const *) b);
}

/*
 * The files of `patterns`, in their order, each pattern's matches in the
 * order of their names' bytes; a pattern that matches no file is
 * refused, 58P01.
 */
List *
parquet_expand(List *patterns)
{
	List	   *paths = NIL;

	foreach_ptr(char, pattern, patterns)
	{
		glob_t		found;
		int			result = glob(pattern, GLOB_ERR | GLOB_NOSORT, NULL, &found);

		if (result == GLOB_NOMATCH)
			ereport(ERROR,
					(errcode(ERRCODE_UNDEFINED_FILE),
					 errmsg("no Parquet file matches \"%s\"", pattern)));
		if (result != 0)
		{
			globfree(&found);
			ereport(ERROR,
					(errcode_for_file_access(),
					 errmsg("could not expand \"%s\": %m", pattern)));
		}
		/* The matches are the C library's: freed however the copy ends. */
		PG_TRY();
		{
			qsort(found.gl_pathv, found.gl_pathc, sizeof(char *), compare_paths);
			for (size_t i = 0; i < found.gl_pathc; i++)
				paths = lappend(paths, pstrdup(found.gl_pathv[i]));
		}
		PG_FINALLY();
		{
			globfree(&found);
		}
		PG_END_TRY();
	}
	return paths;
}

static void
free_file(void *arg)
{
	tess_parquet_file_free(((ParquetFileEntry *) arg)->file);
	ReleaseExternalFD();
}

static void
free_column(void *arg)
{
	tess_parquet_column_free((TessParquetColumn *) arg);
}

/*
 * The file at `path`, open, its footer read, owned by the current memory
 * context: its descriptor is counted against the backend's until the
 * context is reset or deleted. Where `expected` is not NULL, a file of
 * another identity is refused, 55000.
 */
ParquetFileEntry *
parquet_open_file(char *path, const TessParquetIdentity *expected)
{
	ParquetFileEntry *entry = palloc0_object(ParquetFileEntry);
	MemoryContextCallback *callback = palloc0_object(MemoryContextCallback);
	TessParquetStatus status;

	entry->path = path;
	/* A file a scan opens, in the order its participants open them. */
	if (expected != NULL)
		INJECTION_POINT("tessera-parquet-open", path);
	if (!AcquireExternalFD())
		ereport(ERROR,
				(errcode(ERRCODE_INSUFFICIENT_RESOURCES),
				 errmsg("too many open files to open Parquet file \"%s\"", path)));
	if (!tess_parquet_open(path, strlen(path), expected, &entry->file, &status))
	{
		ReleaseExternalFD();
		parquet_raise(&status);
	}
	callback->func = free_file;
	callback->arg = entry;
	MemoryContextRegisterResetCallback(CurrentMemoryContext, callback);
	if (!tess_parquet_file_info(entry->file, &entry->identity, &entry->groups,
								&entry->rows, &status))
		parquet_raise(&status);
	return entry;
}

/* `column` mapped to a column of the file of `entry`, owned by the
 * current memory context. */
TessParquetColumn *
parquet_map_column(ParquetFileEntry *entry, const ParquetColumn *column)
{
	TessParquetColumn *mapped = NULL;
	TessParquetStatus status;
	MemoryContextCallback *callback = palloc0_object(MemoryContextCallback);
	/* The file's names are UTF-8; the catalog's, the database's encoding. */
	char	   *name = pg_server_to_any(column->file_column,
										strlen(column->file_column), PG_UTF8);

	if (!tess_parquet_column(entry->file, name, strlen(name), column->type_oid,
							 column->typmod, column->unit, column->utf8,
							 GetDatabaseEncoding() == PG_UTF8, &mapped, &status))
		parquet_raise(&status);
	callback->func = free_column;
	callback->arg = mapped;
	MemoryContextRegisterResetCallback(CurrentMemoryContext, callback);
	return mapped;
}

/*
 * Opens the files at `paths` one by one, maps each column of `attnums` of
 * the table `relid` to each, and closes each before the next: the
 * refusals of a column come from here. Returns the rows of the files;
 * where `identities` is not NULL, writes what fstat said of each file
 * there, and where `groups` is not NULL, the groups of each file there
 * and an array of the rows of each group in `group_rows`, in the
 * current memory context; where `total_groups` is not NULL, the groups
 * of all the files.
 */
double
parquet_check_files(List *paths, Oid relid, List *attnums,
					TessParquetIdentity *identities, size_t *groups,
					uint64 **group_rows, uint64 *total_groups)
{
	MemoryContext outer = CurrentMemoryContext;
	MemoryContext one = AllocSetContextCreate(outer, "tessera_parquet file",
											  ALLOCSET_SMALL_SIZES);
	List	   *columns = NIL;
	double		rows = 0;

	foreach_int(attnum, attnums)
	{
		ParquetColumn *column = palloc_object(ParquetColumn);

		*column = parquet_column(relid, attnum);
		columns = lappend(columns, column);
	}
	foreach_ptr(char, path, paths)
	{
		ParquetFileEntry *entry;

		MemoryContextSwitchTo(one);
		entry = parquet_open_file(path, NULL);
		foreach_ptr(ParquetColumn, column, columns)
			(void) parquet_map_column(entry, column);
		rows += (double) entry->rows;
		if (total_groups != NULL)
			*total_groups += entry->groups;
		MemoryContextSwitchTo(outer);
		if (identities != NULL)
			identities[foreach_current_index(path)] = entry->identity;
		if (groups != NULL)
		{
			int			f = foreach_current_index(path);
			uint64	   *counts = palloc_array(uint64, Max(entry->groups, 1));

			for (size_t g = 0; g < entry->groups; g++)
			{
				TessParquetStatus status;

				if (!tess_parquet_group_rows(entry->file, g, &counts[g], &status))
					parquet_raise(&status);
			}
			groups[f] = entry->groups;
			group_rows[f] = counts;
		}
		/* The file closed, its descriptor given back, before the next. */
		MemoryContextReset(one);
	}
	MemoryContextDelete(one);
	return rows;
}

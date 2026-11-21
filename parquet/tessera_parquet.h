/*
 * The module tessera_parquet: a foreign data wrapper over Parquet files
 * read in place, through the reader of crates/tessera-parquet.
 *
 * This header is the module's own: its parts share the options of a table
 * and its columns, the files its patterns match, and the reader's objects,
 * each freed when the memory context that holds it is reset or deleted.
 */
#ifndef TESSERA_PARQUET_H
#define TESSERA_PARQUET_H

#include "postgres.h"

#include "nodes/pg_list.h"
#include "utils/palloc.h"

#include "tessera_parquet_reader.h"

/* A column of a foreign table, as its options map it to a file column. */
typedef struct ParquetColumn
{
	int			attnum;			/* the column's number in the table */
	char	   *file_column;	/* the option column, or the column's name */
	Oid			type_oid;
	int32		typmod;
	int32		unit;			/* TESS_PARQUET_UNIT_* */
	int32		utf8;			/* TESS_PARQUET_UTF8_* */
} ParquetColumn;

/* A file a table's patterns matched, open, its footer read. */
typedef struct ParquetFileEntry
{
	char	   *path;
	TessParquetFile *file;
	TessParquetIdentity identity;
	size_t		groups;
	uint64		rows;
} ParquetFileEntry;

/* options.c */
extern List *parquet_table_files(Oid relid);
extern ParquetColumn parquet_column(Oid relid, int attnum);

/* files.c */
extern List *parquet_expand(List *patterns);
extern ParquetFileEntry *parquet_open_file(char *path, const TessParquetIdentity *expected);
extern TessParquetColumn *parquet_map_column(ParquetFileEntry *entry,
											 const ParquetColumn *column);
extern double parquet_check_files(List *paths, Oid relid, List *attnums);
pg_noreturn extern void parquet_raise(const TessParquetStatus *status);

#endif							/* TESSERA_PARQUET_H */

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

/* The state of the wrapper's scan of a table's rows (scan.c). */
typedef struct ParquetScan
{
	Oid			relid;
	List	   *paths;			/* the files the patterns matched at begin */
	TessParquetIdentity *identities;	/* what fstat said of each then */
	size_t	   *groups;			/* of each file, its groups */
	uint64	  **group_rows;		/* of each file, the rows of each group */
	int			ncolumns;		/* the columns the query reads */
	ParquetColumn *columns;
	int			file_index;		/* the file read, -1 before the first */
	MemoryContext file_cxt;		/* the file open and its columns mapped */
	MemoryContext group_cxt;	/* the readers of the group read */
	MemoryContext window_cxt;	/* the values of the window read */
	ParquetFileEntry *file;
	TessParquetColumn **mapped;
	TessParquetColumnInfo *info;
	TessParquetReader **readers;
	size_t		group;			/* the group read */
	bool		group_started;
	uint64		window_start;	/* the window's first row in its group */
	size_t		window_len;
	size_t		row;			/* the next row of the window */
	uint64	  **values;			/* of each column, the window's values */
	uint64	  **nulls;			/* of each column, the window's NULL rows */
} ParquetScan;

/* options.c */
extern List *parquet_table_files(Oid relid);
extern ParquetColumn parquet_column(Oid relid, int attnum);

/* files.c */
extern List *parquet_expand(List *patterns);
extern ParquetFileEntry *parquet_open_file(char *path, const TessParquetIdentity *expected);
extern TessParquetColumn *parquet_map_column(ParquetFileEntry *entry,
											 const ParquetColumn *column);
extern double parquet_check_files(List *paths, Oid relid, List *attnums,
								  TessParquetIdentity *identities, size_t *groups,
								  uint64 **group_rows);

/* scan.c */
extern ParquetScan *parquet_scan_begin(Oid relid, List *patterns, List *attnums);
extern void parquet_scan_rescan(ParquetScan *scan);
extern void parquet_scan_end(ParquetScan *scan);
extern struct TupleTableSlot *parquet_scan_next(ParquetScan *scan,
												struct TupleTableSlot *slot);
pg_noreturn extern void parquet_raise(const TessParquetStatus *status);

#endif							/* TESSERA_PARQUET_H */

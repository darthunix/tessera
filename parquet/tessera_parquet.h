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

#include "foreign/fdwapi.h"
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

/* What planning finds of a table's files, beside their rows. */
typedef struct ParquetFound
{
	uint64		groups;			/* the groups of rows */
	double		file_bytes;		/* the bytes of the files */
	double		column_bytes;	/* the bytes of the chunks of the columns read */
} ParquetFound;

/* The share of a row's cost that the source's batches cost, of what the
 * wrapper's rows cost over the same columns. */
#define PARQUET_BATCH_SHARE 0.25

/* The counters of a scan that EXPLAIN sums over the participants. */
#define PARQUET_GROUPS_READ 0
#define PARQUET_VALUES_DECODED 1
#define PARQUET_COUNTERS 2

struct ParquetShared;
struct TessSharedStats;
struct ParallelContext;

/* The rows of a window of a group, read at once by a reader. */
#define PARQUET_WINDOW_ROWS 1024

/* The state of a scan of a table's files and groups (scan.c), the
 * wrapper's rows and the batch node's. */
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
	uint64	   *first_group;	/* of each file, its first group in the list */
	struct ParquetShared *shared;	/* a parallel scan's list, or NULL */
	uint64	   *selection;		/* the group's rows to read, or NULL for all */
	struct TessSharedStats *stats;	/* the participants' counters */
	/* This participant's, over every rescan, and in the leader the
	 * workers' of the scans before the last of a rescanned plan. */
	uint64		own[PARQUET_COUNTERS];
	bool		private_shared; /* the list in the leader's private memory */
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
								  uint64 **group_rows, ParquetFound *found);

/* fdw.c */
struct PlannerInfo;
struct RelOptInfo;
extern void parquet_get_foreign_paths(struct PlannerInfo *root, struct RelOptInfo *baserel,
									  Oid foreigntableid);
extern List *parquet_planned_list(void *fdw_private);
extern List *parquet_strings(List *nodes);
struct ExplainState;
extern void parquet_explain_files(List *planned, const ParquetScan *scan,
								  struct ExplainState *es);
struct Path;
extern void parquet_source_costs(struct PlannerInfo *root, const struct Path *offered,
								 List *exact, int checked, double *rows, Cost *startup,
								 Cost *total);

/* analyze.c */
extern bool parquet_analyze(Relation relation, AcquireSampleRowsFunc *func,
							BlockNumber *totalpages);

/* node.c */
extern void parquet_register(void);

/* scan.c */
extern ParquetScan *parquet_scan_begin(Oid relid, List *patterns, List *attnums);
extern void parquet_scan_rescan(ParquetScan *scan);
extern void parquet_scan_end(ParquetScan *scan);
extern bool parquet_scan_next_group(ParquetScan *scan);
extern uint64 parquet_scan_group_rows(const ParquetScan *scan);
extern TessParquetReader *parquet_scan_reader(ParquetScan *scan, int c);
extern TessParquetReader *parquet_scan_every_row_reader(ParquetScan *scan, int c,
														MemoryContext cxt);
extern void parquet_scan_select(ParquetScan *scan, uint64 *selection);
extern Size parquet_scan_estimate(const ParquetScan *scan, int nworkers);
extern void parquet_scan_share(ParquetScan *scan, void *coordinate,
							   struct ParallelContext *pcxt, MemoryContext cxt);
extern void parquet_scan_reshare(ParquetScan *scan, struct ParallelContext *pcxt,
								 MemoryContext cxt);
extern ParquetScan *parquet_scan_attach(Oid relid, List *attnums, void *coordinate,
										MemoryContext cxt);
extern void parquet_scan_unshare(ParquetScan *scan);
extern const uint64 *parquet_scan_counts(const ParquetScan *scan);
extern struct TupleTableSlot *parquet_scan_next(ParquetScan *scan,
												struct TupleTableSlot *slot);
pg_noreturn extern void parquet_raise(const TessParquetStatus *status);

#endif							/* TESSERA_PARQUET_H */

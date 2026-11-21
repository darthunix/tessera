/*
 * The foreign data wrapper tessera_parquet: its handler, its planning,
 * which expands the table's patterns, opens the files and maps the
 * columns a query reads, and the begin of its scan, which does so again,
 * so that a plan kept by a prepared statement reads the files as they are
 * when its scan begins.
 */
#include "postgres.h"

#include "access/htup_details.h"
#include "access/sysattr.h"
#include "access/table.h"
#include "executor/executor.h"
#include "foreign/fdwapi.h"
#include "foreign/foreign.h"
#include "nodes/makefuncs.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/planmain.h"
#include "optimizer/restrictinfo.h"
#include "utils/rel.h"

#include "tessera_parquet.h"

PG_MODULE_MAGIC_EXT(.name = "tessera_parquet", .version = "0.1");

PG_FUNCTION_INFO_V1(tessera_parquet_handler);

/* What planning found of a table, kept for its paths and its plan. */
typedef struct PlannedTable
{
	List	   *patterns;		/* the option files */
	List	   *attnums;		/* the columns the query reads */
	double		rows;			/* the rows of the files */
} PlannedTable;

/* The columns of `relid` a scan of `baserel` reads, as their numbers. */
static List *
columns_read(RelOptInfo *baserel, Oid relid)
{
	Bitmapset  *attrs = NULL;
	List	   *attnums = NIL;
	int			i = -1;

	pull_varattnos((Node *) baserel->reltarget->exprs, baserel->relid, &attrs);
	foreach_node(RestrictInfo, rinfo, baserel->baserestrictinfo)
		pull_varattnos((Node *) rinfo->clause, baserel->relid, &attrs);
	while ((i = bms_next_member(attrs, i)) >= 0)
	{
		AttrNumber	attnum = i + FirstLowInvalidHeapAttributeNumber;

		/* A whole-row reference reads every column not dropped. */
		if (attnum == 0)
		{
			Relation	rel = table_open(relid, NoLock);
			TupleDesc	desc = RelationGetDescr(rel);

			for (int a = 1; a <= desc->natts; a++)
			{
				if (!TupleDescAttr(desc, a - 1)->attisdropped)
					attnums = list_append_unique_int(attnums, a);
			}
			table_close(rel, NoLock);
		}
		else if (attnum > 0)
			attnums = list_append_unique_int(attnums, attnum);
	}
	return attnums;
}

static void
rel_size(PlannerInfo *root, RelOptInfo *baserel, Oid foreigntableid)
{
	PlannedTable *planned = palloc0_object(PlannedTable);

	planned->patterns = parquet_table_files(foreigntableid);
	planned->attnums = columns_read(baserel, foreigntableid);
	/* The table's files as they are now, each column the query reads
	 * mapped to each. */
	planned->rows = parquet_check_files(parquet_expand(planned->patterns), foreigntableid,
										planned->attnums);
	baserel->rows = clamp_row_est(planned->rows *
								  clauselist_selectivity(root, baserel->baserestrictinfo,
														 0, JOIN_INNER, NULL));
	baserel->fdw_private = planned;
}

static void
paths(PlannerInfo *root, RelOptInfo *baserel, Oid foreigntableid)
{
	PlannedTable *planned = baserel->fdw_private;
	Cost		total = seq_page_cost + planned->rows * cpu_tuple_cost;

	add_path(baserel, (Path *) create_foreignscan_path(root, baserel, NULL, baserel->rows, 0,
													   0, total, NIL, baserel->lateral_relids,
													   NULL, NIL, NIL));
}

static ForeignScan *
plan(PlannerInfo *root, RelOptInfo *baserel, Oid foreigntableid, ForeignPath *best_path,
	 List *tlist, List *scan_clauses, Plan *outer_plan)
{
	PlannedTable *planned = baserel->fdw_private;
	List	   *patterns = NIL;

	foreach_ptr(char, pattern, planned->patterns)
		patterns = lappend(patterns, makeString(pattern));
	return make_foreignscan(tlist, extract_actual_clauses(scan_clauses, false),
							baserel->relid, NIL,
							list_make2(patterns, planned->attnums), NIL, NIL, outer_plan);
}

static void
begin_scan(ForeignScanState *node, int eflags)
{
	ForeignScan *scan = (ForeignScan *) node->ss.ps.plan;
	List	   *patterns = NIL;

	if (eflags & EXEC_FLAG_EXPLAIN_ONLY)
		return;
	foreach_node(String, pattern, linitial(scan->fdw_private))
		patterns = lappend(patterns, strVal(pattern));
	/* The files as they are now, their columns checked again. */
	(void) parquet_check_files(parquet_expand(patterns),
							   RelationGetRelid(node->ss.ss_currentRelation),
							   lsecond(scan->fdw_private));
}

static TupleTableSlot *
iterate(ForeignScanState *node)
{
	ereport(ERROR,
			(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
			 errmsg("reading the rows of a Parquet table is not built yet")));
	pg_unreachable();
}

static void
rescan(ForeignScanState *node)
{
}

static void
end_scan(ForeignScanState *node)
{
}

Datum
tessera_parquet_handler(PG_FUNCTION_ARGS)
{
	FdwRoutine *routine = makeNode(FdwRoutine);

	routine->GetForeignRelSize = rel_size;
	routine->GetForeignPaths = paths;
	routine->GetForeignPlan = plan;
	routine->BeginForeignScan = begin_scan;
	routine->IterateForeignScan = iterate;
	routine->ReScanForeignScan = rescan;
	routine->EndForeignScan = end_scan;
	PG_RETURN_POINTER(routine);
}

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
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "commands/explain_state.h"
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
	List	   *paths;			/* the files the patterns matched */
	uint64		groups;			/* their groups of rows */
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

	/* The source of a bridge loaded after the module. */
	parquet_register();
	planned->patterns = parquet_table_files(foreigntableid);
	planned->attnums = columns_read(baserel, foreigntableid);
	/* The table's files as they are now, each column the query reads
	 * mapped to each. */
	planned->paths = parquet_expand(planned->patterns);
	planned->rows = parquet_check_files(planned->paths, foreigntableid, planned->attnums,
										NULL, NULL, NULL, &planned->groups);
	baserel->rows = clamp_row_est(planned->rows *
								  clauselist_selectivity(root, baserel->baserestrictinfo,
														 0, JOIN_INNER, NULL));
	baserel->fdw_private = planned;
}

/* The wrapper's path; the source knows its tables by this function. */
void
parquet_get_foreign_paths(PlannerInfo *root, RelOptInfo *baserel, Oid foreigntableid)
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
	return make_foreignscan(tlist, extract_actual_clauses(scan_clauses, false),
							baserel->relid, NIL, parquet_planned_list(baserel->fdw_private),
							NIL, NIL, outer_plan);
}

/*
 * What planning found, as a list a plan carries: the patterns and the
 * files' paths as strings, the columns read, and the groups as a string.
 */
List *
parquet_planned_list(void *fdw_private)
{
	PlannedTable *planned = fdw_private;
	List	   *patterns = NIL;
	List	   *paths = NIL;

	foreach_ptr(char, pattern, planned->patterns)
		patterns = lappend(patterns, makeString(pattern));
	foreach_ptr(char, path, planned->paths)
		paths = lappend(paths, makeString(path));
	return list_make4(patterns, planned->attnums, paths,
					  makeString(psprintf(UINT64_FORMAT, planned->groups)));
}

/* The strings of a list of String nodes. */
List *
parquet_strings(List *nodes)
{
	List	   *strings = NIL;

	foreach_node(String, node, nodes)
		strings = lappend(strings, strVal(node));
	return strings;
}

/*
 * The lines of EXPLAIN of both scans: the files and groups planned, with
 * VERBOSE the files' paths, and with ANALYZE the groups read.
 */
void
parquet_explain_files(List *planned, const ParquetScan *scan, ExplainState *es)
{
	ExplainPropertyInteger("Parquet Files", NULL, list_length(lthird(planned)), es);
	ExplainPropertyUInteger("Row Groups", NULL, strtou64(strVal(lfourth(planned)), NULL, 10), es);
	if (es->verbose)
		ExplainPropertyList("Parquet File Paths", parquet_strings(lthird(planned)), es);
	if (es->analyze && scan != NULL)
		ExplainPropertyUInteger("Row Groups Read", NULL, scan->groups_read, es);
}

static void
begin_scan(ForeignScanState *node, int eflags)
{
	ForeignScan *scan = (ForeignScan *) node->ss.ps.plan;

	if (eflags & EXEC_FLAG_EXPLAIN_ONLY)
		return;
	node->fdw_state = parquet_scan_begin(RelationGetRelid(node->ss.ss_currentRelation),
										 parquet_strings(linitial(scan->fdw_private)),
										 lsecond(scan->fdw_private));
}

static void
explain(ForeignScanState *node, ExplainState *es)
{
	parquet_explain_files(((ForeignScan *) node->ss.ps.plan)->fdw_private, node->fdw_state, es);
}

static TupleTableSlot *
iterate(ForeignScanState *node)
{
	return parquet_scan_next(node->fdw_state, node->ss.ss_ScanTupleSlot);
}

static void
rescan(ForeignScanState *node)
{
	if (node->fdw_state != NULL)
		parquet_scan_rescan(node->fdw_state);
}

static void
end_scan(ForeignScanState *node)
{
	if (node->fdw_state != NULL)
		parquet_scan_end(node->fdw_state);
}

Datum
tessera_parquet_handler(PG_FUNCTION_ARGS)
{
	FdwRoutine *routine = makeNode(FdwRoutine);

	routine->GetForeignRelSize = rel_size;
	routine->GetForeignPaths = parquet_get_foreign_paths;
	routine->GetForeignPlan = plan;
	routine->BeginForeignScan = begin_scan;
	routine->IterateForeignScan = iterate;
	routine->ReScanForeignScan = rescan;
	routine->EndForeignScan = end_scan;
	routine->ExplainForeignScan = explain;
	PG_RETURN_POINTER(routine);
}

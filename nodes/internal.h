/* Definitions shared within the Tessera nodes module. */
#ifndef TESSERA_NODES_INTERNAL_H
#define TESSERA_NODES_INTERNAL_H

#include "catalog/pg_opfamily_d.h"
#include "catalog/pg_type_d.h"

#include "tessera/node.h"
#include "tessera/planner.h"
#include "tessera/sort.h"

#define TESS_FILTER_NODE_NAME "tessera.filter"
#define TESS_AGG_NODE_NAME "tessera.agg"
#define TESS_HASH_JOIN_NODE_NAME "tessera.hash_join"
#define TESS_SORT_NODE_NAME "tessera.sort"
#define TESS_GATHER_NODE_NAME "tessera.gather"
#define TESS_SEND_NODE_NAME "tessera.send"
#define TESS_GATHER_MERGE_NODE_NAME "tessera.gather_merge"

/* The plan data of TessFilter and TessHashJoin, written by their planners. */
#define TESS_AGG_DATA "tessera.agg"
#define TESS_AGG_DATA_VERSION 1
#define TESS_FILTER_DATA "tessera.filter"
#define TESS_FILTER_DATA_VERSION 1
#define TESS_HASH_JOIN_DATA "tessera.hash_join"
#define TESS_HASH_JOIN_DATA_VERSION 6
#define TESS_SORT_DATA "tessera.sort"
#define TESS_SORT_DATA_VERSION 1
#define TESS_GATHER_DATA "tessera.gather"
#define TESS_GATHER_DATA_VERSION 1
#define TESS_SEND_DATA "tessera.send"
#define TESS_SEND_DATA_VERSION 1
#define TESS_APPEND_DATA "tessera.append"
#define TESS_APPEND_DATA_VERSION 1

/*
 * The table's kind of a key of this type: a value its word holds whole and
 * compares bit for bit, sign-extended to eight bytes as the kernels hash,
 * order and give it back, as PostgreSQL's Datum of the type is: int2,
 * int4, date and bool as INT4, int8, timestamp and timestamptz as INT8.
 * False for any other type: its values go by reference, or compare other
 * than bit for bit, or come back unlike the core's Datum (oid).
 */
static inline bool
tess_word_key_kind(Oid type, TessTableKeyKind *kind)
{
	switch (type)
	{
		case INT2OID:
		case INT4OID:
		case DATEOID:
		case BOOLOID:
			*kind = TESS_TABLE_KEY_INT4;
			return true;
		case INT8OID:
		case TIMESTAMPOID:
		case TIMESTAMPTZOID:
			*kind = TESS_TABLE_KEY_INT8;
			return true;
		default:
			return false;
	}
}

/* The B-tree family whose order the key's word keeps, for a sort key. */
static inline bool
tess_word_key_order(Oid type, Oid opfamily)
{
	switch (type)
	{
		case INT2OID:
		case INT4OID:
		case INT8OID:
			return opfamily == INTEGER_BTREE_FAM_OID;
		case DATEOID:
		case TIMESTAMPOID:
		case TIMESTAMPTZOID:
			/* datetime_ops */
			return opfamily == 434;
		case BOOLOID:
			return opfamily == BOOL_BTREE_FAM_OID;
		default:
			return false;
	}
}

/* Clauses (RestrictInfos) in the order the planner evaluates a plan's quals. */
extern List *tess_order_clauses(PlannerInfo *root, List *rinfos);
extern double tess_parallel_divisor(const Path *path);

/* tessera.join_bloom_ratio: see nodes/module.c. */
extern double tess_join_bloom_ratio;
/* tessera.batch_gather: see nodes/module.c. */
extern bool tess_batch_gather;

extern const TessNode tess_pack_node;
extern const CustomScanMethods tess_pack_scan_methods;
extern bool tess_pack_forwards(const Path *path);

extern const TessNode tess_heap_scan_node;
extern const CustomScanMethods tess_heap_scan_scan_methods;

extern const TessNode tess_filter_node;
extern const CustomScanMethods tess_filter_scan_methods;
extern Node *tess_filter_create_state(CustomScan *cscan);
extern void tess_filter_planner_init(void);
extern Path *tess_filter_row_path(PlannerInfo *root, RelOptInfo *rel, Path *seqscan);

extern const TessNode tess_agg_node;
extern const CustomScanMethods tess_agg_scan_methods;
extern void tess_agg_planner_init(void);

extern const TessNode tess_hash_join_node;
extern const CustomScanMethods tess_hash_join_scan_methods;
extern void tess_hash_join_planner_init(void);

extern const TessNode tess_sort_node;
extern const CustomScanMethods tess_sort_scan_methods;
extern void tess_sort_planner_init(void);
/* A path key the sort kernels order by: its target's place in target, kind and flags. */
extern bool tess_sort_key_of(PathKey *pathkey, PathTarget *target, Relids relids,
							 int *place, TessSortKey *key);

extern const TessNode tess_gather_node;
extern const CustomScanMethods tess_gather_scan_methods;
extern const TessNode tess_send_node;
extern const CustomScanMethods tess_send_scan_methods;
extern const TessNode tess_gather_merge_node;
extern const CustomScanMethods tess_gather_merge_scan_methods;
extern void tess_gather_planner_init(void);
extern void tess_gather_add_paths(PlannerInfo *root, RelOptInfo *rel);
extern Path *tess_gather_merge_path(PlannerInfo *root, RelOptInfo *rel, Path *sorted,
									PathTarget *target);

extern const TessNode tess_append_node;
extern const CustomScanMethods tess_append_scan_methods;

#endif							/* TESSERA_NODES_INTERNAL_H */

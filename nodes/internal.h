/* Definitions shared within the Tessera nodes module. */
#ifndef TESSERA_NODES_INTERNAL_H
#define TESSERA_NODES_INTERNAL_H

#include "tessera/node.h"
#include "tessera/planner.h"

#define TESS_FILTER_NODE_NAME "tessera.filter"
#define TESS_AGG_NODE_NAME "tessera.agg"
#define TESS_HASH_JOIN_NODE_NAME "tessera.hash_join"

/* The plan data of TessFilter and TessHashJoin, written by their planners. */
#define TESS_AGG_DATA "tessera.agg"
#define TESS_AGG_DATA_VERSION 1
#define TESS_FILTER_DATA "tessera.filter"
#define TESS_FILTER_DATA_VERSION 1
#define TESS_HASH_JOIN_DATA "tessera.hash_join"
#define TESS_HASH_JOIN_DATA_VERSION 6

/* Clauses (RestrictInfos) in the order the planner evaluates a plan's quals. */
extern List *tess_order_clauses(PlannerInfo *root, List *rinfos);

extern const TessNode tess_pack_node;
extern const CustomScanMethods tess_pack_scan_methods;

extern const TessNode tess_heap_scan_node;
extern const CustomScanMethods tess_heap_scan_scan_methods;

extern const TessNode tess_filter_node;
extern const CustomScanMethods tess_filter_scan_methods;
extern Node *tess_filter_create_state(CustomScan *cscan);
extern void tess_filter_planner_init(void);

extern const TessNode tess_agg_node;
extern const CustomScanMethods tess_agg_scan_methods;
extern void tess_agg_planner_init(void);

extern const TessNode tess_hash_join_node;
extern const CustomScanMethods tess_hash_join_scan_methods;
extern void tess_hash_join_planner_init(void);

#endif							/* TESSERA_NODES_INTERNAL_H */

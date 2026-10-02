/*
 * The nodes of this module: their names, the kinds and versions of their
 * plan data, their methods and their planners' entry points.
 */
#ifndef TESSERA_NODES_REGISTRY_H
#define TESSERA_NODES_REGISTRY_H

#include "nodes/extensible.h"
#include "nodes/pathnodes.h"

#include "tessera/node.h"

#define TESS_FILTER_NODE_NAME "tessera.filter"
#define TESS_AGG_NODE_NAME "tessera.agg"
#define TESS_HASH_JOIN_NODE_NAME "tessera.hash_join"
#define TESS_SORT_NODE_NAME "tessera.sort"
#define TESS_GATHER_NODE_NAME "tessera.gather"
#define TESS_SEND_NODE_NAME "tessera.send"
#define TESS_GATHER_MERGE_NODE_NAME "tessera.gather_merge"

/* The plan data of the nodes of this module, written by their planners. */
#define TESS_AGG_DATA "tessera.agg"
#define TESS_AGG_DATA_VERSION 8
#define TESS_FILTER_DATA "tessera.filter"
#define TESS_FILTER_DATA_VERSION 1
#define TESS_HASH_JOIN_DATA "tessera.hash_join"
#define TESS_HASH_JOIN_DATA_VERSION 8
#define TESS_SORT_DATA "tessera.sort"
#define TESS_SORT_DATA_VERSION 2
#define TESS_GATHER_DATA "tessera.gather"
#define TESS_GATHER_DATA_VERSION 2
#define TESS_SEND_DATA "tessera.send"
#define TESS_SEND_DATA_VERSION 2
#define TESS_APPEND_DATA "tessera.append"
#define TESS_APPEND_DATA_VERSION 2

extern const TessNode tess_pack_node;
extern const CustomScanMethods tess_pack_scan_methods;
extern bool tess_pack_forwards(const Path *path);

extern const TessNode tess_heap_scan_node;
extern const CustomScanMethods tess_heap_scan_scan_methods;
/* The node's scan of a bitmap's pages in place of the core's bitmap heap scan. */
extern Path *tess_heap_bitmap_path(PlannerInfo *root, BitmapHeapPath *bitmap,
								   PathTarget *target);
/* The node over the core's index scan, in the index's order. */
extern Path *tess_heap_index_path(PlannerInfo *root, IndexPath *index, PathTarget *target);

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

extern const TessNode tess_gather_node;
extern const CustomScanMethods tess_gather_scan_methods;
extern const TessNode tess_send_node;
extern const CustomScanMethods tess_send_scan_methods;
extern const TessNode tess_gather_merge_node;
extern const CustomScanMethods tess_gather_merge_scan_methods;
extern void tess_gather_planner_init(void);
extern void tess_gather_add_paths(PlannerInfo *root, RelOptInfo *rel);
extern Path *tess_gather_path(PlannerInfo *root, RelOptInfo *rel, Path *subpath);
extern Path *tess_gather_merge_path(PlannerInfo *root, RelOptInfo *rel, Path *sorted,
									PathTarget *target);

extern const TessNode tess_append_node;
extern const CustomScanMethods tess_append_scan_methods;

#endif							/* TESSERA_NODES_REGISTRY_H */

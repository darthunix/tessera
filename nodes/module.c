#include "postgres.h"

#include "fmgr.h"
#include "utils/guc.h"

#include "tessera/runtime.h"

#include "internal.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

double		tess_join_bloom_ratio = 0.5;
double		tess_bitmap_page_rows = 2.0;
bool		tess_batch_gather = true;

/*
 * The module registers its node kinds and scan methods. The pack and heap
 * scan nodes are created by batch parents and need no hook; the filter
 * node offers its path to base relations through the set_rel_pathlist
 * hook, the aggregate node to the grouping stage through the
 * create_upper_paths hook, the hash join node to joins through the
 * set_join_pathlist hook, the sort node to the ordered stage through the
 * create_upper_paths hook. The append node stands in for an Append under
 * a batch parent, through tess_batch_input_path, and needs no hook either.
 */
void
_PG_init(void)
{
	/* Raises ERROR without the bridge: CREATE EXTENSION tessera first. */
	const TessApi *api = tess_runtime_api();

	RegisterCustomScanMethods(&tess_pack_scan_methods);
	api->nodes->add(&tess_pack_node);
	RegisterCustomScanMethods(&tess_heap_scan_scan_methods);
	api->nodes->add(&tess_heap_scan_node);
	RegisterCustomScanMethods(&tess_filter_scan_methods);
	api->nodes->add(&tess_filter_node);
	tess_filter_planner_init();
	RegisterCustomScanMethods(&tess_agg_scan_methods);
	api->nodes->add(&tess_agg_node);
	tess_agg_planner_init();
	RegisterCustomScanMethods(&tess_hash_join_scan_methods);
	api->nodes->add(&tess_hash_join_node);
	tess_hash_join_planner_init();
	RegisterCustomScanMethods(&tess_sort_scan_methods);
	api->nodes->add(&tess_sort_node);
	tess_sort_planner_init();
	RegisterCustomScanMethods(&tess_gather_scan_methods);
	api->nodes->add(&tess_gather_node);
	RegisterCustomScanMethods(&tess_send_scan_methods);
	api->nodes->add(&tess_send_node);
	RegisterCustomScanMethods(&tess_gather_merge_scan_methods);
	api->nodes->add(&tess_gather_merge_node);
	tess_gather_planner_init();
	RegisterCustomScanMethods(&tess_append_scan_methods);
	api->nodes->add(&tess_append_node);
	DefineCustomRealVariable("tessera.join_bloom_ratio",
							 "Share of probe rows with a pair below which a hash join builds a Bloom filter.",
							 "After its first probe rows a join builds a Bloom filter of its keys when "
							 "fewer of them than this share found a pair: 0 never builds one, 1 builds "
							 "one at once, whatever the sizes.",
							 &tess_join_bloom_ratio, 0.5, 0.0, 1.0,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.bitmap_page_rows",
							 "Rows a page of a bitmap must give for the batch scan of its pages.",
							 "The node reads a bitmap's pages in place of the core's bitmap heap scan "
							 "when the planner expects at least this many rows of each page: below, a "
							 "page's pin of the node's own costs more than its batches save. 0 always.",
							 &tess_bitmap_page_rows, 2.0, 0.0, 1000.0,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomBoolVariable("tessera.batch_gather",
							 "Gathers a parallel batch subtree's rows in batches.",
							 "TessGather stands in for the core's Gather over a batch path: the workers "
							 "send batches of rows instead of a tuple each.",
							 &tess_batch_gather, true, PGC_USERSET, 0, NULL, NULL, NULL);
}

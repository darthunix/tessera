#include "postgres.h"

#include "fmgr.h"
#include "utils/guc.h"

#include "tessera/runtime.h"

#include "internal.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

double		tess_join_bloom_ratio = 0.5;

/*
 * The module registers its node kinds and scan methods. The pack and heap
 * scan nodes are created by batch parents and need no hook; the filter
 * node offers its path to base relations through the set_rel_pathlist
 * hook, the aggregate node to the grouping stage through the
 * create_upper_paths hook, the hash join node to joins through the
 * set_join_pathlist hook.
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
	DefineCustomRealVariable("tessera.join_bloom_ratio",
							 "Share of probe rows with a pair below which a hash join builds a Bloom filter.",
							 "After its first probe rows a join builds a Bloom filter of its keys when "
							 "fewer of them than this share found a pair: 0 never builds one, 1 builds "
							 "one at once, whatever the sizes.",
							 &tess_join_bloom_ratio, 0.5, 0.0, 1.0,
							 PGC_USERSET, 0, NULL, NULL, NULL);
}

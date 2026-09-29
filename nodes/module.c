#include "postgres.h"

#include "fmgr.h"
#include "utils/guc.h"

#include "tessera/runtime.h"

#include "internal.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

double		tess_scan_cost_factor = 0.9;
double		tess_join_cost_factor = 0.9;
double		tess_agg_cost_factor = 0.9;
double		tess_agg_key_share = 0.25;
double		tess_agg_kernel_share = 0.25;
double		tess_setop_word_share = 0.5;
double		tess_setop_dictionary_share = 0.9;
double		tess_gather_tuple_share = 0.25;
double		tess_scan_page_cost = 1.0;
double		tess_scan_tuple_cost = 0.0077;
double		tess_index_only_tuple_cost = 0.058;
double		tess_index_tuple_cost = 0.112;
double		tess_bitmap_page_cost = 0.665;
double		tess_bitmap_tuple_cost = 0.075;
double		tess_bitmap_scatter_cost = 0.031;
double		tess_scan_parallel_setup_cost = 7000.0;
double		tess_scan_worker_page_cost = 3.15;
double		tess_filter_clause_cost = 0.0053;
double		tess_filter_row_clause_cost = 0.022;
double		tess_filter_row_operator_cost = 0.018;
double		tess_deform_varlena_cost = 0.017;
double		tess_join_bloom_ratio = 0.5;
double		tess_bitmap_page_rows = 2.0;
double		tess_index_min_correlation = 0.8;
double		tess_index_min_rows = 1000.0;
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
	DefineCustomRealVariable("tessera.index_min_correlation",
							 "Correlation of an index's order with the table's for the batch scan of its rows.",
							 "The node takes the rows of the core's index scan in place of it when the "
							 "index's order follows the table's at least this much: its rows then come "
							 "in runs of a page, which a batch pins once. 0 always.",
							 &tess_index_min_correlation, 0.8, 0.0, 1.0,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.index_min_rows",
							 "Rows an index scan must give for the batch scan of its rows.",
							 "The node takes the rows of the core's index scan in place of it when the "
							 "planner expects at least this many of them, a limit counted: its setup "
							 "costs a query a few microseconds more. 0 always.",
							 &tess_index_min_rows, 1000.0, 0.0, 1e15,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	/*
	 * The planner's calibration of the nodes' costs against the core's,
	 * each measured where the node's code says; not in the sample file.
	 */
	DefineCustomRealVariable("tessera.scan_cost_factor",
							 "Share of the core's cost of a scan that the node's scan costs.",
							 "TessHeapScan, with TessFilter above for the relation's clauses, in "
							 "place of the core's sequential, bitmap, index or index-only scan.",
							 &tess_scan_cost_factor, 0.9, 0.0, 10.0,
							 PGC_USERSET, GUC_NOT_IN_SAMPLE, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.join_cost_factor",
							 "Share of the core's cost of a hash join that TessHashJoin costs.",
							 NULL,
							 &tess_join_cost_factor, 0.9, 0.0, 10.0,
							 PGC_USERSET, GUC_NOT_IN_SAMPLE, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.agg_cost_factor",
							 "Share of the core's cost of an aggregation without GROUP BY that TessAgg costs.",
							 NULL,
							 &tess_agg_cost_factor, 0.9, 0.0, 10.0,
							 PGC_USERSET, GUC_NOT_IN_SAMPLE, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.agg_key_share",
							 "Share of cpu_operator_cost TessAgg costs a key of a row it groups.",
							 NULL,
							 &tess_agg_key_share, 0.25, 0.0, 10.0,
							 PGC_USERSET, GUC_NOT_IN_SAMPLE, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.agg_kernel_share",
							 "Share of the core's transition cost a row of TessAgg's own aggregates costs.",
							 NULL,
							 &tess_agg_kernel_share, 0.25, 0.0, 10.0,
							 PGC_USERSET, GUC_NOT_IN_SAMPLE, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.setop_word_share",
							 "Share of the core's own cost of INTERSECT or EXCEPT TessAgg costs with keys of words.",
							 NULL,
							 &tess_setop_word_share, 0.5, 0.0, 10.0,
							 PGC_USERSET, GUC_NOT_IN_SAMPLE, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.setop_dictionary_share",
							 "Share of the core's own cost of INTERSECT or EXCEPT TessAgg costs with a key through a dictionary.",
							 NULL,
							 &tess_setop_dictionary_share, 0.9, 0.0, 10.0,
							 PGC_USERSET, GUC_NOT_IN_SAMPLE, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.gather_tuple_share",
							 "Share of parallel_tuple_cost a row costs through TessGather.",
							 NULL,
							 &tess_gather_tuple_share, 0.25, 0.0, 10.0,
							 PGC_USERSET, GUC_NOT_IN_SAMPLE, NULL, NULL, NULL);
	/*
	 * The model of the node's scans, by which the planner orders them
	 * against one another (bench/pg/scancost fits it): a full scan's page is
	 * the unit.
	 */
	DefineCustomRealVariable("tessera.scan_page_cost",
							 "The node's time to read a page of a full scan, the model's unit.",
							 NULL,
							 &tess_scan_page_cost, 1.0, 0.0, 1e10,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.scan_tuple_cost",
							 "The node's time for a row of a full scan with its filter.",
							 NULL,
							 &tess_scan_tuple_cost, 0.0077, 0.0, 1e10,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.index_only_tuple_cost",
							 "The node's time for a row of an index-only scan.",
							 NULL,
							 &tess_index_only_tuple_cost, 0.058, 0.0, 1e10,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.index_tuple_cost",
							 "The node's time for a row of an index scan.",
							 NULL,
							 &tess_index_tuple_cost, 0.112, 0.0, 1e10,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.bitmap_page_cost",
							 "The node's time to read a page of a bitmap.",
							 NULL,
							 &tess_bitmap_page_cost, 0.665, 0.0, 1e10,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.bitmap_tuple_cost",
							 "The node's time for a row of a bitmap.",
							 NULL,
							 &tess_bitmap_tuple_cost, 0.075, 0.0, 1e10,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.bitmap_scatter_cost",
							 "The node's time a row of a bitmap takes more for an index out of the table's order.",
							 "Scaled by 1 - c * c for the correlation c of the index's first column.",
							 &tess_bitmap_scatter_cost, 0.031, 0.0, 1e10,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.scan_parallel_setup_cost",
							 "The time a parallel scan of the node takes to start and finish its workers.",
							 "Part of a partial scan's time, which the planner weighs against the "
							 "relation's fastest serial scan.",
							 &tess_scan_parallel_setup_cost, 7000.0, 0.0, 1e10,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.scan_worker_page_cost",
							 "The time a parallel worker takes more for a page it reads first.",
							 "A worker begun for the query maps every page of the shared buffers it "
							 "reads; near zero with huge pages.",
							 &tess_scan_worker_page_cost, 3.15, 0.0, 1e10,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.filter_clause_cost",
							 "The node's time for a row of a batch clause past the filter's first.",
							 NULL,
							 &tess_filter_clause_cost, 0.0053, 0.0, 1e10,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.filter_row_clause_cost",
							 "The node's time for a row of a clause the filter evaluates row by row.",
							 NULL,
							 &tess_filter_row_clause_cost, 0.022, 0.0, 1e10,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.filter_row_operator_cost",
							 "The node's time for a row of an operator of a clause evaluated row by row.",
							 "Counted as the core's cost of the clause over cpu_operator_cost.",
							 &tess_filter_row_operator_cost, 0.018, 0.0, 1e10,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomRealVariable("tessera.deform_varlena_cost",
							 "The node's time for a row to deform a column past one of varying length.",
							 NULL,
							 &tess_deform_varlena_cost, 0.017, 0.0, 1e10,
							 PGC_USERSET, 0, NULL, NULL, NULL);
	DefineCustomBoolVariable("tessera.batch_gather",
							 "Gathers a parallel batch subtree's rows in batches.",
							 "TessGather stands in for the core's Gather over a batch path: the workers "
							 "send batches of rows instead of a tuple each.",
							 &tess_batch_gather, true, PGC_USERSET, 0, NULL, NULL, NULL);
}

/* The planner's cost model: its settings, in nodes/module.c, and helpers. */
#ifndef TESSERA_NODES_COSTS_H
#define TESSERA_NODES_COSTS_H

#include "nodes/pathnodes.h"

/*
 * The planner's cost calibration, tessera.scan_cost_factor and the rest:
 * see nodes/module.c, and each one's measurement where it is used.
 */
extern double tess_scan_cost_factor;
extern double tess_agg_key_share;
extern double tess_agg_dictionary_share;
extern double tess_agg_kernel_share;
extern double tess_agg_generic_share;
extern double tess_setop_word_share;
extern double tess_setop_dictionary_share;
extern double tess_gather_tuple_share;
extern double tess_pack_value_share;
/*
 * The model of the node's scans, tessera.scan_page_cost and the rest: see
 * nodes/module.c and nodes/scan_planner.c.
 */
extern double tess_scan_page_cost;
extern double tess_scan_tuple_cost;
extern double tess_index_only_tuple_cost;
extern double tess_index_tuple_cost;
extern double tess_bitmap_page_cost;
extern double tess_bitmap_tuple_cost;
extern double tess_bitmap_scatter_cost;
extern double tess_scan_parallel_setup_cost;
extern double tess_scan_worker_page_cost;
extern double tess_filter_clause_cost;
extern double tess_filter_row_clause_cost;
extern double tess_filter_row_operator_cost;
extern double tess_deform_varlena_cost;
extern double tess_bitmap_build_cost;
extern double tess_bitmap_build_scatter_cost;
extern double tess_index_worker_share;
/*
 * The model of the node's hash join, tessera.join_build_cost and the rest,
 * in the scan model's units: see join_planner.c, join_cost.
 */
extern double tess_join_build_cost;
extern double tess_join_probe_cost;
extern double tess_join_pair_cost;
extern double tess_join_batch_cost;
extern double tess_join_gather_cost;
extern double tess_join_text_value_cost;
extern double tess_join_hashed_key_cost;
extern double tess_join_compact_pair_cost;
extern double tess_join_bloom_test_cost;
extern double tess_join_spill_row_cost;
extern double tess_join_cost_unit;
/* The correlation of an index's order with the table's: see heapscan.c. */
extern double tess_index_correlation(PlannerInfo *root, IndexOptInfo *index);
/* Whether every index of a bitmap is BRIN, whose bitmap names whole pages: see heapscan.c. */
extern bool tess_bitmap_only_brin(Path *bitmapqual);
/* tessera.join_bloom_ratio: see nodes/module.c. */
extern double tess_join_bloom_ratio;
/*
 * The rows a hash join's table must have for a Bloom filter: a smaller
 * table stays in the cache, where a miss costs less than the check. The
 * executor's rule, and the planner's expectation of the filter.
 */
#define JOIN_BLOOM_MIN_ROWS 4096

/*
 * Whether a hash join's table gets a Bloom filter of its keys: always at
 * a tessera.join_bloom_ratio of 1, never at 0, and between them for a
 * table of JOIN_BLOOM_MIN_ROWS rows at least when fewer than ratio of the
 * probed rows find a record. The executor asks it with the rows of its
 * sample, the planner with its estimate of the share found and one row.
 * join_bloom_possible says whether a table of table_rows rows may get
 * one at all, whatever its probes find: a shared table's filter is
 * allocated only then.
 */
static inline bool
join_bloom_possible(double ratio, double table_rows)
{
	return ratio >= 1.0 || (ratio > 0.0 && table_rows >= JOIN_BLOOM_MIN_ROWS);
}

static inline bool
join_bloom_wanted(double ratio, double table_rows, double found, double probed)
{
	if (!join_bloom_possible(ratio, table_rows))
		return false;
	return ratio >= 1.0 || found < ratio * probed;
}
/* tessera.bitmap_page_rows: see nodes/module.c. */
extern double tess_bitmap_page_rows;
/* tessera.index_min_correlation and tessera.index_min_rows: see nodes/module.c. */
extern double tess_index_min_correlation;
extern double tess_index_min_rows;
/* tessera.batch_gather: see nodes/module.c. */
extern bool tess_batch_gather;

#endif							/* TESSERA_NODES_COSTS_H */

/*
 * The operations of TessKernelOps (tessera/kernel_ops.h), one list for
 * every place that names them all: X(name) for the field `name`, which
 * holds the entry point tess_<name> of the kernels library. The order is
 * the fields' and part of the ABI: a new operation goes last, and
 * TESS_KERNEL_OPS_MIN_SIZE moves to it.
 *
 * The struct's fields, the kernels module's table, the bridge's check
 * that every operation is there and the test module's copy of the table
 * expand it; the prototypes and their contracts stay in the headers of
 * the entry points (tessera/kernels.h, decimal.h, sort.h, spill.h and
 * table.h).
 */
#ifndef TESSERA_KERNEL_OPS_LIST_H
#define TESSERA_KERNEL_OPS_LIST_H

#define TESS_KERNEL_OPS(X) \
	X(int4_hash) \
	X(int4_hash_next) \
	X(int8_hash) \
	X(int8_hash_next) \
	X(table_size) \
	X(table_create) \
	X(table_stats) \
	X(table_chunk_init) \
	X(table_append) \
	X(table_link) \
	X(table_link_grouped) \
	X(table_probe) \
	X(table_next_match) \
	X(table_gather) \
	X(table_record) \
	X(table_find_or_insert) \
	X(table_scan) \
	X(table_regrow) \
	X(table_next_in_group) \
	X(table_accumulate) \
	X(table_gather_key) \
	X(table_bloom_words) \
	X(table_bloom) \
	X(bloom_probe) \
	X(bloom_shared_words) \
	X(bloom_shared_init) \
	X(table_try_build_bloom) \
	X(bloom_shared_ready) \
	X(bloom_shared_probe) \
	X(build_counters_init) \
	X(build_report) \
	X(build_take_chunk) \
	X(build_add_duplicates) \
	X(build_totals) \
	X(build_step) \
	X(table_fingerprint) \
	X(spill_header_write) \
	X(spill_header_read) \
	X(table_split) \
	X(bloom_add) \
	X(table_find_or_insert_partitioned) \
	X(table_combine) \
	X(table_spill_words) \
	X(table_spill_init) \
	X(table_spill_split) \
	X(table_spill_partitions) \
	X(table_spill_add_bytes) \
	X(table_spill_evict) \
	X(table_spill_flags) \
	X(table_spill_records) \
	X(table_spill_start) \
	X(table_spill_take_file) \
	X(table_spill_take_alone) \
	X(round_step) \
	X(bloom_shared_add) \
	X(table_spill_evictions) \
	X(spill_pack) \
	X(spill_unpack) \
	X(table_payloads) \
	X(sort_item_words) \
	X(sort_items) \
	X(sort) \
	X(table_append_columns) \
	X(table_gather_scattered) \
	X(sort_top_candidates) \
	X(sort_top_push) \
	X(table_append_partitioned_columns) \
	X(spill_columns_init) \
	X(spill_columns_append_partitioned) \
	X(spill_columns_pack) \
	X(spill_columns_unpack) \
	X(sort_merge) \
	X(sort_key_lanes) \
	X(decimal_read) \
	X(decimal_write) \
	X(decimal_read_datum) \
	X(decimal_write_datum) \
	X(decimal_sum) \
	X(int4_in_set) \
	X(int8_in_set) \
	X(table_accumulate_sums) \
	X(table_clear_key) \
	X(table_gather_words) \
	X(table_record_size)

#endif							/* TESSERA_KERNEL_OPS_LIST_H */

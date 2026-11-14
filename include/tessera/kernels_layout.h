/*
 * Whether the linked Rust kernels lay out the shared structures as these
 * headers do.
 *
 * The C side and the kernels library agree on the sizes and offsets of the
 * structures they pass each other (a row mask, a Datum column, a status, a
 * table key, a sort key, a chunk header) and on a few numbers the C side
 * computes with (the bits of a record reference, the bytes of a chunk's
 * used mark, the offsets of a record's fields, the bytes of a spilled
 * block's header). The library reports them through its layout probes;
 * this check compares every one with the headers. The module that links
 * the library runs it when it loads, so a library built from another
 * source tree is refused instead of corrupting memory, and the tests of
 * the probes run the same check.
 */
#ifndef TESSERA_KERNELS_LAYOUT_H
#define TESSERA_KERNELS_LAYOUT_H

#include "postgres.h"

#include "tessera/kernels.h"
#include "tessera/sort.h"
#include "tessera/spill.h"
#include "tessera/table.h"

/*
 * The table region's header, which only the kernels read: 96 bytes with
 * the format version at offset 8 (crates/tessera-kernels, table/header.rs).
 */
#define TESS_TABLE_REGION_HEADER_SIZE 96
#define TESS_TABLE_REGION_VERSION_OFFSET 8

/*
 * True when every probe of the linked library matches these headers,
 * including 0 for a kind the library does not know.
 */
static inline bool
tess_kernels_layout_matches(void)
{
	return tess_kernels_abi_version() == TESS_KERNELS_ABI_VERSION &&
		tess_kernels_probe(TESS_PROBE_ROW_MASK_SIZE) == sizeof(TessRowMask) &&
		tess_kernels_probe(TESS_PROBE_DATUM_COLUMN_SIZE) ==
		sizeof(TessDatumColumn) &&
		tess_kernels_probe(TESS_PROBE_DATUM_COLUMN_NROWS_OFFSET) ==
		offsetof(TessDatumColumn, nrows) &&
		tess_kernels_probe(TESS_PROBE_STATUS_SIZE) == sizeof(TessStatus) &&
		tess_kernels_probe(TESS_PROBE_STATUS_MESSAGE_OFFSET) ==
		offsetof(TessStatus, message) &&
		tess_kernels_probe((TessKernelsProbe) 99) == 0 &&
		tess_table_format_version() == TESS_TABLE_FORMAT_VERSION &&
		tess_table_layout(TESS_TABLE_LAYOUT_HEADER_SIZE) ==
		TESS_TABLE_REGION_HEADER_SIZE &&
		tess_table_layout(TESS_TABLE_LAYOUT_VERSION_OFFSET) ==
		TESS_TABLE_REGION_VERSION_OFFSET &&
		tess_table_layout(TESS_TABLE_LAYOUT_KEY_SIZE) == sizeof(TessTableKey) &&
		tess_table_layout(TESS_TABLE_LAYOUT_KEY_PREPARED_OFFSET) ==
		offsetof(TessTableKey, prepared) &&
		tess_table_layout(TESS_TABLE_LAYOUT_STATS_SIZE) ==
		sizeof(TessTableStats) &&
		tess_table_layout(TESS_TABLE_LAYOUT_STATS_REGION_LEN_OFFSET) ==
		offsetof(TessTableStats, region_len) &&
		tess_table_layout(TESS_TABLE_LAYOUT_RECORD_SIZE) ==
		sizeof(TessTableRecord) &&
		tess_table_layout(TESS_TABLE_LAYOUT_RECORD_PAYLOAD_OFFSET) ==
		offsetof(TessTableRecord, payload) &&
		tess_table_layout(TESS_TABLE_LAYOUT_REF_SIZE) == sizeof(TessTableRef) &&
		tess_table_layout(TESS_TABLE_LAYOUT_REF_NCHUNKS_OFFSET) ==
		offsetof(TessTableRef, nchunks) &&
		tess_table_layout(TESS_TABLE_LAYOUT_UNIT_BITS) == TESS_TABLE_UNIT_BITS &&
		tess_table_layout(TESS_TABLE_LAYOUT_SPILL_WEIGHTS_SIZE) ==
		sizeof(TessSpillWeights) &&
		tess_table_layout(TESS_TABLE_LAYOUT_SPILL_WEIGHTS_PER_CHECK_OFFSET) ==
		offsetof(TessSpillWeights, per_check) &&
		tess_table_layout(TESS_TABLE_LAYOUT_CHUNK_HEADER) ==
		TESS_TABLE_CHUNK_HEADER &&
		tess_table_layout(TESS_TABLE_LAYOUT_RECORD_HASH_OFFSET) ==
		TESS_TABLE_RECORD_HASH_OFFSET &&
		tess_table_layout(TESS_TABLE_LAYOUT_RECORD_NEXT_OFFSET) ==
		TESS_TABLE_RECORD_NEXT_OFFSET &&
		tess_table_layout(TESS_TABLE_LAYOUT_RECORD_NULL_BITS_OFFSET) ==
		TESS_TABLE_RECORD_NULL_BITS_OFFSET &&
		tess_table_layout(TESS_TABLE_LAYOUT_RECORD_SIZE_OFFSET) ==
		TESS_TABLE_RECORD_SIZE_OFFSET &&
		tess_table_layout(TESS_TABLE_LAYOUT_RECORD_HEADER) ==
		TESS_TABLE_RECORD_HEADER &&
		tess_table_layout(TESS_TABLE_LAYOUT_KEY_SLOT) == TESS_TABLE_KEY_SLOT &&
		tess_table_layout((TessTableLayoutKind) 99) == 0 &&
		tess_sort_layout(TESS_SORT_LAYOUT_KEY_SIZE) == sizeof(TessSortKey) &&
		tess_sort_layout(TESS_SORT_LAYOUT_KEY_FLAGS_OFFSET) ==
		offsetof(TessSortKey, flags) &&
		tess_sort_layout((TessSortLayoutKind) 99) == 0 &&
		tess_spill_header_size() == TESS_SPILL_HEADER_SIZE &&
		tess_spill_set_layout(TESS_SPILL_SET_WRITER_SIZE) ==
		sizeof(TessSpillWriter) &&
		tess_spill_set_layout(TESS_SPILL_SET_ENTRY_SIZE) ==
		sizeof(TessSpillEntry) &&
		tess_spill_set_layout(TESS_SPILL_SET_LIST_SIZE) == sizeof(TessSpillList) &&
		tess_spill_set_layout(TESS_SPILL_SET_LIST_COUNT_OFFSET) ==
		offsetof(TessSpillList, count) &&
		tess_spill_set_layout(TESS_SPILL_SET_RANGE_SIZE) ==
		sizeof(TessSpillRange) &&
		tess_spill_set_layout(TESS_SPILL_SET_STEP_SIZE) == sizeof(TessSpillStep) &&
		tess_spill_set_layout(TESS_SPILL_SET_STEP_RANGES_OFFSET) ==
		offsetof(TessSpillStep, ranges) &&
		tess_spill_set_layout(TESS_SPILL_SET_READER_SIZE) ==
		sizeof(TessSpillReadState) &&
		tess_spill_set_layout(TESS_SPILL_SET_READ_STEP_SIZE) ==
		sizeof(TessSpillReadStep) &&
		tess_spill_set_layout(TESS_SPILL_SET_READ_STEP_HEADER_OFFSET) ==
		offsetof(TessSpillReadStep, header) &&
		tess_spill_set_layout(TESS_SPILL_SET_SHAPE_SIZE) == sizeof(TessSpillShape) &&
		tess_spill_set_layout(TESS_SPILL_SET_SHAPE_MAX_PARTITIONS_OFFSET) ==
		offsetof(TessSpillShape, max_partitions) &&
		tess_spill_set_layout(TESS_SPILL_SET_LEVEL_SIZE) == sizeof(TessSpillLevel) &&
		tess_spill_set_layout(TESS_SPILL_SET_LEVEL_PARTITIONS_OFFSET) ==
		offsetof(TessSpillLevel, partitions) &&
		tess_spill_set_layout(TESS_SPILL_SET_PLAN_SIZE) == sizeof(TessSpillPlan) &&
		tess_spill_set_layout(TESS_SPILL_SET_PLAN_DEEPER_OFFSET) ==
		offsetof(TessSpillPlan, deeper) &&
		tess_spill_set_layout((TessSpillSetLayout) 99) == 0 &&
	/* The machine of a join's spill and what a step of it takes and gives. */
		tess_join_layout(TESS_JOIN_MACHINE_SIZE) == sizeof(TessJoinMachine) &&
		tess_join_layout(TESS_JOIN_PARAMS_SIZE) == sizeof(TessJoinParams) &&
		tess_join_layout(TESS_JOIN_PARAMS_TAIL_OFFSET) == offsetof(TessJoinParams, tail) &&
		tess_join_layout(TESS_JOIN_HOST_SIZE) == sizeof(TessJoinHost) &&
		tess_join_layout(TESS_JOIN_HOST_NROUTES_OFFSET) == offsetof(TessJoinHost, nroutes) &&
		tess_join_layout(TESS_JOIN_EVENT_SIZE) == sizeof(TessJoinEvent) &&
		tess_join_layout(TESS_JOIN_EVENT_MEMORY_OFFSET) == offsetof(TessJoinEvent, memory) &&
		tess_join_layout(TESS_JOIN_ACTION_SIZE) == sizeof(TessJoinAction) &&
		tess_join_layout(TESS_JOIN_ACTION_PLAN_OFFSET) == offsetof(TessJoinAction, plan) &&
		tess_join_layout(TESS_JOIN_ACTION_BELOW_OFFSET) == offsetof(TessJoinAction, below) &&
		tess_join_layout(TESS_JOIN_BELOW_SIZE) == sizeof(TessJoinBelow) &&
		tess_join_layout(TESS_JOIN_BELOW_SHIFT_OFFSET) == offsetof(TessJoinBelow, shift) &&
		tess_join_layout(TESS_JOIN_EVENT_LEN_OFFSET) == offsetof(TessJoinEvent, len) &&
		tess_join_layout(TESS_JOIN_ACTION_OUTER_LEN_OFFSET) ==
		offsetof(TessJoinAction, outer_len) &&
		tess_join_layout((TessJoinLayout) 99) == 0 &&
	/* The machine of a grouping's spill and what a step of it takes and gives. */
		tess_group_layout(TESS_GROUP_MACHINE_SIZE) == sizeof(TessGroupMachine) &&
		tess_group_layout(TESS_GROUP_PARAMS_SIZE) == sizeof(TessGroupParams) &&
		tess_group_layout(TESS_GROUP_PARAMS_SPLIT_ROOM_OFFSET) ==
		offsetof(TessGroupParams, split_room) &&
		tess_group_layout(TESS_GROUP_HOST_SIZE) == sizeof(TessGroupHost) &&
		tess_group_layout(TESS_GROUP_HOST_NDISK_OFFSET) == offsetof(TessGroupHost, ndisk) &&
		tess_group_layout(TESS_GROUP_EVENT_SIZE) == sizeof(TessGroupEvent) &&
		tess_group_layout(TESS_GROUP_EVENT_ROWS_OFFSET) == offsetof(TessGroupEvent, rows) &&
		tess_group_layout(TESS_GROUP_ACTION_SIZE) == sizeof(TessGroupAction) &&
		tess_group_layout(TESS_GROUP_ACTION_BELOW_OFFSET) == offsetof(TessGroupAction, below) &&
		tess_group_layout(TESS_GROUP_BELOW_SIZE) == sizeof(TessGroupBelow) &&
		tess_group_layout(TESS_GROUP_BELOW_SHIFT_OFFSET) == offsetof(TessGroupBelow, shift) &&
		tess_group_layout((TessGroupLayout) 99) == 0 &&
	/* The machine of a grouping's rows and what a step of it takes and gives. */
		tess_rows_layout(TESS_ROWS_MACHINE_SIZE) == sizeof(TessRowsMachine) &&
		tess_rows_layout(TESS_ROWS_PARAMS_SIZE) == sizeof(TessRowsParams) &&
		tess_rows_layout(TESS_ROWS_PARAMS_LEAST_OFFSET) == offsetof(TessRowsParams, least) &&
		tess_rows_layout(TESS_ROWS_HOST_SIZE) == sizeof(TessRowsHost) &&
		tess_rows_layout(TESS_ROWS_EVENT_SIZE) == sizeof(TessRowsEvent) &&
		tess_rows_layout(TESS_ROWS_ACTION_SIZE) == sizeof(TessRowsAction) &&
		tess_rows_layout(TESS_ROWS_ACTION_TABLE_OFFSET) == offsetof(TessRowsAction, table) &&
		tess_rows_layout((TessRowsLayout) 99) == 0 &&
	/*
	 * The header of a chunk of columns as the inline readers of
	 * tessera/spill.h take it: the row count, the capacity and the stored
	 * words, a uint32 each from the start.
	 */
		tess_spill_columns_layout(TESS_SPILL_COLUMNS_HEADER_SIZE) ==
		TESS_SPILL_COLUMNS_HEADER &&
		tess_spill_columns_layout(TESS_SPILL_COLUMNS_ROWS_OFFSET) ==
		0 * sizeof(uint32) &&
		tess_spill_columns_layout(TESS_SPILL_COLUMNS_CAPACITY_OFFSET) ==
		1 * sizeof(uint32) &&
		tess_spill_columns_layout(TESS_SPILL_COLUMNS_WORDS_OFFSET) ==
		2 * sizeof(uint32) &&
		tess_spill_columns_layout(99) == 0;
}

#endif							/* TESSERA_KERNELS_LAYOUT_H */

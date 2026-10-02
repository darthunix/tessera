/*
 * Whether the linked Rust kernels lay out the shared structures as these
 * headers do.
 *
 * The C side and the kernels library agree on the sizes and offsets of the
 * structures they pass each other (a row mask, a Datum column, a status, a
 * table key, a sort key, a chunk header) and on a few numbers the C side
 * computes with (the bits of a record reference, the bytes of a spilled
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
		tess_table_layout((TessTableLayoutKind) 99) == 0 &&
		tess_sort_layout(TESS_SORT_LAYOUT_KEY_SIZE) == sizeof(TessSortKey) &&
		tess_sort_layout(TESS_SORT_LAYOUT_KEY_FLAGS_OFFSET) ==
		offsetof(TessSortKey, flags) &&
		tess_sort_layout((TessSortLayoutKind) 99) == 0 &&
		tess_spill_header_size() == TESS_SPILL_HEADER_SIZE &&
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

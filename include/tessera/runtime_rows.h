/*
 * Rows a node keeps as records of the kernels' table format. Part of
 * tessera/runtime.h.
 */
#ifndef TESSERA_RUNTIME_ROWS_H
#define TESSERA_RUNTIME_ROWS_H

#include "postgres.h"

#include "tessera/abi.h"
#include "tessera/row_mask.h"
#include "tessera/table_key.h"

/* Declared in tessera/kernel_ops.h and tessera/sort.h. */
typedef struct TessKernelOps TessKernelOps;
typedef struct TessSortKey TessSortKey;

/*
 * Rows a node keeps, such as the input of a sort: records of the kernels'
 * table format (tessera/table.h) in chunks of the node's memory, never
 * moved, each with the row's keys and a payload of its kept columns, a
 * word of their NULL bits per 64 of them and then a Datum each; a
 * by-reference value is
 * copied into value chunks of its own and its word is a reference to it
 * (the chunk's number plus one, and the byte), so that a chunk means the
 * same wherever it is read, spilled blocks included. A record is named by
 * its 32-bit reference, which tess_rows_append returns and every gather
 * takes. The records are not linked: there is an index only of the
 * layout, which the kernels check on every call. Serial only, in the
 * caller's memory. (TessHashJoin keeps its rows the same way with code of
 * its own, which shares them between processes and partitions.)
 */
typedef struct TessRows TessRows;

typedef struct TessRowsConfig
{
	Size		struct_size;
	/* Owns the rows, their chunks and their values. */
	MemoryContext parent_context;
	/*
	 * The table's kernels: size, create, chunk_init, append_columns,
	 * gather_scattered, gather_words.
	 */
	const TessKernelOps *kernels;
	/* The keys every record holds, in their slots: one at least. */
	int			nkeys;
	const TessTableKeyKind *kinds;
	/* The kept columns, at most TESS_ROWS_MAX_COLUMNS, and their types. */
	int			ncolumns;
	const int16 *typlens;
	const bool *typbyvals;
	/*
	 * The most bytes a chunk of records or of values takes, 0 for 1 MB (the
	 * first of each takes 64 kB at most): smaller for a caller whose memory
	 * is small, such as a sort that writes runs.
	 */
	Size		chunk_len;
} TessRowsConfig;

#define TESS_ROWS_CONFIG_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessRowsConfig, typbyvals)

/* The most kept columns: a tuple's most attributes (MaxTupleAttributeNumber). */
#define TESS_ROWS_MAX_COLUMNS 1664

/* Empty rows. */
extern TessRows *tess_rows_create(const TessRowsConfig *config);

/*
 * Append the rows of rows as records: keys are the rows' nkeys keys, as a
 * table takes them, and columns their kept columns, each over the same
 * rows; refs[row] receives the reference of each row's record. Chunks
 * are added as the records need; a by-reference value is copied, an
 * expanded object flattened.
 */
extern void tess_rows_append(TessRows *rows, const TessTableKey *keys,
							 const TessDatumColumn *columns,
							 const TessRowMask *mask, uint32 *refs);

/*
 * Kept column `column` of the records refs[row] for each row of mask into
 * values and isnull: a by-reference value as the address of its copy,
 * valid as long as the rows. Other rows keep their values; their NULL
 * flags may be set false, for all mask->nrows rows at once.
 */
extern void tess_rows_gather(TessRows *rows, int column, const uint32 *refs,
							 const TessRowMask *mask, Datum *values,
							 bool *isnull);

/*
 * Every kept column of the records refs[row] for each row of mask, column
 * c into values[c] and isnull[c], as tess_rows_gather leaves one: the
 * payload's words in one call of the kernels, each record located once,
 * as a sort's batch serves all its columns.
 */
extern void tess_rows_gather_columns(TessRows *rows, const uint32 *refs,
									 const TessRowMask *mask,
									 Datum *const *values, bool *const *isnull);

/*
 * The references of every record, ordered by keys, one per key of the
 * rows in key order (tessera/sort.h), into refs, which holds
 * tess_rows_count of them. The items are allocated for the call and
 * freed; the kernels need the sort's operations.
 */
extern void tess_rows_sort(TessRows *rows, const TessSortKey *keys,
						   uint32 *refs);

/*
 * As tess_rows_sort, and the sorted items returned: *words words each, in
 * the rows' memory, the reference of each in the low 32 bits of its last
 * word; the caller frees them. NULL for no rows.
 */
extern uint64 *tess_rows_sort_items(TessRows *rows, const TessSortKey *keys,
									uint32 *refs, int *words);

/*
 * Top-N: push the item of each record refs[row] of mask, keyed by keys as
 * for tess_rows_sort, into a heap of capacity items at heap whose first
 * *len are the heap (tessera/sort.h, tess_sort_top_push).
 */
extern void tess_rows_top_push(TessRows *rows, const TessSortKey *keys,
							   const uint32 *refs, const TessRowMask *mask,
							   uint64 *heap, Size capacity, uint64 *len);

/*
 * Whether a row of the mask is NULL by its flag in isnull, one per
 * physical row: for a caller that keeps a column's NULLs apart.
 */
extern bool tess_rows_selected_null(const TessRowMask *mask, const bool *isnull);

/*
 * Write 0 into key key of every record, its NULL bit kept: the key then
 * orders no two records that are not NULL. A sort that gives up a key's
 * abbreviated values does this to the records it holds.
 */
extern void tess_rows_clear_key(TessRows *rows, int key);

/* The records appended, and the bytes the rows take: chunks, values and index. */
extern uint64 tess_rows_count(const TessRows *rows);
extern Size tess_rows_memory(const TessRows *rows);

/* Forget every record, keeping nothing but the layout. */
extern void tess_rows_reset(TessRows *rows);

/* Release the rows and their memory. */
extern void tess_rows_free(TessRows *rows);

#endif							/* TESSERA_RUNTIME_ROWS_H */

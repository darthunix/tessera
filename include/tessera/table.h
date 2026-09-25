/* C entry points of the Rust hash table in a region the caller owns. */
#ifndef TESSERA_TABLE_H
#define TESSERA_TABLE_H

#include "postgres.h"

#include "tessera/abi.h"
#include "tessera/batch.h"
#include "tessera/row_mask.h"
#include "tessera/status.h"

/*
 * The table (crates/tessera-kernels, module table) lives in a region of
 * memory the caller allocates and passes to every call as a pointer and a
 * length: palloc'd memory of a serial plan, grown with repalloc, or dynamic
 * shared memory of a parallel one. Rust keeps nothing between calls and
 * allocates nothing: everything in the region is addressed by offsets from
 * its start, so the bytes survive a move and mean the same in every
 * process. The region must be aligned to 8; tess_table_size says how many
 * bytes a capacity needs, tess_table_create lays the table out over all
 * the bytes given, and every other call attaches anew, checking the whole
 * header, and checks every offset it follows, so a corrupt region is a
 * status, never a crash or a hang. See docs/table.md.
 *
 * Records are addressed by 32-bit offsets in units of 8 bytes; 0 is none.
 * A batch brings its hashes (from tess_int4_hash, tess_int8_hash and their
 * _next forms, which apply the NULL policy), its keys as Datum columns and
 * a row mask. An int8 inside the int4 range hashes as the int4, so int4
 * and int8 key columns meet in one table.
 * A full table is not an error: tess_table_insert leaves the rows it had
 * no room for in the pending mask, and the caller grows the region.
 *
 * Insertions may run in several processes at once over shared memory, and
 * so may probes, but not both at a time. Every status rule of
 * tessera/kernels.h applies: outputs are unspecified after a failure, and
 * the caller reports the status with ereport after the call returns.
 */

/* The format of the region this header describes. */
#define TESS_TABLE_FORMAT_VERSION 1

/* The most keys a record holds. */
#define TESS_TABLE_MAX_KEYS 16

/* What a key column holds; every key takes an 8-byte slot in a record. */
typedef enum TessTableKeyKind
{
	/* An int4 Datum, sign-extended into its slot. */
	TESS_TABLE_KEY_INT4 = 1,
	/* An int8 Datum. */
	TESS_TABLE_KEY_INT8 = 2
} TessTableKeyKind;

/*
 * One key of a batch: a Datum column of the kind, with prepared as its
 * readiness (NULL when the whole column is initialized), as for the
 * kernels. Every key column has the batch's row count.
 */
typedef struct TessTableKey
{
	TessTableKeyKind kind;
	const TessDatumColumn *column;
	const TessRowMask *prepared;
} TessTableKey;

/* Counts of a table, for planning and EXPLAIN; the caller sets struct_size. */
typedef struct TessTableStats
{
	Size		struct_size;
	/* Records inserted. */
	uint64		records;
	/* Buckets of the table. */
	uint64		buckets;
	/* Bytes in use: the header, the records and the buckets. */
	uint64		bytes_used;
	/* Bytes of the region the table was created or grown over. */
	uint64		region_len;
} TessTableStats;

#define TESS_TABLE_STATS_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessTableStats, region_len)

/*
 * A record as the table exposes it; the caller sets struct_size. keys and
 * payload point into the region and stay valid until the region moves or
 * the table grows.
 */
typedef struct TessTableRecord
{
	Size		struct_size;
	/* The hash it was inserted with. */
	uint32		hash;
	/* Bit k set: key k is NULL, and its slot holds 0. */
	uint32		null_bits;
	/* One slot per key, in key order. */
	const int64 *keys;
	/* The payload, of payload_size bytes. */
	const uint8 *payload;
	Size		payload_size;
} TessTableRecord;

#define TESS_TABLE_RECORD_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessTableRecord, payload_size)

/* Sizes and offsets the Rust side was built with, for layout checks. */
typedef enum TessTableLayoutKind
{
	TESS_TABLE_LAYOUT_HEADER_SIZE = 0,
	TESS_TABLE_LAYOUT_VERSION_OFFSET = 1,
	TESS_TABLE_LAYOUT_KEY_SIZE = 2,
	TESS_TABLE_LAYOUT_KEY_PREPARED_OFFSET = 3,
	TESS_TABLE_LAYOUT_STATS_SIZE = 4,
	TESS_TABLE_LAYOUT_STATS_REGION_LEN_OFFSET = 5,
	TESS_TABLE_LAYOUT_RECORD_SIZE = 6,
	TESS_TABLE_LAYOUT_RECORD_PAYLOAD_OFFSET = 7
} TessTableLayoutKind;

/* The region format the library writes and accepts; must equal the header's. */
extern uint32 tess_table_format_version(void);

/* The size or offset for a kind, or 0 for an unknown one. */
extern Size tess_table_layout(TessTableLayoutKind kind);

/*
 * The bytes a region needs for a table of nkeys keys of the given kinds,
 * a payload of payload_size bytes per record and capacity records: the
 * header, the records and the buckets, a multiple of 8.
 */
extern TessStatusCode tess_table_size(int nkeys,
									  const TessTableKeyKind *kinds,
									  Size payload_size,
									  uint64 capacity,
									  Size *size,
									  TessStatus *status);

/*
 * Create an empty table for capacity records in the len bytes at region,
 * a multiple of 8 of at least tess_table_size; all of them are used, so a
 * larger region holds more records.
 */
extern TessStatusCode tess_table_create(void *region,
										Size len,
										int nkeys,
										const TessTableKeyKind *kinds,
										Size payload_size,
										uint64 capacity,
										TessStatus *status);

/* Check that the len bytes at region hold a table of this format. */
extern TessStatusCode tess_table_attach(const void *region,
										Size len,
										TessStatus *status);

/* The counts of the table as of now. */
extern TessStatusCode tess_table_stats(const void *region,
									   Size len,
									   TessTableStats *stats,
									   TessStatus *status);

/*
 * Insert the rows of pending as new records, in row order, until the
 * table has no room: each row inserted leaves pending and gets the offset
 * of its record in offsets (one slot per row of the batch); rows still
 * pending need a larger region. hashes has one hash per row; keys are the
 * table's nkeys keys; payload is the payload of every row one after
 * another (payload_size bytes each, as the table was created), or NULL
 * for zeros. Equal keys make separate records.
 */
extern TessStatusCode tess_table_insert(void *region,
										Size len,
										const uint32 *hashes,
										int nkeys,
										const TessTableKey *keys,
										const uint8 *payload,
										TessRowMask *pending,
										uint32 *offsets,
										TessStatus *status);

/*
 * Find the first record of its chain with the hash, NULL bits and keys of
 * each row of rows: matches[row] receives its offset and found, a mask
 * this call fills whole, the rows that have one. The other records with
 * the same keys follow through tess_table_next_match, or
 * tess_table_next_in_group in a table filled by grouped insertion.
 */
extern TessStatusCode tess_table_probe(const void *region,
									   Size len,
									   const uint32 *hashes,
									   int nkeys,
									   const TessTableKey *keys,
									   const TessRowMask *rows,
									   uint32 *matches,
									   TessRowMask *found,
									   TessStatus *status);

/*
 * For each row of rows, replace offsets[row], an offset from a probe or an
 * earlier call, by the offset of the next record in its chain with the
 * same hash, NULL bits and keys; found receives the rows that have one,
 * and the others keep their offset.
 */
extern TessStatusCode tess_table_next_match(const void *region,
											Size len,
											uint32 *offsets,
											const TessRowMask *rows,
											TessRowMask *found,
											TessStatus *status);

/*
 * For each row of rows, the 8 bytes at byte `at` of the payload of the
 * record at offsets[row] into values[row]: one word of a batch's matches
 * per call, such as a Datum of the build row a join keeps there, with one
 * check of the header for the batch. at + 8 must be within the payload;
 * rows outside rows keep their values.
 */
extern TessStatusCode tess_table_gather(const void *region,
										Size len,
										const uint32 *offsets,
										const TessRowMask *rows,
										Size at,
										Datum *values,
										TessStatus *status);

/*
 * For each row of rows, replace offsets[row] by the record right after it
 * in its chain when that one has the same hash, NULL bits and keys, and
 * put the row in found; the others keep their offset. In a table filled
 * by tess_table_insert_grouped this is the key's next record, in one step
 * instead of tess_table_next_match's walk down the chain.
 */
extern TessStatusCode tess_table_next_in_group(const void *region,
											   Size len,
											   uint32 *offsets,
											   const TessRowMask *rows,
											   TessRowMask *found,
											   TessStatus *status);

/* The record at an offset a call of this table returned. */
extern TessStatusCode tess_table_record(const void *region,
										Size len,
										uint32 offset,
										TessTableRecord *record,
										TessStatus *status);

/*
 * What one writer alone may do, with no other call over the region at the
 * same time: grouping resolves rows to the record of their keys and
 * changes payloads in place, output walks the records, and a full table
 * grows.
 */

/*
 * Give each row of pending the record of its keys, creating one with a
 * zero payload where none exists, in row order, until the table has no
 * room for a new one: resolved rows leave pending and get their record
 * offsets in offsets; the rows whose record this call created form
 * inserted, a mask this call fills whole. Rows left pending need a larger
 * region.
 */
extern TessStatusCode tess_table_find_or_insert(void *region,
												Size len,
												const uint32 *hashes,
												int nkeys,
												const TessTableKey *keys,
												TessRowMask *pending,
												uint32 *offsets,
												TessRowMask *inserted,
												TessStatus *status);

/*
 * Insert the rows of pending as tess_table_insert does, but each right
 * after a record with the same keys when the table holds one, so that a
 * key's records lie next to each other and tess_table_next_in_group steps
 * through them; duplicates, a mask this call fills whole, receives the
 * rows whose keys were there already, from an earlier call or an earlier
 * row. A lookup per row, and one writer: not for a table several
 * processes build at once.
 */
extern TessStatusCode tess_table_insert_grouped(void *region,
												Size len,
												const uint32 *hashes,
												int nkeys,
												const TessTableKey *keys,
												const uint8 *payload,
												TessRowMask *pending,
												uint32 *offsets,
												TessRowMask *duplicates,
												TessStatus *status);

/* How tess_table_accumulate folds a row into an aggregate state. */
typedef enum TessTableAccumulate
{
	/* count(*): +1 per row, no column. */
	TESS_TABLE_COUNT_ROWS = 1,
	/* count(x): +1 per non-NULL value, the column's NULL flags alone. */
	TESS_TABLE_COUNT = 2,
	/* sum(int4): an int8 sum; past its range 22003 "bigint out of range". */
	TESS_TABLE_SUM_INT4 = 3,
	TESS_TABLE_MIN_INT4 = 4,
	TESS_TABLE_MAX_INT4 = 5,
	TESS_TABLE_MIN_INT8 = 6,
	TESS_TABLE_MAX_INT8 = 7
} TessTableAccumulate;

/*
 * Fold each selected row of rows into the aggregate state of its record,
 * the offset in offsets from tess_table_find_or_insert: the int8 at byte
 * value_at of the payload. Counts add one; a sum, a minimum or a maximum
 * takes the row's non-NULL value, the first one also setting bit
 * flag_bit of the word at byte flags_at, so a state without it has seen
 * no value and stands for NULL. Rows go in row order, so an overflow
 * fails where the row-wise transition would. column (with prepared as its
 * readiness) is NULL for count(*). Both words are 8-byte aligned within
 * the payload. One writer, as for the other grouping calls.
 */
extern TessStatusCode tess_table_accumulate(void *region,
											Size len,
											const uint32 *offsets,
											const TessRowMask *rows,
											TessTableAccumulate op,
											const TessDatumColumn *column,
											const TessRowMask *prepared,
											Size value_at,
											Size flags_at,
											uint32 flag_bit,
											TessStatus *status);

/*
 * For each row of rows, key `key` of the record at offsets[row]: its
 * Datum into values[row] (an int4 key sign-extended, as Int32GetDatum
 * makes it) and whether it is NULL into isnull[row]; rows outside rows
 * keep their values.
 */
extern TessStatusCode tess_table_gather_key(const void *region,
											Size len,
											const uint32 *offsets,
											const TessRowMask *rows,
											int key,
											Datum *values,
											bool *isnull,
											TessStatus *status);

/*
 * The payload of the record at an offset, to change in place: payload_size
 * bytes valid until the region moves or the table grows.
 */
extern TessStatusCode tess_table_payload(void *region,
										 Size len,
										 uint32 offset,
										 uint8 **payload,
										 TessStatus *status);

/*
 * Visit the records from the cursor on, in insertion order, up to capacity
 * of them: their offsets fill offsets, count receives how many, and the
 * cursor moves past them. The caller starts the cursor at 0; a count of 0
 * means the walk is over.
 */
extern TessStatusCode tess_table_scan(void *region,
									  Size len,
									  uint64 *cursor,
									  uint32 *offsets,
									  int capacity,
									  int *count,
									  TessStatus *status);

/*
 * Grow the table to the len bytes at region, after the caller made the
 * region that large with its used bytes intact (repalloc, or a copy into
 * a new region): the buckets are rebuilt at the new end for the records
 * that could now fit, and records and their offsets stay as they were.
 * len is a multiple of 8 of at least the old length.
 */
extern TessStatusCode tess_table_grow(void *region,
									  Size len,
									  TessStatus *status);

#endif							/* TESSERA_TABLE_H */

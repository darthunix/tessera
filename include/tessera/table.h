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
 * A batch brings its hashes (from tess_int4_hash and tess_int4_hash_next,
 * which apply the NULL policy), its keys as Datum columns and a row mask.
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
 * Find the newest record with the hash, NULL bits and keys of each row of
 * rows: matches[row] receives its offset and found, a mask this call
 * fills whole, the rows that have one. Older records with the same keys
 * follow through tess_table_next_match.
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

/* The record at an offset a call of this table returned. */
extern TessStatusCode tess_table_record(const void *region,
										Size len,
										uint32 offset,
										TessTableRecord *record,
										TessStatus *status);

#endif							/* TESSERA_TABLE_H */

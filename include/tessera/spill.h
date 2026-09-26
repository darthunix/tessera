/*
 * C entry points of the spilled blocks' headers (crates/tessera-spill).
 *
 * A node that spills writes chunks of its hash table whole: a chunk of
 * records or a chunk of the by-reference values its records refer to,
 * each as a header of tess_spill_header_size() bytes and then the chunk's
 * bytes. The header names the chunk's number, partition and level and the
 * table's layout fingerprint (tess_table_fingerprint), so that a block of
 * another table or a damaged file is an error status on reading, never a
 * record read the wrong way. The library reads and writes no file: the
 * node does, through PostgreSQL's temporary files. See docs/spill.md.
 */
#ifndef TESSERA_SPILL_H
#define TESSERA_SPILL_H

#include "postgres.h"

#include "tessera/status.h"
#include "tessera/table.h"

/* What a spilled block holds. */
typedef enum TessSpillKind
{
	/* A chunk of records, its used mark first. */
	TESS_SPILL_RECORDS = 1,
	/* A chunk of by-reference values. */
	TESS_SPILL_VALUES = 2
} TessSpillKind;

/* The header of a spilled block. */
typedef struct TessSpillHeader
{
	uint32		kind;
	/* The chunk's number in its table, records and values each their own. */
	uint32		number;
	uint32		partition;
	/* The level of partitioning, 0 for the first, below 32. */
	uint32		level;
	uint64		fingerprint;
	/* Bytes of the body after the header, a multiple of 8. */
	uint64		len;
	/*
	 * A chunk of records stored packed (tess_spill_pack): the bytes on
	 * disk, fewer than len; 0 when the body is stored as it is.
	 */
	uint32		packed;
} TessSpillHeader;

/* Bytes of a spilled block's header, what tess_spill_header_size() returns. */
#define TESS_SPILL_HEADER_SIZE 48

extern Size tess_spill_header_size(void);

/* The fingerprint of the table's record layout. */
extern TessStatusCode tess_table_fingerprint(const TessTableRef *table,
											 uint64 *fingerprint,
											 TessStatus *status);

/*
 * Lay out a header in the first tess_spill_header_size() of the len bytes
 * at out, checked as a reader accepting bodies of at most max_len bytes
 * would check it.
 */
extern TessStatusCode tess_spill_header_write(void *out, Size len,
											  const TessSpillHeader *header,
											  uint64 max_len,
											  TessStatus *status);

/*
 * Read and check the header at bytes: its magic, version and kind, the
 * table's fingerprint and a body of at most max_len bytes.
 */
extern TessStatusCode tess_spill_header_read(const void *bytes, Size len,
											 uint64 fingerprint,
											 uint64 max_len,
											 TessSpillHeader *header,
											 TessStatus *status);

/*
 * Pack the len bytes of a chunk of records at chunk into out, of capacity
 * bytes (len is enough): *packed gets the packed length, or 0 when the
 * chunk is not one of records of one length or would not get shorter.
 * Each 4-byte lane of the records is stored at the width its values need
 * in this chunk; the lane of the next-record references is dropped.
 */
extern TessStatusCode tess_spill_pack(const void *chunk, Size len, void *out,
									  Size capacity, Size *packed,
									  TessStatus *status);

/* Unpack len bytes tess_spill_pack made into the chunk of chunk_len bytes. */
extern TessStatusCode tess_spill_unpack(const void *packed, Size len, void *chunk,
										Size chunk_len, TessStatus *status);

#endif							/* TESSERA_SPILL_H */

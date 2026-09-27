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
	TESS_SPILL_VALUES = 2,
	/* A chunk of rows by column, always stored packed. */
	TESS_SPILL_COLUMNS = 3
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

/*
 * Chunks of rows by column (crates/tessera-spill, columns): the outer rows
 * a join keeps for their partition, which are only written and read back,
 * never linked. A chunk is a header of TESS_SPILL_COLUMNS_HEADER bytes (its
 * row count, then its capacity, a uint32 each, then its stored words and
 * a magic), then lanes of capacity words each: the rows' NULL bits (bit
 * w % 64 of lane w / 64 for word w, one lane at least), then one lane per
 * stored word, 0 for a NULL. A row's place
 * is referred to as (chunk << TESS_SPILL_COLUMNS_PLACE_BITS) | place. On
 * disk each lane is stored for the chunk's rows only, by frame of
 * reference; read back, the chunk has a capacity of its rows.
 */
#define TESS_SPILL_COLUMNS_HEADER 16
#define TESS_SPILL_COLUMNS_PLACE_BITS 17
/* The lanes of NULL bits of a chunk of words stored words. */
static inline int
tess_spill_columns_null_lanes(uint32 words)
{
	return words <= 64 ? 1 : (int) ((words + 63) / 64);
}

/* The most a packed chunk of words stored words takes past its own bytes. */
#define TESS_SPILL_COLUMNS_SLACK(words) \
	(8 + 16 * ((Size) tess_spill_columns_null_lanes(words) + (Size) (words)))

static inline uint32
tess_spill_columns_rows(const void *chunk)
{
	return ((const uint32 *) chunk)[0];
}

static inline uint32
tess_spill_columns_capacity(const void *chunk)
{
	return ((const uint32 *) chunk)[1];
}

/* A chunk's stored words. */
static inline uint32
tess_spill_columns_words(const void *chunk)
{
	return ((const uint32 *) chunk)[2];
}

/* Set a chunk's row count: its rows fill the first places of each lane. */
static inline void
tess_spill_columns_set_rows(void *chunk, uint32 rows)
{
	((uint32 *) chunk)[0] = rows;
}

/*
 * Lane `lane` of a chunk: the lanes of NULL bits first, then a lane per
 * stored word (tess_spill_columns_word); with 64 words or fewer, 0 is the
 * NULL bits and 1 + w stored word w.
 */
static inline uint64 *
tess_spill_columns_lane(void *chunk, int lane)
{
	return (uint64 *) ((char *) chunk + TESS_SPILL_COLUMNS_HEADER +
					   sizeof(uint64) * (Size) tess_spill_columns_capacity(chunk) * lane);
}

/* The lane of stored word `word`. */
static inline uint64 *
tess_spill_columns_word(void *chunk, int word)
{
	return tess_spill_columns_lane(chunk,
								   tess_spill_columns_null_lanes(tess_spill_columns_words(chunk)) +
								   word);
}

/*
 * The header's bytes (0), where its row count (1), capacity (2) and stored
 * words (3) lie.
 */
extern Size tess_spill_columns_layout(int what);

/* Make the len bytes at chunk an empty chunk of words stored words. */
extern TessStatusCode tess_spill_columns_init(void *chunk, Size len, int words,
											  Size *capacity, TessStatus *status);

/*
 * Append the rows of pending to the chunks of their partitions, as
 * tess_table_append_partitioned_columns appends records: the partition of
 * a hash is (hash >> shift) & (npartitions - 1) and appends to chunk
 * partition_chunks[p] of the nchunks at bases, of lens bytes; a row whose
 * chunk is full stays pending. Appended rows get their places in offsets
 * and add one to rows[partition].
 */
extern TessStatusCode tess_spill_columns_append_partitioned(void *const *bases,
															const Size *lens,
															int nchunks,
															const uint32 *partition_chunks,
															int npartitions,
															uint32 shift,
															const uint32 *hashes,
															int ncolumns,
															const TessDatumColumn *columns,
															TessRowMask *pending,
															uint32 *offsets,
															uint64 *rows,
															TessStatus *status);

/*
 * Pack the chunk of len bytes at chunk into out, of capacity bytes (len +
 * TESS_SPILL_COLUMNS_SLACK suffice): *packed gets the packed bytes,
 * *unpacked the bytes of the chunk it unpacks into.
 */
extern TessStatusCode tess_spill_columns_pack(const void *chunk, Size len, void *out,
											  Size capacity, Size *packed,
											  Size *unpacked, TessStatus *status);

/* Unpack len bytes tess_spill_columns_pack made into the chunk of chunk_len bytes. */
extern TessStatusCode tess_spill_columns_unpack(const void *packed, Size len,
												void *chunk, Size chunk_len,
												TessStatus *status);

#endif							/* TESSERA_SPILL_H */

/*
 * C entry points of the spilled blocks' headers (crates/tessera-spill).
 *
 * A node that spills writes chunks whole: a chunk of its hash table's
 * records, a chunk of the by-reference values that records or rows refer
 * to, or a chunk of rows by column, each as a header of
 * tess_spill_header_size() bytes and then the chunk's bytes or their
 * packed form. The header names the chunk's number, partition and level and the
 * table's layout fingerprint (tess_table_fingerprint), so that a block of
 * another table or a damaged file is an error status on reading, never a
 * record read the wrong way. The library reads and writes no file: the
 * node does, through PostgreSQL's temporary files. The format is the
 * capability spill-format (openspec/specs/spill-format).
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
	/*
	 * Bytes of the chunk a reader gets, a multiple of 8: the body on disk,
	 * or what it unpacks into when it is stored packed.
	 */
	uint64		len;
	/*
	 * The bytes on disk of a body stored packed: a chunk of records when
	 * that makes it shorter (tess_spill_pack), a chunk of columns always; 0
	 * when the body is stored as it is.
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
 * set's fingerprint, a body of at most max_len bytes, the packed length
 * its kind allows and a level below 32.
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
 * a join keeps for their partition, the runs of an external TessSort and
 * the messages of TessGather, which are only written and read back, never
 * linked. A chunk is a header of TESS_SPILL_COLUMNS_HEADER bytes (its
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
/* The most words stored for a row: tess_spill_columns_init refuses more. */
#define TESS_SPILL_COLUMNS_MAX_WORDS 4096

/*
 * A word that refers to a by-reference value, in a record's payload and in
 * a row of a chunk of columns alike: the number of the value's chunk plus
 * one, above TESS_SPILL_VALUE_BYTE_BITS bits of the value's byte in that
 * chunk. No address of a process, so the word means the same in every
 * process and on disk; and never 0, which is no reference. A chunk of
 * columns that is written with one chunk of values before it refers to
 * that chunk as number 0.
 */
#define TESS_SPILL_VALUE_BYTE_BITS 32
#define TESS_SPILL_VALUE_REF(number, byte) \
	((((uint64) (number) + 1) << TESS_SPILL_VALUE_BYTE_BITS) | (uint64) (byte))

/* The byte a reference names in its chunk of values. */
static inline uint64
tess_spill_value_byte(uint64 ref)
{
	return ref & ((UINT64CONST(1) << TESS_SPILL_VALUE_BYTE_BITS) - 1);
}

/*
 * The value that ref names in the len bytes at values, when those bytes
 * are the one chunk of values of a chunk of columns, its number 0; NULL
 * when ref is no reference into them. Bytes read from a file are held to
 * this: tess_spill_value_damaged reports the other case.
 */
static inline const char *
tess_spill_value_in(const char *values, uint64 len, uint64 ref)
{
	if ((ref >> TESS_SPILL_VALUE_BYTE_BITS) != 1 || tess_spill_value_byte(ref) >= len)
		return NULL;
	return values + tess_spill_value_byte(ref);
}
/*
 * The lanes of NULL bits of a chunk of words stored words, one at least, as
 * crates/tessera-spill (columns::null_lanes) lays them out; a constant
 * expression, for the bound of an array. A message of TessGather is a
 * chunk of columns too.
 */
#define TESS_SPILL_COLUMNS_NULL_LANES(words) \
	((words) <= 64 ? 1 : ((words) + 63) / 64)

static inline int
tess_spill_columns_null_lanes(uint32 words)
{
	return (int) TESS_SPILL_COLUMNS_NULL_LANES(words);
}

/* The lane of NULL bits that holds stored word word's bit. */
static inline int
tess_spill_columns_null_lane(int word)
{
	return word / 64;
}

/* Stored word word's bit in its lane of NULL bits. */
static inline uint64
tess_spill_columns_null_bit(int word)
{
	return UINT64CONST(1) << (word % 64);
}

/* Whether a word of a lane of NULL bits marks stored word word NULL. */
static inline bool
tess_spill_columns_is_null(uint64 bits, int word)
{
	return ((bits >> (word % 64)) & 1) != 0;
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
 * The partitions of a level of a spill: from min_partitions, the power of
 * two that makes each hold about half of limit of the expected bytes, or
 * at_least of them, as long as each partition's reserve (its tails of
 * chunks and its files' buffers) fits in half of limit, the hash bits from
 * shift last and max_partitions is not passed. A grouping's spill, a
 * join's and a shared join table's take it with their own parameters
 * (openspec/specs/hash-table/). Expected bytes below zero or not a
 * number, and a shift past which min_partitions do not fit in the 32 bits
 * of the hash, are refused.
 */
extern TessStatusCode tess_spill_partitions(double expected, Size limit, Size reserve,
											uint32 shift, uint32 min_partitions,
											uint32 max_partitions, uint32 at_least,
											uint32 *partitions, TessStatus *status);

/*
 * The length of a level's chunks: limit / (share * partitions) bytes, no
 * more than max_chunk, no less than min_chunk (which wins over max_chunk),
 * rounded down to a multiple of 8. A share or partitions of 0, and a
 * min_chunk below 8 or not a multiple of 8, are refused.
 */
extern TessStatusCode tess_spill_chunk_len(Size limit, uint32 partitions, Size share,
										   Size min_chunk, Size max_chunk,
										   Size *chunk_len, TessStatus *status);

/* What tess_spill_columns_layout gives the size or offset of. */
typedef enum TessSpillColumnsField
{
	/* The header's bytes, TESS_SPILL_COLUMNS_HEADER. */
	TESS_SPILL_COLUMNS_HEADER_SIZE = 0,
	/* Where the row count, the capacity and the stored words lie. */
	TESS_SPILL_COLUMNS_ROWS_OFFSET = 1,
	TESS_SPILL_COLUMNS_CAPACITY_OFFSET = 2,
	TESS_SPILL_COLUMNS_WORDS_OFFSET = 3
} TessSpillColumnsField;

/*
 * The size or offset of a field of the header as the Rust side lays it
 * out, which the inline readers above take for granted; 0 for another code.
 */
extern Size tess_spill_columns_layout(int what);

/* What tess_spill_columns_shape gives of a chunk of a number of stored words. */
typedef enum TessSpillColumnsShape
{
	/* Its lanes of NULL bits, tess_spill_columns_null_lanes. */
	TESS_SPILL_COLUMNS_SHAPE_NULL_LANES = 0,
	/* The most its packed form takes past its bytes, TESS_SPILL_COLUMNS_SLACK. */
	TESS_SPILL_COLUMNS_SHAPE_SLACK = 1
} TessSpillColumnsShape;

/*
 * A count of a chunk of words stored words as the Rust side computes it,
 * which the inline formulas above repeat; 0 for another code.
 */
extern Size tess_spill_columns_shape(uint32 words, int what);

/* Make the len bytes at chunk an empty chunk of words stored words. */
extern TessStatusCode tess_spill_columns_init(void *chunk, Size len, int words,
											  Size *capacity, TessStatus *status);

/*
 * Append the rows of rows, in their order, to the chunk of columns of len
 * bytes at chunk after its rows: column c of a row (ncolumns of columns,
 * by value when byvals[c], else of typlens[c], -1 a varlena, -2 a C
 * string) is the chunk's word c, the words past them the caller's. A NULL
 * value's word is 0 with its NULL bit; a by-value one's its Datum; a
 * by-reference one's a reference (TESS_SPILL_VALUE_REF of chunk 0) to its
 * bytes, as datumGetSize counts them, copied into values at *values_used,
 * which moves past them to a multiple of 8; values_len is at most 4 GiB. It stops when the chunk is full or the next row's values
 * would pass values_len: the rows appended leave rows, *appended gets
 * their count and *need the bytes of values the next row takes (0 when
 * every row went), for the caller to make room.
 */
extern TessStatusCode tess_spill_columns_append(void *chunk, Size len, int ncolumns,
												const TessDatumColumn *columns,
												const bool *byvals, const int16 *typlens,
												TessRowMask *rows, char *values,
												Size values_len, Size *values_used,
												int *appended, Size *need,
												TessStatus *status);

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

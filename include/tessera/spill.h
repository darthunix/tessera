/*
 * C entry points of the spilled blocks' headers (crates/relspill).
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
 * Chunks of rows by column (crates/relspill, columns): the outer rows
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
 * crates/relspill (columns::null_lanes) lays them out; a constant
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
 * The plan of a level of a spill, one rule for every node's levels
 * (relspill's plan): how many partitions a level splits into, how long
 * the chunks each keeps in memory are, how many spill words count them,
 * and which bits of the hash it takes. A level takes the low bits of a
 * row's hash, the first level from bit 0 and each level below from the
 * bit past those of the level above, so a row's partition is
 * tess_table_partition(hash, plan.shift, plan.partitions); the buckets of
 * a table take the high bits.
 *
 * TessSpillShape is what every level of a node's spill shares:
 *
 * - reserve: what each partition keeps in memory at the least, in bytes;
 * - min_partitions and max_partitions: the partitions to start from and
 *   the most, powers of two, the first no larger, the same for a node of
 *   a fixed count;
 * - share, min_chunk and max_chunk: the chunks take the limit shared
 *   among share times the partitions, no longer than max_chunk, and no
 *   shorter than min_chunk nor than a chunk's header and four records of
 *   record bytes (0 for chunks of no records), which win over max_chunk.
 *
 * TessSpillLevel is a level about to be made: the bytes it expects
 * (tess_spill_expected for the first level, the bytes of the partition it
 * splits for a level below), its limit (hash_mem or what the levels above
 * leave of it), the shift of its bits (0 for the first level, the plan's
 * below of the level above for one below it), at_least partitions (0 for
 * none), and the partitions another process planned for it, which it
 * takes as they are (0 to plan them).
 */
typedef struct TessSpillShape
{
	Size		reserve;
	Size		share;
	Size		min_chunk;
	Size		max_chunk;
	Size		record;
	uint32		min_partitions;
	uint32		max_partitions;
} TessSpillShape;

typedef struct TessSpillLevel
{
	double		expected;
	Size		limit;
	uint32		shift;
	uint32		at_least;
	uint32		partitions;
} TessSpillLevel;

/*
 * The plan of a level: its partitions; the length of its chunks, a
 * multiple of 8; the spill words that count its partitions
 * (tess_table_spill_init); the first bit of the hash it takes, its level's
 * shift, below 32; the shift of a level below; and whether a level of its
 * shape fits below, its least partitions, two or more, in the bits left. A
 * shape of one partition takes no bits and has no level below.
 */
typedef struct TessSpillPlan
{
	Size		chunk_len;
	Size		words;
	uint32		partitions;
	uint32		shift;
	uint32		below;
	bool		deeper;
} TessSpillPlan;

/*
 * The plan of a level into plan. The partitions start from min_partitions
 * and double while they are fewer than max_partitions; fewer than at_least,
 * or, each holding half of the limit, hold less than the level expects;
 * each reserve of twice as many fits in half of the limit; and the bits of
 * twice as many, after shift, stay below 32. Refused, SQLSTATE XX000, with
 * nothing written: least and most partitions that are not powers of two in
 * order, expected bytes below zero or not a number, a shift of 32 or more or
 * one past which the least partitions do not fit in the hash, given
 * partitions that are no
 * power of two of the shape or pass the hash, a share of 0, a least chunk
 * not a multiple of 8, partitions past those spill words count, and a null
 * argument (openspec/specs/hash-table/).
 */
extern TessStatusCode tess_spill_plan(const TessSpillShape *shape,
									  const TessSpillLevel *level,
									  TessSpillPlan *plan, TessStatus *status);

/*
 * The bytes the first level of a spill expects: twice the held bytes of
 * the rows so far, or what estimate rows would take at as many bytes a
 * row, if more; for each of participants that build one table.
 */
extern double tess_spill_expected(double held, double rows, double estimate,
								  uint32 participants);

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
 * bytes, as datumCopy copies them, copied into values at *values_used,
 * which moves past them to a multiple of 8; values_len is at most 4 GiB. It stops when the chunk is full or the next row's values
 * would pass values_len: the rows appended leave rows, *appended gets
 * their count and *need the bytes of values the next row takes (0 when
 * every row went), for the caller to make room. An expanded object is
 * refused, as its bytes are a pointer of this process: the caller
 * flattens it first (tess_value_flatten_column).
 */
extern TessStatusCode tess_spill_columns_append(void *chunk, Size len, int ncolumns,
												const TessDatumColumn *columns,
												const bool *byvals, const int16 *typlens,
												TessRowMask *rows, char *values,
												Size values_len, Size *values_used,
												int *appended, Size *need,
												TessStatus *status);

/*
 * A chunk of columns is written as a pair of blocks under one number: its
 * values (TESS_SPILL_VALUES), then its rows (TESS_SPILL_COLUMNS); a chunk
 * of no values as its rows alone. Whether header, read back as block at of
 * the pair (0 the values, or the rows of a chunk of no values; 1 the
 * rows), numbered number, is that block: one of another kind or number is
 * damaged data (SQLSTATE XX001), an at past the pair a wrong call.
 */
extern TessStatusCode tess_spill_columns_pair(const TessSpillHeader *header, int at,
											  uint32 number, TessStatus *status);

/*
 * Check the chunk of columns of len bytes at chunk, read back with its
 * chunk of values_len bytes at values, once and in one pass, so that its
 * rows are read unchecked after: it holds words words and, unless rows is
 * -1, rows rows; and every word of its first ncolumns columns not by value
 * (byvals) of a row that is not NULL refers into the values, to a byte or
 * to their end, where an empty value lies, and the value there ends inside
 * them by its type's length (typlens; -1 a varlena, -2 a C string, as
 * tess_spill_value_fits tells). Any of these that fails, or a chunk that
 * is no chunk of columns, is damaged data (SQLSTATE XX001); ncolumns past
 * words or rows below -1 is a wrong call.
 */
extern TessStatusCode tess_spill_columns_check(const void *chunk, Size len, int words,
											   int64 rows, int ncolumns, const bool *byvals,
											   const int16 *typlens, const char *values,
											   Size values_len, TessStatus *status);

/*
 * The words of the first ncolumns words of the chunk of columns of len
 * bytes at chunk, checked with values by tess_spill_columns_check, for its
 * count rows from first on: column c as Datums into out[c], a word of a
 * column not byvals[c] of a row not NULL as a pointer to its value in
 * values, a NULL's as 0, and whether each is NULL into isnull[c]. Words
 * past the chunk's words or rows past its rows are a wrong call.
 */
extern TessStatusCode tess_spill_columns_window(const void *chunk, Size len, int ncolumns,
												const bool *byvals, const char *values,
												Size first, Size count, Datum *const *out,
												bool *const *isnull, TessStatus *status);

/*
 * Append the rows of pending to the chunks of their partitions, as
 * tess_table_append_partitioned_columns appends records: the partition of
 * a hash is (hash >> shift) & (npartitions - 1) and appends to chunk
 * partition_chunks[p] of the nchunks at bases, of lens bytes; a row whose
 * chunk is full stays pending. Appended rows get their places in offsets
 * and add one to rows[partition]. A row's words are its ncolumns columns,
 * ncolumns at most 2048, with their NULL bits in the chunk's lanes.
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

/*
 * The writer of a set's file (crates/relspill, set): it lays the blocks
 * of every partition out in a write buffer, keeps a list of each
 * partition's blocks, and at the finish of a shared set the lists and the
 * trailer after the blocks, and writes nothing itself. Each call returns
 * a step: the ranges of the buffer, the scratch region, the block's body
 * or a list to write to the file, in order, and where; or the bytes of
 * scratch or the entries of a list it needs first. The caller writes the
 * ranges, calls tess_spill_set_written, and calls again until a step is
 * done, changing none of the regions in between. The writer's words are
 * the caller's; tess_spill_set_init makes them. The regions are
 * initialized bytes: the runtime allocates them zeroed. A write that
 * fails raises an ERROR, which ends the query and frees the set with its
 * memory, so the runtime never reports a failed write to the writer.
 */
#define TESS_SPILL_WRITER_WORDS 21
#define TESS_SPILL_MAX_RANGES 3

typedef struct TessSpillWriter
{
	uint64		words[TESS_SPILL_WRITER_WORDS];
} TessSpillWriter;

/* Where a block lies in the file: its header's start, the bytes it takes. */
typedef struct TessSpillEntry
{
	uint64		offset;
	uint64		stored;
} TessSpillEntry;

/* A partition's block list: slots entries, the first count its blocks. */
typedef struct TessSpillList
{
	TessSpillEntry *entries;
	uint64		count;
	uint64		slots;
} TessSpillList;

typedef enum TessSpillRegion
{
	TESS_SPILL_REGION_BUFFER = 0,
	TESS_SPILL_REGION_SCRATCH = 1,
	TESS_SPILL_REGION_BODY = 2,
	/* The entries of the list of the range's partition, as bytes. */
	TESS_SPILL_REGION_LIST = 3
} TessSpillRegion;

/* len bytes of a region from byte start, to the file at byte at. */
typedef struct TessSpillRange
{
	uint32		region;
	uint32		partition;
	uint64		start;
	uint64		len;
	uint64		at;
} TessSpillRange;

typedef enum TessSpillStepKind
{
	/* Write the ranges, report them, and call again. */
	TESS_SPILL_STEP_AGAIN = 0,
	/* Write the ranges, report them if any: the call is done. */
	TESS_SPILL_STEP_DONE = 1,
	/* Give a scratch region of need bytes and call again. */
	TESS_SPILL_STEP_SCRATCH = 2,
	/* Give the block's partition a list of need entries and call again. */
	TESS_SPILL_STEP_LIST = 3
} TessSpillStepKind;

typedef struct TessSpillStep
{
	uint32		kind;
	uint32		nranges;
	uint64		need;
	/* Of a block written: where it starts, the bytes it takes. */
	uint64		offset;
	uint64		stored;
	TessSpillRange ranges[TESS_SPILL_MAX_RANGES];
} TessSpillStep;

/*
 * Make *writer the writer of an empty set of npartitions partitions at
 * level, its headers carrying fingerprint, its chunks of records and of
 * columns of at most max_chunk body bytes and its chunks of values of at
 * most max_values, through a write buffer of buffer_len bytes, a multiple
 * of 8 of at least TESS_SPILL_HEADER_SIZE + 64; a shared set's finish
 * writes its lists and trailer.
 */
extern TessStatusCode tess_spill_set_init(TessSpillWriter *writer, int npartitions,
										  uint32 level, uint64 fingerprint,
										  uint64 max_chunk, uint64 max_values,
										  Size buffer_len, bool shared,
										  TessStatus *status);

/*
 * A step of writing the block of kind, number and partition whose len
 * bytes are at body: buffer is the set's write buffer, scratch a region
 * the writer packs a block into aside (NULL and 0 until a step asks),
 * lists the npartitions block lists.
 */
extern TessStatusCode tess_spill_set_write(TessSpillWriter *writer, void *buffer,
										   Size buffer_len, void *scratch,
										   Size scratch_len, TessSpillList *lists,
										   int npartitions, int partition,
										   TessSpillKind kind, uint32 number,
										   const void *body, Size len,
										   TessSpillStep *step, TessStatus *status);

/* The ranges of the last step were written. */
extern TessStatusCode tess_spill_set_written(TessSpillWriter *writer,
											 TessStatus *status);

/*
 * A step of finishing the set: what the buffer holds and, for a shared set
 * that wrote a block, the lists and the trailer. Once the step that is
 * done is written and reported, the set takes no more blocks and needs its
 * buffer no more.
 */
extern TessStatusCode tess_spill_set_finish(TessSpillWriter *writer, void *buffer,
											Size buffer_len, TessSpillList *lists,
											int npartitions, TessSpillStep *step,
											TessStatus *status);

/*
 * Forget the blocks of partition: its list's count goes to 0. A shared set
 * drops a partition only once it is finished.
 */
extern TessStatusCode tess_spill_set_drop(TessSpillWriter *writer, TessSpillList *lists,
										  int npartitions, int partition,
										  TessStatus *status);

/*
 * The blocks and bytes written, the partitions with blocks, and the
 * longest body of values written: a reader's max_values. Each output may
 * be NULL.
 */
extern TessStatusCode tess_spill_set_stats(const TessSpillWriter *writer,
										   uint64 *blocks, uint64 *bytes, int *files,
										   uint64 *values, TessStatus *status);

/*
 * The reader of a partition of a set (crates/relspill, set): it reads no
 * byte itself. Of the caller's own set it takes the partition's list in
 * memory; of another participant's file the file's length, and its open
 * asks for the trailer, the counts and the partition's entries, a page
 * at a time, each checked before what it names is asked for. Each step
 * asks for bytes of the file to read into the page or into the block
 * buffer, from their starts, after which the caller calls again; or says
 * what it found. The reader's words are the caller's, made by
 * tess_spill_reader_own or tess_spill_reader_file.
 */
#define TESS_SPILL_READER_WORDS 28

typedef struct TessSpillReadState
{
	uint64		words[TESS_SPILL_READER_WORDS];
} TessSpillReadState;

typedef enum TessSpillReadStepKind
{
	/* Read len bytes at at into the page, from its start; call again. */
	TESS_SPILL_READ_PAGE = 0,
	/* Read len bytes at at into the block buffer, from its start; call again. */
	TESS_SPILL_READ_BLOCK = 1,
	/* The partition has no block. */
	TESS_SPILL_READ_EMPTY = 2,
	/* The open is done: the block buffer must hold largest bytes. */
	TESS_SPILL_READ_READY = 3,
	/* The next block's header, checked; its body waits in the block buffer. */
	TESS_SPILL_READ_HEADER = 4,
	/* Every block of the partition was read. */
	TESS_SPILL_READ_END = 5,
	/* A seek found its block. */
	TESS_SPILL_READ_FOUND = 6
} TessSpillReadStepKind;

typedef struct TessSpillReadStep
{
	uint32		kind;
	uint32		pad;
	uint64		at;
	uint64		len;
	uint64		largest;
	TessSpillHeader header;
} TessSpillReadStep;

/*
 * A reader of a partition of the caller's own set, whose list is the count
 * entries at entries, its blocks within max_chunk and max_values as
 * tess_spill_set_init takes them; *largest gets the bytes of its largest
 * block.
 */
extern TessStatusCode tess_spill_reader_own(TessSpillReadState *reader, int npartitions,
											uint32 level, int partition,
											uint64 fingerprint, uint64 max_chunk,
											uint64 max_values,
											const TessSpillEntry *entries, uint64 count,
											uint64 *largest, TessStatus *status);

/*
 * A reader of a partition of another participant's file of file_len
 * bytes, with the bounds of tess_spill_reader_own.
 */
extern TessStatusCode tess_spill_reader_file(TessSpillReadState *reader, int npartitions,
											 uint32 level, int partition,
											 uint64 fingerprint, uint64 max_chunk,
											 uint64 max_values, uint64 file_len,
											 TessStatus *status);

/* A step of opening another participant's file; page holds page_len entries. */
extern TessStatusCode tess_spill_reader_open(TessSpillReadState *reader,
											 const TessSpillEntry *page, Size page_len,
											 TessSpillReadStep *step, TessStatus *status);

/*
 * A step of reading the next block: page is the page, or the own set's
 * list, and block the block buffer.
 */
extern TessStatusCode tess_spill_reader_next(TessSpillReadState *reader,
											 const TessSpillEntry *page, Size page_len,
											 const void *block, Size block_len,
											 TessSpillReadStep *step, TessStatus *status);

/* The body of the block whose header the last step gave, into the len bytes at body. */
extern TessStatusCode tess_spill_reader_body(TessSpillReadState *reader,
											 const void *block, Size block_len,
											 void *body, Size len, TessStatus *status);

/* A step of going to the block a write placed at byte offset of the file. */
extern TessStatusCode tess_spill_reader_seek(TessSpillReadState *reader,
											 const TessSpillEntry *page, Size page_len,
											 uint64 offset, TessSpillReadStep *step,
											 TessStatus *status);

/* Sizes and offsets of the mirrors above, for layout checks. */
typedef enum TessSpillSetLayout
{
	TESS_SPILL_SET_WRITER_SIZE = 0,
	TESS_SPILL_SET_ENTRY_SIZE = 1,
	TESS_SPILL_SET_LIST_SIZE = 2,
	TESS_SPILL_SET_LIST_COUNT_OFFSET = 3,
	TESS_SPILL_SET_RANGE_SIZE = 4,
	TESS_SPILL_SET_STEP_SIZE = 5,
	TESS_SPILL_SET_STEP_RANGES_OFFSET = 6,
	TESS_SPILL_SET_READER_SIZE = 7,
	TESS_SPILL_SET_READ_STEP_SIZE = 8,
	TESS_SPILL_SET_READ_STEP_HEADER_OFFSET = 9,
	TESS_SPILL_SET_SHAPE_SIZE = 10,
	TESS_SPILL_SET_SHAPE_MAX_PARTITIONS_OFFSET = 11,
	TESS_SPILL_SET_LEVEL_SIZE = 12,
	TESS_SPILL_SET_LEVEL_PARTITIONS_OFFSET = 13,
	TESS_SPILL_SET_PLAN_SIZE = 14,
	TESS_SPILL_SET_PLAN_DEEPER_OFFSET = 15
} TessSpillSetLayout;

/* The size or offset for a kind, or 0 for an unknown one. */
extern Size tess_spill_set_layout(TessSpillSetLayout what);

/*
 * The machine of a join's spill (relspill's join): the protocol of a
 * table that outgrows its memory, which the join steps with what it saw
 * and which answers with what the join does next. A machine is plain
 * words, TESS_JOIN_WORDS of them, which the join keeps for each level of
 * its spill; a step checks them, and words that are no machine are
 * refused. The calls are refused, SQLSTATE XX000, with the machine as it
 * was: an event the machine's phase does not take, a partition past the
 * level's, regions that are not the level's, and a null argument.
 */
#define TESS_JOIN_WORDS 31

typedef struct TessJoinMachine
{
	uint64		words[TESS_JOIN_WORDS];
} TessJoinMachine;

/*
 * What every level of a join's spill shares: the memory limit, the shape
 * of its levels' inner side, the words an outer row stores in a chunk of
 * columns besides its NULL bits, whether the outer rows keep values by
 * reference (1) or not (0), whether the join gives out the inner rows
 * without a pair (1, RIGHT and FULL) or not (0), whether it gives out the
 * outer rows without a pair (1, LEFT, FULL and ANTI) or not (0), a spare
 * word of 0, the bytes of a file's buffer, the weights of the rule that sends partitions to disk
 * (TessSpillWeights), with the share of the outer tail of a partition on
 * disk counted in reserve, and those of the rule that splits a partition
 * into a level below (tess_table_spill_splits).
 */
typedef struct TessJoinParams
{
	uint64		limit;
	TessSpillShape shape;
	uint64		outer_words;
	uint32		outer_values;
	uint32		preserve_inner;
	uint32		unmatched_outer;
	uint32		spare;
	uint64		page;
	double		start;
	double		target;
	double		spilled;
	double		resident;
	double		tail;
	double		split_room;
	double		split_key;
} TessJoinParams;

/*
 * The regions of a level a step reads: the level's spill words, nwords of
 * them, whose bytes the join counts by partition; the inner rows of each
 * of its nrows partitions, and of the level; the outer rows written of
 * each, and the bytes of the inner side's blocks on disk of each; and a
 * byte for each of its nroutes partitions, which the end of the build
 * sets to a TessJoinRoute. Before a level is made, all are empty.
 */
typedef struct TessJoinHost
{
	uint64	   *words;
	Size		nwords;
	const uint64 *rows;
	Size		nrows;
	uint64		total;
	const uint64 *outer;
	Size		nouter;
	const uint64 *disk;
	Size		ndisk;
	uint8	   *routes;
	Size		nroutes;
} TessJoinHost;

typedef enum TessJoinRoute
{
	/* In memory: the outer rows probe the table now. */
	TESS_JOIN_ROUTE_PROBE = 0,
	/* On disk: the outer rows are written by their partition. */
	TESS_JOIN_ROUTE_WRITE = 1,
	/* On disk without an inner row: the outer rows have no pair. */
	TESS_JOIN_ROUTE_ANSWER = 2
} TessJoinRoute;

typedef enum TessJoinEventKind
{
	/*
	 * A batch of inner rows is about to go into the table in memory, which
	 * holds held bytes in rows rows of the estimate the join expects; with
	 * the batch it would take memory bytes.
	 */
	TESS_JOIN_INNER = 1,
	/* The level of the last TESS_JOIN_START is made, with its words. */
	TESS_JOIN_MADE = 2,
	/*
	 * A partition of side (0 inner, 1 outer) has no room for its next row,
	 * or its values passed a chunk; flag 1 when it holds a chunk of rows.
	 */
	TESS_JOIN_FULL = 3,
	/* Rows went into the partitions: the join's memory now. */
	TESS_JOIN_GROWN = 4,
	/*
	 * The partition of the last TESS_JOIN_DEMOTE or TESS_JOIN_EVICT went
	 * to disk: flag 1 when that freed memory; the memory now.
	 */
	TESS_JOIN_SENT = 5,
	/* The inner rows are read. */
	TESS_JOIN_BUILD_END = 6,
	/* The outer rows are read. */
	TESS_JOIN_OUTER_END = 7,
	/*
	 * The join asks for the next partition on disk; memory is what its
	 * spill holds, the levels above included.
	 */
	TESS_JOIN_NEXT = 8,
	/*
	 * A participant of a shared build took partition to join alone;
	 * memory as for TESS_JOIN_NEXT.
	 */
	TESS_JOIN_TAKE = 9,
	/* The join starts the next piece of a partition; memory as for TESS_JOIN_NEXT. */
	TESS_JOIN_PIECE = 10,
	/*
	 * The header of the partition's next block of inner rows: flag 1 for a
	 * block of values, of len bytes.
	 */
	TESS_JOIN_BLOCK = 11,
	/* The partition's inner rows on disk are all read. */
	TESS_JOIN_FILE_END = 12,
	/* A pass over the partition's outer rows ended. */
	TESS_JOIN_PASS_END = 13,
	/*
	 * The outer rows of the table in memory are read: the level's table of
	 * the partitions in memory, a partition's or a piece's; flag 1 when the
	 * join still holds output that refers to the table.
	 */
	TESS_JOIN_TABLE_END = 14,
	/* The tail of the last TESS_JOIN_TAIL went out. */
	TESS_JOIN_TAIL_END = 15
} TessJoinEventKind;

typedef struct TessJoinEvent
{
	uint32		kind;
	uint32		side;
	uint32		partition;
	uint32		flag;
	uint64		held;
	uint64		rows;
	double		estimate;
	uint64		memory;
	uint64		len;
} TessJoinEvent;

typedef enum TessJoinActionKind
{
	/* Nothing more: the event is done. */
	TESS_JOIN_DONE = 0,
	/*
	 * The table spills: make the level of plan, with chunks of outer rows
	 * of outer_len bytes, then report TESS_JOIN_MADE.
	 */
	TESS_JOIN_START = 1,
	/* Give the partition of the event a new chunk. */
	TESS_JOIN_ADD_CHUNK = 2,
	/* Write what the partition of the event holds; its chunk takes the next rows. */
	TESS_JOIN_FLUSH = 3,
	/*
	 * Send partition, in memory, to disk, keeping its last chunk, emptied,
	 * as its tail; then report TESS_JOIN_SENT.
	 */
	TESS_JOIN_DEMOTE = 4,
	/* Partition, on disk, writes and frees what it holds; then TESS_JOIN_SENT. */
	TESS_JOIN_EVICT = 5,
	/* Partition has nothing to join: free it and its files, then TESS_JOIN_NEXT. */
	TESS_JOIN_RELEASE = 6,
	/*
	 * Join partition: whole, then TESS_JOIN_NEXT; or, with pieces, a piece
	 * at a time, each from TESS_JOIN_PIECE.
	 */
	TESS_JOIN_OPEN = 7,
	/* Split partition into the level below, its machine by tess_join_below. */
	TESS_JOIN_SPLIT = 8,
	/* Every partition of the level is done. */
	TESS_JOIN_LEVEL_DONE = 9,
	/* Read the block into the piece. */
	TESS_JOIN_LOAD = 10,
	/*
	 * The piece ends before the block, which the next piece starts with:
	 * probe the piece with the partition's outer rows, then
	 * TESS_JOIN_PASS_END.
	 */
	TESS_JOIN_CARRY = 11,
	/* Drop the piece and start the next with TESS_JOIN_PIECE. */
	TESS_JOIN_NEXT_PIECE = 12,
	/*
	 * Drop the piece and read the partition's outer rows once more,
	 * without a table, giving out those no piece paired; then
	 * TESS_JOIN_PASS_END.
	 */
	TESS_JOIN_FINAL_PASS = 13,
	/* The table may not go yet: give out the output held, then TESS_JOIN_TABLE_END again. */
	TESS_JOIN_HOLD = 14,
	/*
	 * Give out the table's inner rows without a pair, then
	 * TESS_JOIN_TAIL_END and TESS_JOIN_TABLE_END again.
	 */
	TESS_JOIN_TAIL = 15
} TessJoinActionKind;

/*
 * A level below to make: the partition of the level above it splits, its
 * level, the bytes it expects, the memory it may take, and the first bit
 * of the hash it takes. spare is 0.
 */
typedef struct TessJoinBelow
{
	uint32		partition;
	uint32		level;
	uint64		expected;
	uint64		limit;
	uint32		shift;
	uint32		spare;
} TessJoinBelow;

typedef struct TessJoinAction
{
	uint32		kind;
	uint32		partition;
	uint32		pieces;
	uint32		spare;
	Size		outer_len;
	TessSpillPlan plan;
	TessJoinBelow below;
} TessJoinAction;

/* Make *machine the machine of a join whose table fits so far. */
extern TessStatusCode tess_join_init(TessJoinMachine *machine, const TessJoinParams *params,
									 TessStatus *status);

/*
 * Make *machine the machine of the level below a TESS_JOIN_SPLIT asked
 * for; the level to make, a TESS_JOIN_START, into *action.
 */
extern TessStatusCode tess_join_below(TessJoinMachine *machine, const TessJoinParams *params,
									  const TessJoinBelow *below, TessJoinAction *action,
									  TessStatus *status);

/*
 * Make *machine the machine of the level of a build several processes
 * share, whose partitions one of them planned; the level to make, a
 * TESS_JOIN_START, into *action. The shared build decides the level, but
 * for whether a partition a participant takes alone splits
 * (TESS_JOIN_TAKE).
 */
extern TessStatusCode tess_join_given(TessJoinMachine *machine, const TessJoinParams *params,
									  uint32 partitions, TessJoinAction *action,
									  TessStatus *status);

/* Give *machine the join's event and its next action into *action. */
extern TessStatusCode tess_join_step(TessJoinMachine *machine, const TessJoinHost *host,
									 const TessJoinEvent *event, TessJoinAction *action,
									 TessStatus *status);

/* What tess_join_layout gives the size or offset of. */
typedef enum TessJoinLayout
{
	TESS_JOIN_MACHINE_SIZE = 1,
	TESS_JOIN_PARAMS_SIZE = 2,
	TESS_JOIN_PARAMS_TAIL_OFFSET = 3,
	TESS_JOIN_HOST_SIZE = 4,
	TESS_JOIN_HOST_NROUTES_OFFSET = 5,
	TESS_JOIN_EVENT_SIZE = 6,
	TESS_JOIN_EVENT_MEMORY_OFFSET = 7,
	TESS_JOIN_ACTION_SIZE = 8,
	TESS_JOIN_ACTION_PLAN_OFFSET = 9,
	TESS_JOIN_ACTION_BELOW_OFFSET = 10,
	TESS_JOIN_BELOW_SIZE = 11,
	TESS_JOIN_BELOW_SHIFT_OFFSET = 12,
	TESS_JOIN_EVENT_LEN_OFFSET = 13,
	TESS_JOIN_ACTION_OUTER_LEN_OFFSET = 14
} TessJoinLayout;

/* The size or offset for a kind, or 0 for an unknown one. */
extern Size tess_join_layout(TessJoinLayout what);

/*
 * The machine of a grouping's spill (relspill's group): the protocol of a
 * table of groups that outgrows its memory, which the grouping steps with
 * what it saw. A machine is plain words, TESS_GROUP_WORDS of them, one for
 * each level of the spill; its calls are refused as the join machine's
 * are.
 */
#define TESS_GROUP_WORDS 20

typedef struct TessGroupMachine
{
	uint64		words[TESS_GROUP_WORDS];
} TessGroupMachine;

/*
 * What every level of a grouping's spill shares: the memory limit, every
 * level's; the shape of its levels; the bytes a group takes merged, its
 * record and its share of an index; the eighths of the limit past which
 * the table is full (limit / 8 * fill bytes); the weights of the rule that
 * sends partitions to disk; and the share of what the limit leaves a
 * partition must pass to split.
 */
typedef struct TessGroupParams
{
	uint64		limit;
	TessSpillShape shape;
	uint64		merged;
	uint64		fill;
	double		start;
	double		target;
	double		spilled;
	double		split_room;
} TessGroupParams;

/*
 * The regions of a level a step reads: its spill words, the records in
 * memory of each of its partitions, and the bytes of each one's blocks on
 * disk. Before a level is made, all are empty.
 */
typedef struct TessGroupHost
{
	uint64	   *words;
	Size		nwords;
	const uint64 *records;
	Size		nrecords;
	const uint64 *disk;
	Size		ndisk;
} TessGroupHost;

typedef enum TessGroupEventKind
{
	/*
	 * The table in memory takes bytes in records groups of the estimate
	 * the grouping expects; flag 1 spills it whatever the bytes.
	 */
	TESS_GROUP_INNER = 1,
	/* The level of the last TESS_GROUP_START is made, with its words. */
	TESS_GROUP_MADE = 2,
	/* A partition's chunk has no room; flag 1 when it holds one. */
	TESS_GROUP_FULL = 3,
	/* Records went into the partitions: the memory now. */
	TESS_GROUP_GROWN = 4,
	/* The partition of the last TESS_GROUP_WRITE went to disk: the memory now. */
	TESS_GROUP_SENT = 5,
	/* The input is read. */
	TESS_GROUP_INPUT_END = 6,
	/* The next partition to give out, or to write at the end of the input. */
	TESS_GROUP_NEXT = 7,
	/*
	 * The partition of the last TESS_GROUP_WEIGH holds about groups groups;
	 * the other partitions in memory take others bytes.
	 */
	TESS_GROUP_WEIGHED = 8,
	/*
	 * A partial grouping's groups take memory bytes in records groups, of
	 * rows rows read since its table started; flag 1 when they can spill.
	 */
	TESS_GROUP_PARTIAL = 9
} TessGroupEventKind;

typedef struct TessGroupEvent
{
	uint32		kind;
	uint32		partition;
	uint32		flag;
	uint32		spare;
	uint64		bytes;
	uint64		records;
	double		estimate;
	uint64		memory;
	uint64		groups;
	uint64		others;
	uint64		rows;
} TessGroupEvent;

typedef enum TessGroupActionKind
{
	TESS_GROUP_DONE = 0,
	/* The table spills: make the level of plan, then TESS_GROUP_MADE. */
	TESS_GROUP_START = 1,
	/* Give the partition of the event a new chunk. */
	TESS_GROUP_ADD_CHUNK = 2,
	/* Write the partition's full chunk and fill it again. */
	TESS_GROUP_FLUSH = 3,
	/*
	 * Write and free every chunk partition holds in memory; then
	 * TESS_GROUP_SENT, or at the end of the input TESS_GROUP_NEXT.
	 */
	TESS_GROUP_WRITE = 4,
	/* Give partition out, its records on disk merged in; then TESS_GROUP_NEXT. */
	TESS_GROUP_MERGE = 5,
	/* Estimate partition's groups, then TESS_GROUP_WEIGHED. */
	TESS_GROUP_WEIGH = 6,
	/* Split partition into the level below, its machine by tess_group_below. */
	TESS_GROUP_SPLIT = 7,
	/* Every partition of the level is given out. */
	TESS_GROUP_LEVEL_DONE = 8,
	/* The partial grouping's table is full: its groups go up, a new one starts. */
	TESS_GROUP_EMIT = 9,
	/* The partial grouping's table spills instead: TESS_GROUP_INNER with flag 1. */
	TESS_GROUP_SPILL = 10
} TessGroupActionKind;

/*
 * A level below to make: the partition of the level above it splits, its
 * level, the bytes it expects and the first bit of the hash it takes.
 * spare is 0.
 */
typedef struct TessGroupBelow
{
	uint32		partition;
	uint32		level;
	uint64		expected;
	uint32		shift;
	uint32		spare;
} TessGroupBelow;

typedef struct TessGroupAction
{
	uint32		kind;
	uint32		partition;
	TessSpillPlan plan;
	TessGroupBelow below;
} TessGroupAction;

/* Make *machine the machine of a grouping whose table fits so far. */
extern TessStatusCode tess_group_init(TessGroupMachine *machine, const TessGroupParams *params,
									  TessStatus *status);

/*
 * Make *machine the machine of the level below a TESS_GROUP_SPLIT asked
 * for; its plan, a TESS_GROUP_START, into *action.
 */
extern TessStatusCode tess_group_below(TessGroupMachine *machine,
									   const TessGroupParams *params,
									   const TessGroupBelow *below,
									   TessGroupAction *action, TessStatus *status);

/* Give *machine the grouping's event and its next action into *action. */
extern TessStatusCode tess_group_step(TessGroupMachine *machine, const TessGroupHost *host,
									  const TessGroupEvent *event, TessGroupAction *action,
									  TessStatus *status);

/* What tess_group_layout gives the size or offset of. */
typedef enum TessGroupLayout
{
	TESS_GROUP_MACHINE_SIZE = 1,
	TESS_GROUP_PARAMS_SIZE = 2,
	TESS_GROUP_PARAMS_SPLIT_ROOM_OFFSET = 3,
	TESS_GROUP_HOST_SIZE = 4,
	TESS_GROUP_HOST_NDISK_OFFSET = 5,
	TESS_GROUP_EVENT_SIZE = 6,
	TESS_GROUP_EVENT_ROWS_OFFSET = 7,
	TESS_GROUP_ACTION_SIZE = 8,
	TESS_GROUP_ACTION_BELOW_OFFSET = 9,
	TESS_GROUP_BELOW_SIZE = 10,
	TESS_GROUP_BELOW_SHIFT_OFFSET = 11
} TessGroupLayout;

/* The size or offset for a kind, or 0 for an unknown one. */
extern Size tess_group_layout(TessGroupLayout what);

/*
 * The machine of a table of a grouping's rows and of the level of rows it
 * writes once frozen (relspill's Rows): TESS_ROWS_WORDS plain words. A
 * table's machine weighs its groups after each batch and freezes it,
 * planning the level its rows go to; kept with that level once written,
 * it gives the level's partitions to read.
 */
#define TESS_ROWS_WORDS 13

typedef struct TessRowsMachine
{
	uint64		words[TESS_ROWS_WORDS];
} TessRowsMachine;

/*
 * What every level of rows shares: the memory limit, the shape of a level
 * of rows, the eighths of the limit past which a table freezes, and the
 * groups it has at least then.
 */
typedef struct TessRowsParams
{
	uint64		limit;
	TessSpillShape shape;
	uint64		fill;
	uint64		least;
} TessRowsParams;

/* The rows written of each partition of a level of rows. */
typedef struct TessRowsHost
{
	const uint64 *written;
	Size		nwritten;
} TessRowsHost;

typedef enum TessRowsEventKind
{
	/* After a batch, the table's groups take memory bytes in records groups. */
	TESS_ROWS_GROWN = 1,
	/* The rows of the level the last TESS_ROWS_FREEZE planned are written. */
	TESS_ROWS_WRITTEN = 2,
	/* The next partition of the level to read. */
	TESS_ROWS_NEXT = 3
} TessRowsEventKind;

typedef struct TessRowsEvent
{
	uint32		kind;
	uint32		spare;
	uint64		memory;
	uint64		records;
} TessRowsEvent;

/*
 * A table of a partition of rows read back: the partition, the level a
 * freeze of it writes, its first hash bit, and whether no level fits
 * (1, the table never freezes) or one does (0).
 */
typedef struct TessRowsTable
{
	uint32		partition;
	uint32		level;
	uint32		shift;
	uint32		last;
} TessRowsTable;

typedef enum TessRowsActionKind
{
	TESS_ROWS_DONE = 0,
	/*
	 * The table takes no new group: its rows go to the partitions of a
	 * level of plan, numbered level, which the grouping makes.
	 */
	TESS_ROWS_FREEZE = 1,
	/* Read table's partition into a table of its own, its machine by tess_rows_read. */
	TESS_ROWS_READ = 2,
	/* Every partition of the level with rows is read. */
	TESS_ROWS_LEVEL_DONE = 3
} TessRowsActionKind;

typedef struct TessRowsAction
{
	uint32		kind;
	uint32		level;
	TessSpillPlan plan;
	TessRowsTable table;
} TessRowsAction;

/* Make *machine the machine of the table of a grouping's input. */
extern TessStatusCode tess_rows_init(TessRowsMachine *machine, const TessRowsParams *params,
									 TessStatus *status);

/* Make *machine the machine of the table of the partition a TESS_ROWS_READ gave. */
extern TessStatusCode tess_rows_read(TessRowsMachine *machine, const TessRowsParams *params,
									 const TessRowsTable *table, TessStatus *status);

/* Give *machine the grouping's event of its rows and its next action into *action. */
extern TessStatusCode tess_rows_step(TessRowsMachine *machine, const TessRowsHost *host,
									 const TessRowsEvent *event, TessRowsAction *action,
									 TessStatus *status);

/* What tess_rows_layout gives the size or offset of. */
typedef enum TessRowsLayout
{
	TESS_ROWS_MACHINE_SIZE = 1,
	TESS_ROWS_PARAMS_SIZE = 2,
	TESS_ROWS_PARAMS_LEAST_OFFSET = 3,
	TESS_ROWS_HOST_SIZE = 4,
	TESS_ROWS_EVENT_SIZE = 5,
	TESS_ROWS_ACTION_SIZE = 6,
	TESS_ROWS_ACTION_TABLE_OFFSET = 7
} TessRowsLayout;

/* The size or offset for a kind, or 0 for an unknown one. */
extern Size tess_rows_layout(TessRowsLayout what);

/*
 * The machine of a sort's runs (relspill's sort::Runs): TESS_RUNS_WORDS
 * plain words. It says when the rows in memory go to disk as a run, which
 * set and partition each run takes, which runs each pass of the merge
 * merges, and how many runs the last merge reads.
 */
#define TESS_RUNS_WORDS 10

typedef struct TessRunsMachine
{
	uint64		words[TESS_RUNS_WORDS];
} TessRunsMachine;

/*
 * What a sort's runs share: the memory limit at its start, by which its
 * blocks are planned; the bytes of a row in a run's block, what sorting a
 * row in memory takes besides it, and a chunk of rows in memory; the runs
 * a set holds; the most runs a merge reads (2 at least); the rows a block
 * holds at least; and 1 when the output is read backward too, else 0.
 */
typedef struct TessRunsParams
{
	uint64		limit;
	uint64		row;
	uint64		item;
	uint64		chunk;
	uint32		set_runs;
	uint32		max_merge;
	uint32		batch_rows;
	uint32		backward;
} TessRunsParams;

/* The rows and bytes of values of a run's block, and the runs a merge reads. */
typedef struct TessRunsBlocks
{
	uint32		rows;
	uint32		fan_in;
	uint64		values;
} TessRunsBlocks;

typedef enum TessRunsEventKind
{
	/*
	 * After a batch, the rows in memory take memory bytes, rows of them,
	 * and the sort may take limit bytes now.
	 */
	TESS_RUNS_GROWN = 1,
	/* The input ended, rows rows in memory. */
	TESS_RUNS_INPUT_END = 2,
	/* The next step of the merge. */
	TESS_RUNS_NEXT = 3,
	/* The output is read again from its start. */
	TESS_RUNS_RESCAN = 4
} TessRunsEventKind;

typedef struct TessRunsEvent
{
	uint32		kind;
	uint32		spare;
	uint64		memory;
	uint64		rows;
	uint64		limit;
} TessRunsEvent;

/*
 * Where a run is written: a new set (1), the one being written ending
 * first, or the set being written (0); and its partition of the set.
 */
typedef struct TessRunsPlace
{
	uint32		create;
	uint32		partition;
} TessRunsPlace;

typedef enum TessRunsActionKind
{
	TESS_RUNS_DONE = 0,
	/* Sort the rows in memory and write them as a run at place. */
	TESS_RUNS_RUN = 1,
	/* A pass starts: the set being written ends, the runs are the pass's. */
	TESS_RUNS_PASS = 2,
	/* Merge count runs of the pass from first into a new run at place. */
	TESS_RUNS_MERGE = 3,
	/* Run first of the pass goes on as it is. */
	TESS_RUNS_KEEP = 4,
	/* The pass ends: the runs it wrote and kept are the runs. */
	TESS_RUNS_PASS_END = 5,
	/* The set being written ends; the last merge reads runs runs, one by blocks. */
	TESS_RUNS_READ = 6
} TessRunsActionKind;

typedef struct TessRunsAction
{
	uint32		kind;
	uint32		first;
	uint32		count;
	uint32		runs;
	TessRunsPlace place;
} TessRunsAction;

/* Make *machine the machine of a sort's runs, and *blocks their blocks. */
extern TessStatusCode tess_runs_init(TessRunsMachine *machine, const TessRunsParams *params,
									 TessRunsBlocks *blocks, TessStatus *status);

/* Give *machine the sort's event and its next action into *action. */
extern TessStatusCode tess_runs_step(TessRunsMachine *machine, const TessRunsEvent *event,
									 TessRunsAction *action, TessStatus *status);

/* What tess_runs_layout gives the size or offset of. */
typedef enum TessRunsLayout
{
	TESS_RUNS_MACHINE_SIZE = 1,
	TESS_RUNS_PARAMS_SIZE = 2,
	TESS_RUNS_PARAMS_BACKWARD_OFFSET = 3,
	TESS_RUNS_BLOCKS_SIZE = 4,
	TESS_RUNS_EVENT_SIZE = 5,
	TESS_RUNS_ACTION_SIZE = 6,
	TESS_RUNS_ACTION_PLACE_OFFSET = 7,
	TESS_RUNS_EVENT_LIMIT_OFFSET = 8
} TessRunsLayout;

/* The size or offset for a kind, or 0 for an unknown one. */
extern Size tess_runs_layout(TessRunsLayout what);

#endif							/* TESSERA_SPILL_H */

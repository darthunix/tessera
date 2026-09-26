/* C entry points of the Rust hash table in memory the caller owns. */
#ifndef TESSERA_TABLE_H
#define TESSERA_TABLE_H

#include "postgres.h"

#include "tessera/abi.h"
#include "tessera/batch.h"
#include "tessera/row_mask.h"
#include "tessera/status.h"

/*
 * The table (crates/tessera-kernels, module table) is an index and the
 * chunks its records lie in, all memory the caller allocates: palloc'd in
 * a serial plan, dynamic shared memory in a parallel one. Every call gets
 * a TessTableRef: the index's address and length, and the addresses and
 * lengths of the chunks in this process, by number. Rust keeps nothing
 * between calls and allocates nothing: a record is addressed by its chunk
 * number and its place in the chunk, so the bytes mean the same in every
 * process whatever address a chunk has there. Every block must be aligned
 * to 8; tess_table_size says how many bytes an index for a capacity
 * needs, tess_table_create lays the index out, tess_table_chunk_init
 * makes a block an empty chunk, and every other call attaches anew,
 * checking the whole header, and checks every reference it follows, so a
 * corrupt table is a status, never a crash or a hang. See docs/table.md.
 *
 * Records are addressed by 32-bit references: a chunk number and a place
 * in units of 8 bytes; 0 is none. Records never move: a chunk fills and
 * the caller adds another, and when the records outgrow the buckets only
 * the index is made anew over the same chunks (tess_table_regrow).
 * A batch brings its hashes (from tess_int4_hash, tess_int8_hash and their
 * _next forms, which apply the NULL policy), its keys as Datum columns and
 * a row mask. An int8 inside the int4 range hashes as the int4, so int4
 * and int8 key columns meet in one table.
 *
 * Building has two steps: tess_table_append writes rows as records into a
 * chunk, without the index, and tess_table_link puts them into the
 * buckets. Several processes may append to chunks of their own and link
 * them at once over shared memory, and several may probe, but not both at
 * a time. Every status rule of tessera/kernels.h applies: outputs are
 * unspecified after a failure, and the caller reports the status with
 * ereport after the call returns.
 */

/* The format of the table this header describes. */
#define TESS_TABLE_FORMAT_VERSION 1

/* The most keys a record holds. */
#define TESS_TABLE_MAX_KEYS 16

/*
 * Chunks: at most TESS_TABLE_MAX_CHUNKS of at most TESS_TABLE_MAX_CHUNK_LEN
 * bytes each, a multiple of 8; the first TESS_TABLE_CHUNK_HEADER bytes of a
 * chunk count the bytes it uses.
 */
#define TESS_TABLE_MAX_CHUNKS 32768
#define TESS_TABLE_MAX_CHUNK_LEN (1024 * 1024)
#define TESS_TABLE_CHUNK_HEADER 8

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

/*
 * A table as this process sees it: the index, and the chunks by number.
 * index is NULL where a call uses only chunks (tess_table_append).
 */
typedef struct TessTableRef
{
	void	   *index;
	Size		index_len;
	/* nchunks addresses and lengths, chunk k at chunks[k]. */
	void	   *const *chunks;
	const Size *chunk_lens;
	int			nchunks;
} TessTableRef;

/* Counts of a table, for planning and EXPLAIN; the caller sets struct_size. */
typedef struct TessTableStats
{
	Size		struct_size;
	/* Records inserted. */
	uint64		records;
	/* Buckets of the table. */
	uint64		buckets;
	/* Bytes of the index in use: the header and the buckets. */
	uint64		bytes_used;
	/* Bytes of the index the table was created or regrown in. */
	uint64		region_len;
} TessTableStats;

#define TESS_TABLE_STATS_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessTableStats, region_len)

/*
 * A record as the table exposes it; the caller sets struct_size. keys and
 * payload point into the record's chunk and stay valid as long as it.
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
	TESS_TABLE_LAYOUT_RECORD_PAYLOAD_OFFSET = 7,
	TESS_TABLE_LAYOUT_REF_SIZE = 8,
	TESS_TABLE_LAYOUT_REF_NCHUNKS_OFFSET = 9
} TessTableLayoutKind;

/* The table format the library writes and accepts; must equal the header's. */
extern uint32 tess_table_format_version(void);

/* The size or offset for a kind, or 0 for an unknown one. */
extern Size tess_table_layout(TessTableLayoutKind kind);

/*
 * The bytes the index of a table of nkeys keys of the given kinds, a
 * payload of payload_size bytes per record and capacity records needs:
 * the header and the buckets, a multiple of 8.
 */
extern TessStatusCode tess_table_size(int nkeys,
									  const TessTableKeyKind *kinds,
									  Size payload_size,
									  uint64 capacity,
									  Size *size,
									  TessStatus *status);

/*
 * Create the index of an empty table for capacity records in the len
 * bytes at index, a multiple of 8 of at least tess_table_size. A table
 * holds more records than its capacity, in more chunks, but a lookup
 * grows slower past it; tess_table_regrow makes a larger index.
 */
extern TessStatusCode tess_table_create(void *index,
										Size len,
										int nkeys,
										const TessTableKeyKind *kinds,
										Size payload_size,
										uint64 capacity,
										TessStatus *status);

/* Check that table is a table of this format. */
extern TessStatusCode tess_table_attach(const TessTableRef *table,
										TessStatus *status);

/* The counts of the table as of now. */
extern TessStatusCode tess_table_stats(const TessTableRef *table,
									   TessTableStats *stats,
									   TessStatus *status);

/* Make the len bytes at base, aligned to 8, an empty chunk. */
extern TessStatusCode tess_table_chunk_init(void *base, Size len,
											TessStatus *status);

/*
 * Append the rows of pending as records to chunk `chunk` of table, in row
 * order, as long as whole records fit: each row appended leaves pending
 * and gets the reference of its record in offsets (one slot per row of
 * the batch); rows still pending need another chunk. hashes has one hash
 * per row; keys are the table's nkeys keys, of its kinds; payload is the
 * payload of every row one after another (payload_size bytes each, as
 * the table has), or NULL for zeros. The index is not read, so a shared
 * build appends before there is one; the records are not found until
 * linked. The caller is the chunk's one writer.
 */
extern TessStatusCode tess_table_append(const TessTableRef *table,
										int chunk,
										Size payload_size,
										const uint32 *hashes,
										int nkeys,
										const TessTableKey *keys,
										const uint8 *payload,
										TessRowMask *pending,
										uint32 *offsets,
										TessStatus *status);

/*
 * A table that spills keeps its records in partitions: the partition of a
 * hash is (hash >> shift) & (npartitions - 1), npartitions a power of two
 * up to 65536, and partition p appends to chunk partition_chunks[p]. The
 * buckets take the hash's high bits, so the first level of partitions
 * takes its low ones and each further level the bits above. See
 * docs/spill.md.
 *
 * Append the rows of pending as records, each to the chunk of its hash's
 * partition, in row order, as long as whole records fit there: as
 * tess_table_append, except that a row whose partition's chunk is full
 * stays pending while the rows after it go on; the caller gives those
 * partitions new chunks and calls again. The caller is the one writer of
 * every partition's chunk.
 */
extern TessStatusCode tess_table_append_partitioned(const TessTableRef *table,
													const uint32 *partition_chunks,
													int npartitions,
													uint32 shift,
													Size payload_size,
													const uint32 *hashes,
													int nkeys,
													const TessTableKey *keys,
													const uint8 *payload,
													TessRowMask *pending,
													uint32 *offsets,
													TessStatus *status);

/*
 * Copy the records of chunk `source` of a table of the key kinds and
 * payload size from byte *from on (TESS_TABLE_CHUNK_HEADER at first),
 * whole and in order, each to the chunk of its hash's partition, and move
 * *from past them: at most capacity records, their new references into
 * offsets and their hashes into hashes. It stops before a record
 * whose partition's chunk is full, *full receiving that partition, -1
 * otherwise; *count receives the records copied, 0 at the source's end.
 * The copies are not linked; the source must be none of the partitions'
 * chunks. For a table's first spill and a partition split further.
 */
extern TessStatusCode tess_table_split(const TessTableRef *table,
									   int nkeys,
									   const TessTableKeyKind *kinds,
									   Size payload_size,
									   const uint32 *partition_chunks,
									   int npartitions, uint32 shift,
									   int source, Size *from, int capacity,
									   uint32 *offsets, uint32 *hashes,
									   int *count, int *full,
									   TessStatus *status);

/*
 * Link the records of chunk `chunk` from byte *from on into the buckets
 * and move *from past them; *linked (unless NULL) receives how many.
 * *from starts at TESS_TABLE_CHUNK_HEADER. Equal keys make separate
 * records. Several processes may link chunks of their own at once. With
 * duplicates, each record once published also walks the rest of its
 * chain for its keys, and *duplicates receives how many records had keys
 * the table held already: of two records of one key exactly the one
 * linked later counts, so the sum over every participant is exact.
 */
extern TessStatusCode tess_table_link(const TessTableRef *table,
									  int chunk,
									  Size *from,
									  uint64 *linked,
									  uint64 *duplicates,
									  TessStatus *status);

/*
 * As tess_table_link, but each record right after one with the same keys
 * when the table holds one, so that a key's records lie next to each
 * other in its chain and tess_table_next_in_group steps through them;
 * *duplicates (unless NULL) receives how many linked records had keys
 * the table held already. A lookup per record, and one writer: not for a
 * table several processes build at once.
 */
extern TessStatusCode tess_table_link_grouped(const TessTableRef *table,
											  int chunk,
											  Size *from,
											  uint64 *linked,
											  uint64 *duplicates,
											  TessStatus *status);

/*
 * Find the first record of its chain with the hash, NULL bits and keys of
 * each row of rows: matches[row] receives its offset and found, a mask
 * this call fills whole, the rows that have one. The other records with
 * the same keys follow through tess_table_next_match, or
 * tess_table_next_in_group in a table linked by tess_table_link_grouped.
 */
extern TessStatusCode tess_table_probe(const TessTableRef *table,
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
extern TessStatusCode tess_table_next_match(const TessTableRef *table,
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
extern TessStatusCode tess_table_gather(const TessTableRef *table,
										const uint32 *offsets,
										const TessRowMask *rows,
										Size at,
										Datum *values,
										TessStatus *status);

/*
 * For each row of rows, replace offsets[row] by the record right after it
 * in its chain when that one has the same hash, NULL bits and keys, and
 * put the row in found; the others keep their offset. In a table linked
 * by tess_table_link_grouped this is the key's next record, in one step
 * instead of tess_table_next_match's walk down the chain.
 */
extern TessStatusCode tess_table_next_in_group(const TessTableRef *table,
											   uint32 *offsets,
											   const TessRowMask *rows,
											   TessRowMask *found,
											   TessStatus *status);

/* The record at an offset a call of this table returned. */
extern TessStatusCode tess_table_record(const TessTableRef *table,
										uint32 offset,
										TessTableRecord *record,
										TessStatus *status);

/*
 * What one writer alone may do, with no other call over the table at the
 * same time: grouping resolves rows to the record of their keys and
 * changes payloads in place, output walks the records, and the index is
 * made anew.
 */

/*
 * Give each row of pending the record of its keys, creating one with a
 * zero payload in chunk `chunk` where none exists, in row order, until the
 * chunk is full or the records reach half the buckets: resolved rows
 * leave pending and get their record references in offsets; the rows
 * whose record this call created form inserted, a mask this call fills
 * whole. Rows left pending need another chunk or, when the stats show
 * records at half the buckets, a larger index (tess_table_regrow).
 */
extern TessStatusCode tess_table_find_or_insert(const TessTableRef *table,
												int chunk,
												const uint32 *hashes,
												int nkeys,
												const TessTableKey *keys,
												TessRowMask *pending,
												uint32 *offsets,
												TessRowMask *inserted,
												TessStatus *status);

/*
 * A grouping that spills keeps its groups in partitions, as a join's
 * table does: resolve each row of pending to the record of its keys, as
 * tess_table_find_or_insert, but a new record goes to chunk
 * partition_chunks[(hash >> shift) & (npartitions - 1)]; a row whose
 * partition's chunk is full stays pending while the rows after it go on,
 * and every row stops once the records reach half the buckets. The
 * caller is the one writer of the table.
 */
extern TessStatusCode tess_table_find_or_insert_partitioned(const TessTableRef *table,
															const uint32 *partition_chunks,
															int npartitions,
															uint32 shift,
															const uint32 *hashes,
															int nkeys,
															const TessTableKey *keys,
															TessRowMask *pending,
															uint32 *offsets,
															TessRowMask *inserted,
															TessStatus *status);

/*
 * How a group's aggregate state merges into the state of the same group
 * in another record. The payload is a word of flags, bit i set once
 * aggregate i has a value, then a word per aggregate.
 */
typedef enum TessTableCombine
{
	/* Counts add; past the int8 range 22003 "bigint out of range". */
	TESS_TABLE_COMBINE_COUNT = 1,
	/* Sums add when both have a value, as counts do. */
	TESS_TABLE_COMBINE_SUM = 2,
	TESS_TABLE_COMBINE_MIN = 3,
	TESS_TABLE_COMBINE_MAX = 4
} TessTableCombine;

/* Where tess_table_combine stopped. */
typedef enum TessTableCombineStop
{
	TESS_TABLE_COMBINE_DONE = 0,
	/* A group the table lacks needs another chunk. */
	TESS_TABLE_COMBINE_CHUNK_FULL = 1,
	/* A group the table lacks needs a larger index (tess_table_regrow). */
	TESS_TABLE_COMBINE_INDEX_FULL = 2
} TessTableCombineStop;

/*
 * Merge the records of chunk `source` from byte *from on (at first
 * TESS_TABLE_CHUNK_HEADER), each the states of a group, a chunk a
 * grouping wrote to disk and read back, into the table: the record of the
 * same keys takes the states in as combines[i] says for aggregate i, and
 * a group the table lacks is copied whole to chunk `chunk` and linked.
 * *from moves past the records merged, *merged receives their count and
 * *stop why the call stopped. The source is none of the linked chunks.
 */
extern TessStatusCode tess_table_combine(const TessTableRef *table, int source,
										 Size *from, int chunk, int naggregates,
										 const TessTableCombine *combines,
										 int *merged, int *stop,
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
extern TessStatusCode tess_table_accumulate(const TessTableRef *table,
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
extern TessStatusCode tess_table_gather_key(const TessTableRef *table,
											const uint32 *offsets,
											const TessRowMask *rows,
											int key,
											Datum *values,
											bool *isnull,
											TessStatus *status);

/*
 * A Bloom filter of a table's records, which a join checks a batch of
 * probe rows against before it looks them up: a row the filter rejects
 * has no record with its hash. The filter is a buffer of words the caller
 * owns, a power of two of them, with no address inside, like the index.
 * Each hash sets four bits of one word; at 16 bits per record about one
 * absent key in a hundred gets through. See docs/table.md.
 */

/* The words of a filter for a table of records records. */
extern TessStatusCode tess_table_bloom_words(uint64 records, Size *nwords,
											 TessStatus *status);

/*
 * Clear the nwords words at words and set the bits of every record of the
 * table's chunks: no append may run at the same time, as for a walk.
 */
extern TessStatusCode tess_table_bloom(const TessTableRef *table,
									   uint64 *words, Size nwords,
									   TessStatus *status);

/*
 * Set the bits of the hash of every row of rows in the filter: for the
 * filter of a table that spills, filled as its rows come. hashes has a
 * hash per row.
 */
extern TessStatusCode tess_bloom_add(uint64 *words, Size nwords,
									 const uint32 *hashes,
									 const TessRowMask *rows,
									 TessStatus *status);

/*
 * The rows of rows whose hash has all its bits in the filter, into found,
 * which this call fills whole; hashes has a hash per row. rows and found
 * must not share their words.
 */
extern TessStatusCode tess_bloom_probe(const uint64 *words, Size nwords,
									   const uint32 *hashes,
									   const TessRowMask *rows,
									   TessRowMask *found,
									   TessStatus *status);

/*
 * A shared Bloom filter, next to a shared table: a state word (none,
 * building, ready), then the words of a filter. One participant clears
 * it before the others use it; the first that wants the filter builds it
 * alone and marks it ready, and the others check batches against it only
 * once it is ready. See docs/table.md.
 */

/* The words of a shared filter for a table of records records. */
extern TessStatusCode tess_bloom_shared_words(uint64 records, Size *nwords,
											  TessStatus *status);

/* Clear a shared filter, before any other participant uses it. */
extern TessStatusCode tess_bloom_shared_init(uint64 *words, Size nwords,
											 TessStatus *status);

/*
 * Build the shared filter from the table's records unless another
 * participant has claimed it: *built is true for the one that did. No
 * append may run at the same time.
 */
extern TessStatusCode tess_table_try_build_bloom(const TessTableRef *table,
												 uint64 *words, Size nwords,
												 bool *built,
												 TessStatus *status);

/* Whether the shared filter is built. */
extern TessStatusCode tess_bloom_shared_ready(uint64 *words, Size nwords,
											  bool *ready,
											  TessStatus *status);

/* tess_bloom_probe against a shared filter, which must be ready. */
extern TessStatusCode tess_bloom_shared_probe(uint64 *words, Size nwords,
											  const uint32 *hashes,
											  const TessRowMask *rows,
											  TessRowMask *found,
											  TessStatus *status);

/*
 * The phases of a shared build, which a participant steps through without
 * waiting itself: each step returns an action, the node performs it, and
 * a barrier operation's result goes into the next step. The barrier is
 * the core's Barrier; its waits stay in the node, since they may raise an
 * error. The phases, in the barrier's numbering, and the actions follow;
 * see docs/table.md.
 */
#define TESS_BUILD_BUILD		0
#define TESS_BUILD_FLUSH		1
#define TESS_BUILD_SIZE			2
#define TESS_BUILD_LINK			3
#define TESS_BUILD_OUTER		4
#define TESS_BUILD_PROBE		5
#define TESS_BUILD_FREE			6

typedef enum TessBuildAction
{
	/* BarrierAttach, and pass the phase it returns. */
	TESS_BUILD_ATTACH = 1,
	/* BarrierArriveAndWait, and pass whether it elected this participant. */
	TESS_BUILD_ARRIVE_AND_WAIT = 2,
	/*
	 * Append this participant's share of the inner side to chunks of its
	 * own, numbered by tess_build_take_chunk, and report the records.
	 */
	TESS_BUILD_DO_BUILD = 3,
	/*
	 * Create the index for every record appended, clear the filter and
	 * publish the chunks' directory, as the elected one.
	 */
	TESS_BUILD_DO_SIZE = 4,
	/* Link this participant's own chunks into the index. */
	TESS_BUILD_DO_LINK = 5,
	/* Probe; step again once done. */
	TESS_BUILD_DO_PROBE = 6,
	/* BarrierArriveAndDetach, and pass whether it was the last. */
	TESS_BUILD_ARRIVE_AND_DETACH = 7,
	/* BarrierDetach. */
	TESS_BUILD_DETACH = 8,
	/* Free the table, as the last to leave. */
	TESS_BUILD_DO_FREE = 9,
	/* Nothing is left to do. */
	TESS_BUILD_DONE = 10,
	/*
	 * Write this participant's chunks of the partitions on disk, when the
	 * table spilled, and finish its files.
	 */
	TESS_BUILD_DO_FLUSH = 11,
	/*
	 * Write this participant's share of the outer side to the partitions'
	 * files, when the table spilled: before any row goes out, so that no
	 * participant waits at a barrier once it returns rows.
	 */
	TESS_BUILD_DO_OUTER = 12,
	/* A round: make the partition's index, as the elected one. */
	TESS_BUILD_DO_ALLOCATE = 13,
	/* A round: load inner files taken one at a time, and link them. */
	TESS_BUILD_DO_LOAD = 14
} TessBuildAction;

/* A participant's own state: zeroed before its first step. */
typedef struct TessBuildParticipant
{
	uint32		phase;
	uint32		state;
	uint32		elected;
} TessBuildParticipant;

/*
 * The words of a build's shared counters: records appended, NULL columns,
 * chunks numbered, duplicates the links found.
 */
#define TESS_BUILD_COUNTER_WORDS 4

/* Clear a build's counters, before any participant attaches. */
extern TessStatusCode tess_build_counters_init(uint64 *counters,
											   TessStatus *status);

/*
 * Add the records a participant's build appended and the payload words it
 * saw a NULL in, before it arrives at the barrier.
 */
extern TessStatusCode tess_build_report(uint64 *counters, uint64 records,
										uint64 null_columns,
										TessStatus *status);

/* The number of a new chunk, unique among the build's participants. */
extern TessStatusCode tess_build_take_chunk(uint64 *counters, uint64 *number,
											TessStatus *status);

/*
 * Add the duplicates a participant's links found, before it arrives at
 * the barrier after linking.
 */
extern TessStatusCode tess_build_add_duplicates(uint64 *counters,
												uint64 duplicates,
												TessStatus *status);

/*
 * The totals of every participant, once the build is over; *duplicates
 * (unless NULL) once linking is over.
 */
extern TessStatusCode tess_build_totals(uint64 *counters, uint64 *records,
										uint64 *null_columns,
										uint64 *chunks,
										uint64 *duplicates,
										TessStatus *status);

/*
 * The participant's next action (a TessBuildAction) after the previous one
 * is done: reply is the phase BarrierAttach returned, or 1 if
 * BarrierArriveAndWait elected the participant or BarrierArriveAndDetach
 * found it the last, else 0.
 */
extern TessStatusCode tess_build_step(TessBuildParticipant *participant,
									  uint64 *counters, uint32 reply,
									  uint32 *action, TessStatus *status);

/*
 * The payload of the record at a reference, to change in place:
 * payload_size bytes valid as long as the record's chunk.
 */
extern TessStatusCode tess_table_payload(const TessTableRef *table,
										 uint32 offset,
										 uint8 **payload,
										 TessStatus *status);

/*
 * Visit the records from the cursor on, chunk by chunk in the order they
 * were appended, up to capacity of them: their references fill offsets,
 * count receives how many, and the cursor moves past them. The caller
 * starts the cursor at 0; a count of 0 means the walk is over.
 */
extern TessStatusCode tess_table_scan(const TessTableRef *table,
									  uint64 *cursor,
									  uint32 *offsets,
									  int capacity,
									  int *count,
									  TessStatus *status);

/*
 * Move the table to a new index of len bytes at index, for capacity
 * records (tess_table_size), over the same chunks: the buckets are filled
 * anew from the records, which stay where they are, and the old index is
 * no longer the table's. A capacity with fewer buckets is refused.
 */
extern TessStatusCode tess_table_regrow(const TessTableRef *table,
										void *index,
										Size len,
										uint64 capacity,
										TessStatus *status);

/*
 * A shared table that spills (see docs/spill.md): words in memory every
 * participant maps, which decide for all of them once which partitions
 * the table splits into and which of them go to disk, and hold the
 * counters of the rounds over the partitions afterwards. The first
 * participant whose chunks pass the budget splits the table; while they
 * still take more, the largest partition in memory goes to disk, marked
 * by the one participant whose flag set it. After the build, the rounds
 * take the files of a partition one at a time, and a partition too large
 * for one participant's memory goes whole to one of them.
 */

/* The words of the shared state for up to capacity partitions. */
extern TessStatusCode tess_table_spill_words(int capacity, Size *nwords,
											 TessStatus *status);

/* Clear the state for a budget of bytes, before any participant uses it. */
extern TessStatusCode tess_table_spill_init(uint64 *words, Size nwords,
											uint64 budget, TessStatus *status);

/*
 * Split the table into partitions, a power of two, unless another
 * participant did: *in_force receives the partitions in force.
 */
extern TessStatusCode tess_table_spill_split(uint64 *words, Size nwords,
											 uint32 partitions, uint32 *in_force,
											 TessStatus *status);

/* The partitions, 0 while the table is whole. */
extern TessStatusCode tess_table_spill_partitions(uint64 *words, Size nwords,
												  uint32 *partitions,
												  TessStatus *status);

/*
 * Add bytes of chunks in memory (negative when freed), of a partition, or
 * of none (-1) before the split: *over is whether all of them pass the
 * budget.
 */
extern TessStatusCode tess_table_spill_add_bytes(uint64 *words, Size nwords,
												 int64 delta, int32 partition,
												 bool *over, TessStatus *status);

/*
 * Send the partition in memory with the most bytes to disk: *partition is
 * its number for the participant that marked it, -1 for any other.
 */
extern TessStatusCode tess_table_spill_evict(uint64 *words, Size nwords,
											 int32 *partition,
											 TessStatus *status);

/* The partitions sent to disk so far. */
extern TessStatusCode tess_table_spill_evictions(uint64 *words, Size nwords,
												 uint64 *evictions,
												 TessStatus *status);

/* Whether a partition went to disk, and whether one participant took it whole. */
extern TessStatusCode tess_table_spill_flags(uint64 *words, Size nwords,
											 uint32 partition, bool *on_disk,
											 bool *alone, TessStatus *status);

/* Add records to a partition; then its records into *records unless NULL. */
extern TessStatusCode tess_table_spill_records(uint64 *words, Size nwords,
											   uint32 partition, uint64 added,
											   uint64 *records,
											   TessStatus *status);

/* The partition a participant starts its rounds at, spread over them. */
extern TessStatusCode tess_table_spill_start(uint64 *words, Size nwords,
											 uint32 *partition,
											 TessStatus *status);

/*
 * The next file of a partition's inner or outer rows to read, each number
 * to one participant; the partitions' count stands for the outer rows of
 * the partitions kept in memory.
 */
extern TessStatusCode tess_table_spill_take_file(uint64 *words, Size nwords,
												 uint32 partition, bool outer,
												 uint32 *file,
												 TessStatus *status);

/* Take a partition whole: *taken for the one participant that did. */
extern TessStatusCode tess_table_spill_take_alone(uint64 *words, Size nwords,
												  uint32 partition,
												  bool *taken,
												  TessStatus *status);

/*
 * The phases of a round over a partition on disk, in its own barrier's
 * numbering, as the core's batches of a parallel hash join: the elected
 * one makes the partition's index, all load its inner files and link
 * them, all probe with its outer rows and leave without waiting, the last
 * to leave frees it. The actions are TessBuildAction's.
 */
#define TESS_ROUND_ELECT		0
#define TESS_ROUND_ALLOCATE		1
#define TESS_ROUND_LOAD			2
#define TESS_ROUND_PROBE		3
#define TESS_ROUND_FREE			4

/* A round participant's next action, as tess_build_step for a build. */
extern TessStatusCode tess_round_step(TessBuildParticipant *participant,
									  uint32 reply, uint32 *action,
									  TessStatus *status);

/*
 * Set the bits of the hashes of rows in a filter several participants
 * fill at once, word by word atomically; read it with tess_bloom_probe
 * once a barrier ordered every participant's additions before the reads.
 */
extern TessStatusCode tess_bloom_shared_add(uint64 *words, Size nwords,
											const uint32 *hashes,
											const TessRowMask *rows,
											TessStatus *status);

#endif							/* TESSERA_TABLE_H */

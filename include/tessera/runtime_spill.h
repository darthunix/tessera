/* A node's temporary files of spilled blocks. Part of tessera/runtime.h. */
#ifndef TESSERA_RUNTIME_SPILL_H
#define TESSERA_RUNTIME_SPILL_H

#include "postgres.h"

#include "storage/dsm.h"
#include "storage/sharedfileset.h"
#include "varatt.h"

#include "tessera/abi.h"
#include "tessera/spill.h"

/* Declared in tessera/kernel_ops.h. */
typedef struct TessKernelOps TessKernelOps;

/*
 * Temporary files of a node that spills (docs/spill.md): one set per level
 * of partitioning, one file per set, created on its first block, into
 * which the blocks of every partition go one after another through the
 * set's write buffer; the set keeps each partition's list of blocks. A
 * block is a header (tessera/spill.h) and a chunk of the node's table. A
 * serial set writes a PostgreSQL temporary file, deleted when the set
 * frees it or the query's resources are released; a shared set writes
 * the participant's file of a SharedFileSet in the query's shared
 * memory, named "<name>.<participant>", with its lists at the end, which
 * every participant reads once the writer finished it and which is
 * deleted when the last participant detaches the segment. temp_file_limit and
 * temp_tablespaces apply as to every temporary file. A set writes, then,
 * after tess_spill_finish, reads; the files are this module's only calls
 * of PostgreSQL's file layer.
 */
typedef struct TessSpill TessSpill;
typedef struct TessSpillReader TessSpillReader;

typedef struct TessSpillConfig
{
	Size		struct_size;
	/* Owns the set, its readers and the files' buffers. */
	MemoryContext parent_context;
	/* Lay out and check the blocks' headers. */
	const TessKernelOps *kernels;
	int			npartitions;
	/* The level of partitioning, below 32, written into every header. */
	uint32		level;
	/* The table's layout fingerprint (tess_table_fingerprint). */
	uint64		fingerprint;
	/* The longest body a block may have: longer ones are an error. */
	uint64		max_len;
	/* NULL for a serial set; else the query's file set and the names. */
	SharedFileSet *shared;
	/* This participant's number in the shared set's file names. */
	int			participant;
	/* The prefix of the shared set's names, unique in its file set. */
	const char *name;
	/*
	 * The bytes of the set's write buffer, through which every block goes
	 * to its file in large writes; 0 for 64 kB.
	 */
	Size		buffer_len;
} TessSpillConfig;

#define TESS_SPILL_CONFIG_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessSpillConfig, name)

/*
 * A write buffer for a set of a node whose memory is limit bytes: a
 * sixteenth of it, from a block of PostgreSQL, 8 kB, to 256 kB. Past a
 * few blocks a larger one gains
 * little: what a write costs goes with its bytes.
 */
#define TESS_SPILL_BUFFER_LEN(limit) \
	((Size) Min(Max((Size) (limit) / 16, (Size) BLCKSZ), (Size) 256 * 1024))

/* Where a block starts in its file, which is one file of 64-bit offsets. */
typedef struct TessSpillPosition
{
	int64		offset;
} TessSpillPosition;

/*
 * The query's file set of shared sets, in the node's chunk of shared
 * memory: the leader lays it out in InitializeDSMCustomScan, a worker
 * attaches in InitializeWorkerCustomScan; the files are deleted when the
 * last participant detaches the segment.
 */
extern void tess_spill_shared_init(SharedFileSet *shared,
								   dsm_segment *segment);
extern void tess_spill_shared_attach(SharedFileSet *shared,
									 dsm_segment *segment);

/*
 * Before the file set is used again by a node that runs anew (a rescan):
 * delete every participant's file. A set takes "no file" for "this
 * participant wrote no block", so a file of the use before must not stay.
 * No participant may hold a set or a reader of the file set.
 */
extern void tess_spill_shared_reset(SharedFileSet *shared);

/*
 * In a build of PostgreSQL with injection points, a test may damage what a
 * node reads back, past the reader's own checks, to show that the node's
 * checks refuse it. The point "tessera-spill-header" is given the
 * TessSpillHeader that tess_spill_read_header read; "tessera-spill-body"
 * the body that tess_spill_read_body read, as below. Other builds have no
 * such points.
 */
typedef struct TessSpillBodyRead
{
	TessSpillKind kind;
	void	   *body;
	Size		len;
} TessSpillBodyRead;

/* A row read back refers to a value outside its chunk of values: damaged data. */
pg_noreturn extern void tess_spill_value_damaged(void);

/*
 * What a node expects of a block it read back, checked before it uses the
 * bytes: a block it did not write so is damaged data (XX001). The reader
 * checks a block against itself (tess_spill_read_header and _body); these
 * check it against the node.
 */

/* The block whose header was read is of kind. */
extern void tess_spill_expect_kind(const TessSpillHeader *header, TessSpillKind kind);

/*
 * The chunk of columns read back holds rows of words stored words, and,
 * unless rows is negative, rows rows.
 */
extern void tess_spill_expect_columns(const void *chunk, int words, int64 rows);

/* The block's number is below count, the chunks of its kind written. */
extern void tess_spill_expect_number(const TessSpillHeader *header, uint64 count);

/*
 * Every value that a word of the first ncolumns of a chunk of columns
 * read back with its one chunk of values, of len bytes at values, refers
 * to, unless the row is NULL or the column by value (byvals), lies whole
 * in that chunk (tess_spill_value_read, by typlens); once a chunk, so
 * that a reader that reads a value many times reads it unchecked.
 */
extern void tess_spill_columns_check_values(void *chunk, int ncolumns, const bool *byvals,
											const int16 *typlens, const char *values,
											Size len);

/* tess_spill_value_fits of a C string and of a TOAST pointer, out of line. */
extern bool tess_spill_value_fits_other(const char *value, Size room, int16 typlen);

/*
 * Whether a value of typlen (-1 a varlena, -2 a C string) that starts at
 * value, room bytes before the end of its chunk, ends within them. A
 * varlena's header is read by bytes, as a damaged one may lie at any
 * byte; a compressed one holds its header of 8 bytes.
 */
static inline bool
tess_spill_value_fits(const char *value, Size room, int16 typlen)
{
	uint32		header;

	if (typlen > 0)
		return (Size) typlen <= room;
	if (typlen == -1 && room >= VARHDRSZ && !VARATT_IS_1B(value))
	{
		memcpy(&header, value, VARHDRSZ);
		return VARSIZE_4B(&header) >=
			(VARATT_IS_4B_C(&header) ? VARHDRSZ_COMPRESSED : VARHDRSZ) &&
			VARSIZE_4B(&header) <= room;
	}
	if (typlen == -1 && room > 0 && VARATT_IS_1B(value) && !VARATT_IS_1B_E(value))
		return VARSIZE_1B(value) <= room;
	return tess_spill_value_fits_other(value, room, typlen);
}

/*
 * The value of typlen at byte of a chunk of values of len bytes read back,
 * which must be there (not NULL) and hold the value from its start to its
 * end; anything else is damaged data.
 */
static inline const char *
tess_spill_value_of(const char *chunk, Size len, uint64 byte, int16 typlen)
{
	if (chunk == NULL || byte >= len ||
		!tess_spill_value_fits(chunk + byte, len - byte, typlen))
		tess_spill_value_damaged();
	return chunk + byte;
}

/*
 * The value of typlen that ref, a word of a chunk of columns read back
 * with its one chunk of values, number 0, refers to in the len bytes at
 * values (tess_spill_value_of); a reference to another chunk is damaged
 * data.
 */
static inline const char *
tess_spill_value_read(const char *values, Size len, uint64 ref, int16 typlen)
{
	if ((ref >> TESS_SPILL_VALUE_BYTE_BITS) != 1)
		tess_spill_value_damaged();
	return tess_spill_value_of(values, len, tess_spill_value_byte(ref), typlen);
}

/*
 * The value of typlen that ref, a word read back, refers to in chunk
 * (ref >> TESS_SPILL_VALUE_BYTE_BITS) - 1 of the nchunks at bases, of lens
 * bytes (tess_spill_value_of); a chunk past them is damaged data.
 */
static inline const char *
tess_spill_value_at(char *const *bases, const Size *lens, uint64 nchunks,
					uint64 ref, int16 typlen)
{
	uint64		number = (ref >> TESS_SPILL_VALUE_BYTE_BITS) - 1;

	if (number >= nchunks)
		tess_spill_value_damaged();
	return tess_spill_value_of(bases[number], lens[number],
							   tess_spill_value_byte(ref), typlen);
}

/* A set of no files yet. */
extern TessSpill *tess_spill_create(const TessSpillConfig *config);

/*
 * Write a block of len bytes at body to the set's file, into the list of
 * the partition's blocks, its header naming kind and number; its start
 * into position unless that is NULL. A chunk of records is stored packed
 * when that makes it shorter (tess_spill_pack), a chunk of columns
 * always, and both read back whole. Returns the bytes on disk, header
 * included.
 */
extern Size tess_spill_write(TessSpill *spill, int partition,
							 TessSpillKind kind, uint32 number,
							 const void *body, Size len,
							 TessSpillPosition *position);

/* End the writes: a shared set's files become readable by every participant. */
extern void tess_spill_finish(TessSpill *spill);

/*
 * After tess_spill_finish: a reader of the partition's blocks in the
 * file that the participant wrote, at the first of them, or NULL when
 * that participant wrote no block to the partition. A serial set reads
 * only its own participant's.
 */
extern TessSpillReader *tess_spill_open(TessSpill *spill, int participant,
										int partition);

/*
 * The next block's header, checked against the set's fingerprint and
 * longest body; false at the end of the file. A damaged header is an
 * ERROR.
 */
extern bool tess_spill_read_header(TessSpillReader *reader,
								   TessSpillHeader *header);

/* The body of the block whose header was just read, header->len bytes. */
extern void tess_spill_read_body(TessSpillReader *reader, void *body,
								 Size len);

/* Move to the block written at position; the next read is its header. */
extern void tess_spill_seek(TessSpillReader *reader,
							TessSpillPosition position);

/* Release the reader; the file stays. */
extern void tess_spill_close(TessSpillReader *reader);

/*
 * Forget this participant's blocks of the partition, which no reader of
 * it may be reading any more; they stay counted, and their bytes stay in
 * the file until the set goes. A shared set's other participants still
 * find them.
 */
extern void tess_spill_drop(TessSpill *spill, int partition);

/*
 * The blocks and bytes this participant wrote and the partitions of it
 * that have blocks and were not dropped.
 */
extern void tess_spill_stats(const TessSpill *spill, uint64 *blocks,
							 uint64 *bytes, int *files);

/* The bytes of memory the set's buffers take now, its readers' included; 0 for NULL. */
extern Size tess_spill_memory(const TessSpill *spill);

/* Delete this participant's files and release the set. */
extern void tess_spill_free(TessSpill *spill);

/*
 * Release the set, closing this participant's files: a shared set's files
 * stay for the other participants to read until the file set is deleted
 * (SharedFileSetDeleteAll, or its segment's last detach); a serial set's
 * are deleted, as tess_spill_free does.
 */
extern void tess_spill_release(TessSpill *spill);

#endif							/* TESSERA_RUNTIME_SPILL_H */

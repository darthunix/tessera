/* A node's temporary files of spilled blocks. Part of tessera/runtime.h. */
#ifndef TESSERA_RUNTIME_SPILL_H
#define TESSERA_RUNTIME_SPILL_H

#include "postgres.h"

#include "storage/dsm.h"
#include "storage/sharedfileset.h"

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

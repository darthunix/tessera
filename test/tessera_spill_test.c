#include "postgres.h"

#include <fcntl.h>

#include "fmgr.h"
#include "storage/dsm.h"
#include "storage/fd.h"
#include "storage/fileset.h"
#include "utils/memutils.h"
#include "utils/wait_event.h"

#include "tessera/kernel_ops.h"
#include "tessera/runtime.h"
#include "tessera/spill.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_spill_serial);
PG_FUNCTION_INFO_V1(tessera_test_spill_shared);
PG_FUNCTION_INFO_V1(tessera_test_spill_packed);
PG_FUNCTION_INFO_V1(tessera_test_spill_error);
PG_FUNCTION_INFO_V1(tessera_test_spill_bytes);
PG_FUNCTION_INFO_V1(tessera_test_spill_lanes);
PG_FUNCTION_INFO_V1(tessera_test_spill_memory);

#define FINGERPRINT UINT64CONST(0x5445535354455354)
#define MAX_LEN (2 * 1024 * 1024)

/* The kernels of spilled blocks, which this module links. */
static const TessKernelOps kernels = {
	TESS_ABI_INITIALIZER(TESS_KERNEL_OPS_ABI_VERSION, TessKernelOps),
	.table_format_version = TESS_TABLE_FORMAT_VERSION,
	.spill_header_write = tess_spill_header_write,
	.spill_header_read = tess_spill_header_read,
	.spill_pack = tess_spill_pack,
	.spill_unpack = tess_spill_unpack,
	.spill_columns_init = tess_spill_columns_init,
	.spill_columns_pack = tess_spill_columns_pack,
	.spill_columns_unpack = tess_spill_columns_unpack,
};

/* A block's body: every word names its partition, chunk and place. */
typedef struct Block
{
	int			partition;
	TessSpillKind kind;
	uint32		number;
	Size		len;
	TessSpillPosition position;
} Block;

static uint64
word(const Block *block, Size i)
{
	return ((uint64) block->partition << 48) |
		((uint64) block->number << 32) | (uint64) i;
}

static void
fill(const Block *block, uint64 *body)
{
	for (Size i = 0; i < block->len / sizeof(uint64); i++)
		body[i] = word(block, i);
}

static bool
same(const Block *block, const uint64 *body)
{
	for (Size i = 0; i < block->len / sizeof(uint64); i++)
		if (body[i] != word(block, i))
			return false;
	return true;
}

static TessSpill *
make_spill(SharedFileSet *shared, int participant, uint64 fingerprint)
{
	TessSpillConfig config = TESS_STRUCT_INITIALIZER(TessSpillConfig);

	config.parent_context = CurrentMemoryContext;
	config.kernels = &kernels;
	config.npartitions = 4;
	config.level = 1;
	config.fingerprint = fingerprint;
	config.max_len = MAX_LEN;
	config.shared = shared;
	config.participant = participant;
	config.name = "tess_test";
	return tess_spill_create(&config);
}

static void
write_blocks(TessSpill *spill, Block *blocks, int nblocks)
{
	uint64	   *body = palloc(MAX_LEN);

	for (int i = 0; i < nblocks; i++)
	{
		fill(&blocks[i], body);
		tess_spill_write(spill, blocks[i].partition, blocks[i].kind,
						 blocks[i].number, body, blocks[i].len,
						 &blocks[i].position);
	}
	pfree(body);
}

/* The partition's blocks read in order are the ones written to it. */
static bool
read_partition(TessSpill *spill, int participant, int partition,
			   const Block *blocks, int nblocks)
{
	TessSpillReader *reader = tess_spill_open(spill, participant, partition);
	TessSpillHeader header;
	uint64	   *body = palloc(MAX_LEN);
	bool		ok = true;
	int			found = 0;

	for (int i = 0; i < nblocks && ok; i++)
	{
		if (blocks[i].partition != partition)
			continue;
		found++;
		ok = reader != NULL && tess_spill_read_header(reader, &header) &&
			header.kind == (uint32) blocks[i].kind &&
			header.number == blocks[i].number &&
			header.partition == (uint32) partition && header.level == 1 &&
			header.len == blocks[i].len;
		if (ok)
		{
			tess_spill_read_body(reader, body, header.len);
			ok = same(&blocks[i], body);
		}
	}
	if (found == 0)
		ok = reader == NULL;
	else if (ok)
		ok = !tess_spill_read_header(reader, &header);
	tess_spill_close(reader);
	pfree(body);
	return ok;
}

/* A block read after a seek to its position is the one written there. */
static bool
seek_block(TessSpill *spill, int participant, const Block *block)
{
	TessSpillReader *reader = tess_spill_open(spill, participant,
											  block->partition);
	TessSpillHeader header;
	uint64	   *body = palloc(MAX_LEN);
	bool		ok;

	tess_spill_seek(reader, block->position);
	ok = tess_spill_read_header(reader, &header) &&
		header.number == block->number && header.len == block->len;
	if (ok)
	{
		tess_spill_read_body(reader, body, header.len);
		ok = same(block, body);
	}
	tess_spill_close(reader);
	pfree(body);
	return ok;
}

/*
 * Where a header keeps its partition, its level and its body length, as
 * tess_spill_header_write lays them out.
 */
#define HEADER_PARTITION_AT 20
#define HEADER_LEVEL_AT 24
#define HEADER_LEN_AT 40

/* A participant's file of the test's shared set, opened to be damaged. */
static File
open_shared_file(SharedFileSet *shared, int participant)
{
	char		name[MAXPGPATH];
	File		file;

	snprintf(name, sizeof(name), "tess_test.%d", participant);
	file = FileSetOpen(&shared->fs, name, O_RDWR);
	if (file <= 0)
		elog(ERROR, "the test could not open the shared spill file %s", name);
	return file;
}

/*
 * Overwrite len bytes of a participant's finished file: at offset from its
 * start, or from its end when offset is negative.
 */
static void
damage_shared_file(SharedFileSet *shared, int participant, int64 offset,
				   const void *bytes, Size len)
{
	File		file = open_shared_file(shared, participant);

	if (offset < 0)
		offset += FileSize(file);
	if (FileWrite(file, bytes, len, (pgoff_t) offset,
				  WAIT_EVENT_BUFFILE_WRITE) != (ssize_t) len)
		elog(ERROR, "the test could not damage the shared spill file");
	FileClose(file);
}

/* Cut a participant's finished file to len bytes. */
static void
cut_shared_file(SharedFileSet *shared, int participant, int64 len)
{
	File		file = open_shared_file(shared, participant);

	if (FileTruncate(file, (pgoff_t) len, WAIT_EVENT_BUFFILE_TRUNCATE) != 0)
		elog(ERROR, "the test could not cut the shared spill file");
	FileClose(file);
}

/*
 * A chunk of nrecords records of 40 bytes that packs: a hash, the length
 * and a key in each, as a join's table holds them.
 */
static uint32 *
packable_chunk(int nrecords, Size *len)
{
	uint32	   *chunk;

	*len = 8 + (Size) nrecords * 40;
	chunk = palloc0(*len);
	*(uint64 *) chunk = *len;
	for (int record = 0; record < nrecords; record++)
	{
		uint32	   *words = chunk + 2 + record * 10;

		words[0] = (uint32) record * 2654435761U;
		words[3] = 5;
		words[4] = 1000000 + record;
	}
	return chunk;
}

/*
 * A serial set: blocks of records and values of 8 bytes, several file
 * buffers and 1 MB, and an empty one, into two of four partitions, read
 * back in order and by position; a partition without blocks has no
 * reader; the counters; a dropped partition.
 */
Datum
tessera_test_spill_serial(PG_FUNCTION_ARGS)
{
	Block		blocks[] = {
		{0, TESS_SPILL_RECORDS, 0, 8},
		{2, TESS_SPILL_RECORDS, 1, 3 * BLCKSZ + 8},
		{0, TESS_SPILL_VALUES, 0, 1024 * 1024},
		{2, TESS_SPILL_VALUES, 1, 0},
		{0, TESS_SPILL_RECORDS, 2, 1024 * 1024},
	};
	int			nblocks = lengthof(blocks);
	TessSpill  *spill = make_spill(NULL, 0, FINGERPRINT);
	uint64		written = 0;
	uint64		nwritten;
	uint64		bytes;
	int			files;

	write_blocks(spill, blocks, nblocks);
	tess_spill_finish(spill);
	for (int partition = 0; partition < 4; partition++)
		if (!read_partition(spill, 0, partition, blocks, nblocks))
			elog(ERROR, "partition %d reads back wrong", partition);
	/* Twice: a reader starts from the file's first block. */
	if (!read_partition(spill, 0, 0, blocks, nblocks))
		elog(ERROR, "partition 0 reads back wrong the second time");
	for (int i = nblocks - 1; i >= 0; i--)
		if (!seek_block(spill, 0, &blocks[i]))
			elog(ERROR, "block %d reads back wrong at its position", i);
	for (int i = 0; i < nblocks; i++)
		written += TESS_SPILL_HEADER_SIZE + blocks[i].len;
	tess_spill_stats(spill, &nwritten, &bytes, &files);
	if (nwritten != (uint64) nblocks || bytes != written || files != 2)
		elog(ERROR, "counters: " UINT64_FORMAT " blocks, " UINT64_FORMAT " bytes, %d files",
			 nwritten, bytes, files);
	tess_spill_drop(spill, 0);
	tess_spill_stats(spill, &nwritten, NULL, &files);
	if (files != 1 || nwritten != (uint64) nblocks ||
		tess_spill_open(spill, 0, 0) != NULL)
		elog(ERROR, "a dropped file is still there");
	tess_spill_free(spill);
	PG_RETURN_BOOL(true);
}

/*
 * Two participants of a shared set in one segment: each reads the other's
 * file and its own, a partition one of them left empty has no reader, and
 * neither has a participant that wrote nothing; one that releases its set
 * leaves its file to the others, and the files go with the segment.
 */
Datum
tessera_test_spill_shared(PG_FUNCTION_ARGS)
{
	Block		first[] = {
		{1, TESS_SPILL_RECORDS, 0, 64},
		{3, TESS_SPILL_RECORDS, 1, 1024 * 1024},
		{1, TESS_SPILL_VALUES, 0, 5 * BLCKSZ},
	};
	Block		second[] = {
		{1, TESS_SPILL_RECORDS, 2, 1024 * 1024},
		{2, TESS_SPILL_VALUES, 1, 16},
	};
	dsm_segment *segment = dsm_create(sizeof(SharedFileSet), 0);
	SharedFileSet *shared = dsm_segment_address(segment);
	TessSpill  *one;
	TessSpill  *two;
	TessSpill  *none;
	TessSpillReader *early;
	TessSpillReader *late;
	TessSpillHeader header;

	tess_spill_shared_init(shared, segment);
	one = make_spill(shared, 0, FINGERPRINT);
	two = make_spill(shared, 1, FINGERPRINT);
	write_blocks(one, first, lengthof(first));
	write_blocks(two, second, lengthof(second));
	tess_spill_finish(one);
	tess_spill_finish(two);
	for (int partition = 0; partition < 4; partition++)
		if (!read_partition(one, 1, partition, second, lengthof(second)) ||
			!read_partition(two, 0, partition, first, lengthof(first)) ||
			!read_partition(one, 0, partition, first, lengthof(first)))
			elog(ERROR, "shared partition %d reads back wrong", partition);
	if (!seek_block(two, 0, &first[2]) || !seek_block(one, 1, &second[0]))
		elog(ERROR, "a shared block reads back wrong at its position");
	/* A participant that wrote no block has no file: it opens as no reader. */
	none = make_spill(shared, 2, FINGERPRINT);
	tess_spill_finish(none);
	for (int partition = 0; partition < 4; partition++)
		if (tess_spill_open(one, 2, partition) != NULL)
			elog(ERROR, "a participant without blocks has partition %d", partition);
	tess_spill_free(none);
	/* A partition its writer dropped is gone for the writer, not for the other. */
	tess_spill_drop(one, 3);
	if (tess_spill_open(one, 0, 3) != NULL ||
		!read_partition(two, 0, 3, first, lengthof(first)))
		elog(ERROR, "a dropped partition is not the writer's alone to forget");
	/* Two readers of one file keep their own positions. */
	early = tess_spill_open(one, 1, 1);
	late = tess_spill_open(two, 1, 1);
	if (!tess_spill_read_header(late, &header) || header.number != 2 ||
		!tess_spill_read_header(early, &header) || header.number != 2)
		elog(ERROR, "two readers share a position");
	tess_spill_close(late);
	/* The set closes a reader left open; released, its files stay for the others. */
	tess_spill_release(one);
	for (int partition = 0; partition < 4; partition++)
		if (!read_partition(two, 0, partition, first, lengthof(first)))
			elog(ERROR, "a released participant's partition %d is gone", partition);
	tess_spill_drop(two, 1);
	if (tess_spill_open(two, 1, 1) != NULL)
		elog(ERROR, "a dropped shared file is still there");
	tess_spill_free(two);
	dsm_detach(segment);
	PG_RETURN_BOOL(true);
}

/*
 * A chunk of 1000 records of 40 bytes, as a join's table holds them (a
 * hash, a next-record reference, no NULL, the length, an int4 key and an
 * int4 value in slots of 8 bytes, a NULL-bits word of 0), goes to disk
 * packed, a lane of 4 bytes stored at the width its values need, and reads
 * back the same but for the next-record references, which a chunk not yet
 * linked holds as 0; bytes that are no such chunk go as they are.
 */
Datum
tessera_test_spill_packed(PG_FUNCTION_ARGS)
{
	const int	nrecords = 1000;
	Size		len = 8 + nrecords * 40;
	uint32	   *chunk = palloc0(len);
	uint32	   *back = palloc(len);
	uint64	   *noise = palloc(4096);
	TessSpill  *spill = make_spill(NULL, 0, FINGERPRINT);
	TessSpillReader *reader;
	TessSpillHeader header;
	Size		stored;
	Size		plain;
	uint64		bytes;

	*(uint64 *) chunk = len;
	for (int record = 0; record < nrecords; record++)
	{
		uint32	   *words = chunk + 2 + record * 10;

		words[0] = (uint32) record * 2654435761U;
		words[1] = 12345;
		words[3] = 5;
		words[4] = 1000000 + record;
		words[8] = 7 * record + 100000;
	}
	for (int word = 0; word < 512; word++)
		noise[word] = UINT64CONST(0x9e3779b97f4a7c15) * (word + 1);
	stored = tess_spill_write(spill, 1, TESS_SPILL_RECORDS, 0, chunk, len, NULL);
	plain = tess_spill_write(spill, 1, TESS_SPILL_RECORDS, 1, noise, 4096, NULL);
	if (stored > TESS_SPILL_HEADER_SIZE + 8 + 16 + nrecords * 12 + 8 ||
		plain != TESS_SPILL_HEADER_SIZE + 4096)
		elog(ERROR, "stored %zu and %zu bytes", stored, plain);
	tess_spill_stats(spill, NULL, &bytes, NULL);
	if (bytes != stored + plain)
		elog(ERROR, "counted " UINT64_FORMAT " bytes", bytes);
	tess_spill_finish(spill);
	reader = tess_spill_open(spill, 0, 1);
	if (!tess_spill_read_header(reader, &header) || header.len != len ||
		header.packed == 0)
		elog(ERROR, "a packed chunk reads back with a wrong header");
	tess_spill_read_body(reader, back, len);
	for (int record = 0; record < nrecords; record++)
		chunk[2 + record * 10 + 1] = 0;
	if (memcmp(back, chunk, len) != 0)
		elog(ERROR, "a packed chunk reads back wrong");
	if (!tess_spill_read_header(reader, &header) || header.len != 4096 ||
		header.packed != 0)
		elog(ERROR, "a plain block reads back with a wrong header");
	tess_spill_read_body(reader, back, 4096);
	if (memcmp(back, noise, 4096) != 0)
		elog(ERROR, "a plain block reads back wrong");
	tess_spill_close(reader);
	tess_spill_free(spill);
	PG_RETURN_BOOL(true);
}

/*
 * The bytes a set says it holds: its write buffer until it is finished,
 * the buffer a chunk larger than the write buffer is packed through, and
 * the buffer of an open reader, as large as the partition's largest block.
 * The chunk that took the long way reads back as it was written.
 */
Datum
tessera_test_spill_memory(PG_FUNCTION_ARGS)
{
	TessSpill  *spill = make_spill(NULL, 0, FINGERPRINT);
	Size		empty = tess_spill_memory(spill);
	Size		len;
	uint32	   *chunk = packable_chunk(3000, &len);
	uint32	   *back = palloc(len);
	TessSpillReader *reader;
	TessSpillHeader header;
	Size		stored;
	Size		written;
	Size		finished;

	/* The chunk is longer than the write buffer, which is all a new set holds. */
	if (empty == 0 || len <= empty)
		elog(ERROR, "a new set holds %zu bytes", empty);
	stored = tess_spill_write(spill, 2, TESS_SPILL_RECORDS, 0, chunk, len, NULL);
	written = tess_spill_memory(spill);
	if (written < empty + len)
		elog(ERROR, "a set that packed a long chunk holds %zu bytes", written);
	tess_spill_finish(spill);
	finished = tess_spill_memory(spill);
	if (finished != written - empty)
		elog(ERROR, "a finished set holds %zu bytes of %zu", finished, written);
	reader = tess_spill_open(spill, 0, 2);
	if (tess_spill_memory(spill) != finished + TYPEALIGN(8, stored))
		elog(ERROR, "a set with a reader holds %zu bytes", tess_spill_memory(spill));
	if (!tess_spill_read_header(reader, &header) || header.len != len ||
		header.packed == 0)
		elog(ERROR, "a long chunk reads back with a wrong header");
	tess_spill_read_body(reader, back, len);
	if (memcmp(back, chunk, len) != 0)
		elog(ERROR, "a long chunk reads back wrong");
	tess_spill_close(reader);
	if (tess_spill_memory(spill) != finished)
		elog(ERROR, "a set whose reader is closed holds %zu bytes", tess_spill_memory(spill));
	tess_spill_free(spill);
	PG_RETURN_BOOL(true);
}

/* Each case raises the ERROR the SQL expects. */
Datum
tessera_test_spill_error(PG_FUNCTION_ARGS)
{
	int			which = PG_GETARG_INT32(0);
	Block		block = {0, TESS_SPILL_RECORDS, 0, 64};
	uint64		body[8] = {0};
	TessSpill  *spill = make_spill(NULL, 0, FINGERPRINT);
	TessSpillReader *reader;
	TessSpillHeader header;
	dsm_segment *segment = NULL;
	SharedFileSet *shared = NULL;
	TessSpill  *other = NULL;
	TessSpillPosition position;
	uint64		word8;
	uint32		word4;
	uint8		byte;
	uint32	   *chunk;
	Size		len;

	/*
	 * Cases 10 to 15 damage a finished file: participant 0 of a shared set
	 * writes one block and finishes, and participant 1 reads its file.
	 */
	if ((which >= 10 && which <= 15) || which == 17 || which == 18 || which == 22)
	{
		segment = dsm_create(sizeof(SharedFileSet), 0);
		shared = dsm_segment_address(segment);
		tess_spill_shared_init(shared, segment);
		other = make_spill(shared, 0, FINGERPRINT);
		write_blocks(other, &block, 1);
		tess_spill_finish(other);
		other = make_spill(shared, 1, FINGERPRINT);
		tess_spill_finish(other);
	}

	switch (which)
	{
		case 1:
			/* A block after the set was finished. */
			write_blocks(spill, &block, 1);
			tess_spill_finish(spill);
			write_blocks(spill, &block, 1);
			break;
		case 2:
			/* A body of a length the header does not take. */
			tess_spill_write(spill, 0, TESS_SPILL_RECORDS, 0, body, 12, NULL);
			break;
		case 3:
			/* A body longer than the set accepts. */
			tess_spill_write(spill, 0, TESS_SPILL_VALUES, 0, body,
							 MAX_LEN + 8, NULL);
			break;
		case 4:
			/* Another participant's file of a serial set. */
			tess_spill_finish(spill);
			tess_spill_open(spill, 1, 0);
			break;
		case 5:
			/* A body read with another length than its header's. */
			write_blocks(spill, &block, 1);
			tess_spill_finish(spill);
			reader = tess_spill_open(spill, 0, 0);
			tess_spill_read_header(reader, &header);
			tess_spill_read_body(reader, body, 8);
			break;
		case 6:
			/* The next header before the body. */
			write_blocks(spill, &block, 1);
			tess_spill_finish(spill);
			reader = tess_spill_open(spill, 0, 0);
			tess_spill_read_header(reader, &header);
			tess_spill_read_header(reader, &header);
			break;
		case 7:
			/* A block of another table's layout. */
			segment = dsm_create(sizeof(SharedFileSet), 0);
			shared = dsm_segment_address(segment);
			tess_spill_shared_init(shared, segment);
			other = make_spill(shared, 0, FINGERPRINT);
			write_blocks(other, &block, 1);
			tess_spill_finish(other);
			other = make_spill(shared, 1, FINGERPRINT + 1);
			tess_spill_finish(other);
			reader = tess_spill_open(other, 0, 0);
			tess_spill_read_header(reader, &header);
			break;
		case 8:
			/* A partition out of range. */
			tess_spill_write(spill, 4, TESS_SPILL_RECORDS, 0, body, 8, NULL);
			break;
		case 9:
			/* A second reader of a serial file. */
			write_blocks(spill, &block, 1);
			tess_spill_finish(spill);
			tess_spill_open(spill, 0, 0);
			tess_spill_open(spill, 0, 0);
			break;
		case 10:
			/* A shared file that ends before its lists and its trailer. */
			cut_shared_file(shared, 0, 16);
			tess_spill_open(other, 0, 0);
			break;
		case 11:
			/* A trailer without its magic. */
			word8 = 0;
			damage_shared_file(shared, 0, -32, &word8, sizeof(word8));
			tess_spill_open(other, 0, 0);
			break;
		case 12:
			/* A trailer of another number of partitions than the set's. */
			word8 = 5;
			damage_shared_file(shared, 0, -16, &word8, sizeof(word8));
			tess_spill_open(other, 0, 0);
			break;
		case 13:
			/* A block whose header names another partition than the list's. */
			word4 = 2;
			damage_shared_file(shared, 0, HEADER_PARTITION_AT, &word4, sizeof(word4));
			reader = tess_spill_open(other, 0, 0);
			tess_spill_read_header(reader, &header);
			break;
		case 14:
			/* A block whose header names another level than the set's. */
			word4 = 2;
			damage_shared_file(shared, 0, HEADER_LEVEL_AT, &word4, sizeof(word4));
			reader = tess_spill_open(other, 0, 0);
			tess_spill_read_header(reader, &header);
			break;
		case 15:
			/* A header whose body length is not the block's on disk. */
			word8 = block.len + 8;
			damage_shared_file(shared, 0, HEADER_LEN_AT, &word8, sizeof(word8));
			reader = tess_spill_open(other, 0, 0);
			tess_spill_read_header(reader, &header);
			break;
		case 17:
			/*
			 * A count of more blocks than the lists hold: the lists are a
			 * count for each of the four partitions and one block's entry.
			 */
			word8 = 2;
			damage_shared_file(shared, 0, -(32 + 16 + 4 * 8), &word8, sizeof(word8));
			tess_spill_open(other, 0, 0);
			break;
		case 18:
			/* A block that the list says ends past the file's blocks. */
			word8 = TESS_SPILL_HEADER_SIZE + block.len + 8;
			damage_shared_file(shared, 0, -(32 + 8), &word8, sizeof(word8));
			tess_spill_open(other, 0, 0);
			break;
		case 19:
		case 23:
			/* A packed block of participant 0 that participant 1 reads. */
			chunk = packable_chunk(100, &len);
			segment = dsm_create(sizeof(SharedFileSet), 0);
			shared = dsm_segment_address(segment);
			tess_spill_shared_init(shared, segment);
			other = make_spill(shared, 0, FINGERPRINT);
			tess_spill_write(other, 0, TESS_SPILL_RECORDS, 0, chunk, len, NULL);
			tess_spill_finish(other);
			other = make_spill(shared, 1, FINGERPRINT);
			tess_spill_finish(other);
			if (which == 19)
			{
				/* Its header names another length than its body unpacks into. */
				word8 = len + 8;
				damage_shared_file(shared, 0, HEADER_LEN_AT, &word8, sizeof(word8));
			}
			else
			{
				/* Its body is damaged: the code of its first lane does not exist. */
				byte = 9;
				damage_shared_file(shared, 0, TESS_SPILL_HEADER_SIZE + 8, &byte, sizeof(byte));
			}
			reader = tess_spill_open(other, 0, 0);
			tess_spill_read_header(reader, &header);
			tess_spill_read_body(reader, palloc(len), len);
			break;
		case 20:
			/* A partition dropped while a reader of it is open. */
			write_blocks(spill, &block, 1);
			tess_spill_finish(spill);
			tess_spill_open(spill, 0, 0);
			tess_spill_drop(spill, 0);
			break;
		case 21:
			/* A block of a kind that does not exist. */
			tess_spill_write(spill, 0, (TessSpillKind) 9, 0, body, 8, NULL);
			break;
		case 22:
			/* A block that the list says is shorter than a header. */
			word8 = 8;
			damage_shared_file(shared, 0, -(32 + 8), &word8, sizeof(word8));
			tess_spill_open(other, 0, 0);
			break;
		case 16:
			/* A seek to a position that holds no block of the partition. */
			write_blocks(spill, &block, 1);
			tess_spill_finish(spill);
			reader = tess_spill_open(spill, 0, 0);
			position.offset = 8;
			tess_spill_seek(reader, position);
			break;
	}
	PG_RETURN_VOID();
}

/* Write bytes in blocks of 1 MB to one partition, for temp_file_limit. */
Datum
tessera_test_spill_bytes(PG_FUNCTION_ARGS)
{
	int64		bytes = PG_GETARG_INT64(0);
	TessSpill  *spill = make_spill(NULL, 0, FINGERPRINT);
	char	   *body = palloc0(1024 * 1024);

	for (uint32 number = 0; bytes > 0; number++, bytes -= 1024 * 1024)
		tess_spill_write(spill, 0, TESS_SPILL_VALUES, number, body, 1024 * 1024,
						 NULL);
	tess_spill_free(spill);
	pfree(body);
	PG_RETURN_VOID();
}

/*
 * The inline formulas of a chunk of columns (spill.h), which the nodes
 * compute per row, against the Rust side's at every count of stored words.
 */
Datum
tessera_test_spill_lanes(PG_FUNCTION_ARGS)
{
	for (uint32 words = 0; words <= 4096; words++)
	{
		Size		lanes = tess_spill_columns_shape(words, TESS_SPILL_COLUMNS_SHAPE_NULL_LANES);

		if ((Size) tess_spill_columns_null_lanes(words) != lanes ||
			(Size) TESS_SPILL_COLUMNS_NULL_LANES(words) != lanes ||
			TESS_SPILL_COLUMNS_SLACK(words) !=
			tess_spill_columns_shape(words, TESS_SPILL_COLUMNS_SHAPE_SLACK))
			elog(ERROR, "the C formulas of a chunk of %u words differ from Rust's", words);
		/* Word w's NULL bit is bit w % 64 of lane w / 64, within the lanes. */
		for (int word = 0; word < (int) words; word += 63)
			if (tess_spill_columns_null_lane(word) >= (int) lanes ||
				!tess_spill_columns_is_null(tess_spill_columns_null_bit(word), word) ||
				tess_spill_columns_is_null(~tess_spill_columns_null_bit(word), word))
				elog(ERROR, "word %d of a chunk of %u words has no NULL bit of its own",
					 word, words);
	}
	if (tess_spill_columns_shape(1, 99) != 0)
		elog(ERROR, "an unknown shape is not 0");
	PG_RETURN_BOOL(true);
}

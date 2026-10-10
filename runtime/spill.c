/*
 * Temporary files of a node that spills.
 *
 * A set writes one file, created on its first block: a serial set a
 * PostgreSQL temporary file, a shared one the participant's file of the
 * query's SharedFileSet. The blocks of every partition go into it one
 * after another through the set's own write buffer, so that the file is
 * written in large pieces however small the blocks; the set keeps, per
 * partition, where each of its blocks starts and the bytes it takes, and
 * a shared set writes that list at the end of its file when it finishes,
 * for the other participants to read. A block is a header, laid out and
 * checked by the kernels (tessera/spill.h), and a chunk of the node's
 * table; a reader reads a block whole with one read. This is the only
 * code that calls PostgreSQL's file layer for spilling; a core with
 * another manager of work files replaces it here. See docs/spill.md,
 * "Files".
 */
#include "postgres.h"

#include <fcntl.h>

#include "commands/tablespace.h"
#include "storage/fd.h"
#include "storage/fileset.h"
#include "utils/memutils.h"
#include "utils/wait_event.h"

#include "tessera/kernel_ops.h"
#include "tessera/runtime.h"

/* The write buffer when the config gives none. */
#define SPILL_DEFAULT_BUFFER (64 * 1024)

/* The end of a shared set's file: the list of its blocks by partition. */
#define SPILL_TRAILER_MAGIC UINT64CONST(0x5445535354524149)

typedef struct SpillTrailer
{
	uint64		magic;
	/* Where the list starts: a count per partition, then the blocks by partition. */
	uint64		offset;
	uint64		npartitions;
	uint64		fingerprint;
} SpillTrailer;

/* A block in the file: where its header starts and the bytes it takes, header included. */
typedef struct SpillBlock
{
	uint64		offset;
	uint64		stored;
} SpillBlock;

/* The blocks of a partition in the order they were written. */
typedef struct SpillList
{
	SpillBlock *blocks;
	uint64		count;
	uint64		slots;
} SpillList;

struct TessSpill
{
	/* Owns the set, the readers and the buffers. */
	MemoryContext context;
	const TessKernelOps *kernels;
	int			npartitions;
	uint32		level;
	uint64		fingerprint;
	uint64		max_len;
	SharedFileSet *shared;
	int			participant;
	char	   *name;
	/* The set's file, and its length with what the buffer holds. */
	File		file;
	uint64		end;
	/* The write buffer, until the set finishes, and its bytes not yet written. */
	char	   *buffer;
	Size		buffer_len;
	Size		buffered;
	/* This participant's blocks by partition. */
	SpillList  *lists;
	/* This participant's readers of each partition: a serial one has one at a time, as ever. */
	int		   *reading;
	bool		finished;
	/* The open readers, closed with the set. */
	TessSpillReader *readers;
	uint64		blocks;
	uint64		bytes;
	int			nfiles;
	/* A buffer for packing a chunk too large for the write buffer. */
	char	   *scratch;
	Size		scratch_len;
};

struct TessSpillReader
{
	TessSpill  *spill;
	/* Another participant's file, opened for this reader, or the set's own. */
	File		file;
	bool		own;
	int			partition;
	/* The partition's blocks, and the next one to read. */
	SpillBlock *blocks;
	uint64		count;
	uint64		next;
	/* The block just read, header and stored body, until its body is taken. */
	char	   *buffer;
	Size		buffer_len;
	bool		pending;
	uint32		pending_kind;
	uint64		pending_len;
	uint32		pending_packed;
	TessSpillReader *next_reader;
};

static void
check_spill(const TessSpill *spill)
{
	if (spill == NULL)
		elog(ERROR, "Tessera spill set is missing");
}

static void
check_partition(const TessSpill *spill, int partition)
{
	if (partition < 0 || partition >= spill->npartitions)
		elog(ERROR, "Tessera spill partition %d is out of range 0..%d",
			 partition, spill->npartitions - 1);
}

static void
file_name(const TessSpill *spill, int participant, char name[MAXPGPATH])
{
	snprintf(name, MAXPGPATH, "%s.%d", spill->name, participant);
}

void
tess_spill_shared_init(SharedFileSet *shared, dsm_segment *segment)
{
	if (shared == NULL || segment == NULL)
		elog(ERROR, "Tessera shared spill files require a file set and a segment");
	SharedFileSetInit(shared, segment);
}

void
tess_spill_shared_attach(SharedFileSet *shared, dsm_segment *segment)
{
	if (shared == NULL || segment == NULL)
		elog(ERROR, "Tessera shared spill files require a file set and a segment");
	SharedFileSetAttach(shared, segment);
}

void
tess_spill_shared_reset(SharedFileSet *shared)
{
	if (shared == NULL)
		elog(ERROR, "Tessera shared spill files require a file set");
	SharedFileSetDeleteAll(shared);
}

void
tess_spill_value_damaged(void)
{
	ereport(ERROR,
			errcode(ERRCODE_DATA_CORRUPTED),
			errmsg("Tessera row read back refers to a value outside its chunk of values"));
}

TessSpill *
tess_spill_create(const TessSpillConfig *config)
{
	MemoryContext context;
	TessSpill  *spill;

	if (config == NULL || config->struct_size < TESS_SPILL_CONFIG_MIN_SIZE ||
		config->parent_context == NULL)
		elog(ERROR, "Tessera spill set requires a config and a context");
	if (config->kernels == NULL ||
		config->kernels->spill_header_write == NULL ||
		config->kernels->spill_header_read == NULL ||
		config->kernels->spill_pack == NULL || config->kernels->spill_unpack == NULL)
		elog(ERROR, "Tessera spill set requires the kernels of spilled blocks");
	if (config->npartitions <= 0 || config->level >= 32)
		elog(ERROR, "Tessera spill set requires partitions and a level below 32");
	if (config->shared != NULL &&
		(config->participant < 0 || config->name == NULL ||
		 strlen(config->name) > MAXPGPATH / 2))
		elog(ERROR, "Tessera shared spill set requires a participant and a name");

	/*
	 * Temporary tablespaces are looked up now, not at the first block, which
	 * may come deep inside the node's execution.
	 */
	if (config->shared == NULL)
		PrepareTempTablespaces();
	context = AllocSetContextCreate(config->parent_context, "TessSpill",
									ALLOCSET_DEFAULT_SIZES);
	spill = MemoryContextAllocZero(context, sizeof(TessSpill));
	spill->context = context;
	spill->kernels = config->kernels;
	spill->npartitions = config->npartitions;
	spill->level = config->level;
	spill->fingerprint = config->fingerprint;
	spill->max_len = config->max_len;
	spill->shared = config->shared;
	spill->participant = config->shared != NULL ? config->participant : 0;
	spill->name = config->shared != NULL ?
		MemoryContextStrdup(context, config->name) : NULL;
	spill->file = -1;
	spill->buffer_len = SPILL_DEFAULT_BUFFER;
	if (TESS_ABI_HAS_FIELD(config, TessSpillConfig, buffer_len) &&
		config->buffer_len > 0)
		spill->buffer_len = TYPEALIGN(8, Max(config->buffer_len, TESS_SPILL_HEADER_SIZE + 64));
	/* The buffer from the start, so that the node counts it before any block. */
	spill->buffer = MemoryContextAllocExtended(context, spill->buffer_len, MCXT_ALLOC_HUGE);
	spill->lists = MemoryContextAllocZero(context,
										  mul_size(config->npartitions,
												   sizeof(SpillList)));
	spill->reading = MemoryContextAllocZero(context,
											mul_size(config->npartitions,
													 sizeof(int)));
	return spill;
}

/* The set's file, created on its first block. */
static File
set_file(TessSpill *spill)
{
	char		name[MAXPGPATH];

	if (spill->file > 0)
		return spill->file;
	if (spill->shared == NULL)
		spill->file = OpenTemporaryFile(false);
	else
	{
		file_name(spill, spill->participant, name);
		spill->file = FileSetCreate(&spill->shared->fs, name);
	}
	if (spill->file <= 0)
		ereport(ERROR,
				errcode_for_file_access(),
				errmsg("could not create Tessera spill file: %m"));
	return spill->file;
}

/* Write len bytes at offset, whole. */
static void
write_at(File file, const char *bytes, Size len, uint64 offset)
{
	while (len > 0)
	{
		ssize_t		written = FileWrite(file, bytes, len, (pgoff_t) offset,
										WAIT_EVENT_BUFFILE_WRITE);

		if (written <= 0)
			ereport(ERROR,
					errcode_for_file_access(),
					errmsg("could not write Tessera spill file: %m"));
		bytes += written;
		len -= written;
		offset += written;
	}
}

/* Read len bytes at offset, whole. */
static void
read_at(File file, char *bytes, Size len, uint64 offset)
{
	while (len > 0)
	{
		ssize_t		read = FileRead(file, bytes, len, (pgoff_t) offset,
									WAIT_EVENT_BUFFILE_READ);

		if (read < 0)
			ereport(ERROR,
					errcode_for_file_access(),
					errmsg("could not read Tessera spill file: %m"));
		if (read == 0)
			ereport(ERROR,
					errcode(ERRCODE_DATA_CORRUPTED),
					errmsg("Tessera spill file ends inside a block"));
		bytes += read;
		len -= read;
		offset += read;
	}
}

/* Write what the buffer holds. */
static void
flush(TessSpill *spill)
{
	if (spill->buffered == 0)
		return;
	write_at(set_file(spill), spill->buffer, spill->buffered, spill->end - spill->buffered);
	spill->buffered = 0;
}

/* Note a block of the partition, at offset and of stored bytes. */
static void
add_block(TessSpill *spill, int partition, uint64 offset, uint64 stored)
{
	SpillList  *list = &spill->lists[partition];

	if (list->count == list->slots)
	{
		uint64		slots = Max(list->slots * 2, 16);

		list->blocks = list->blocks == NULL ?
			MemoryContextAllocExtended(spill->context, mul_size(slots, sizeof(SpillBlock)),
									   MCXT_ALLOC_HUGE) :
			repalloc_huge(list->blocks, mul_size(slots, sizeof(SpillBlock)));
		list->slots = slots;
	}
	if (list->count == 0)
		spill->nfiles++;
	list->blocks[list->count].offset = offset;
	list->blocks[list->count].stored = stored;
	list->count++;
}

/* The set's buffer for packing, of at least len bytes. */
static char *
scratch(TessSpill *spill, Size len)
{
	if (spill->scratch_len < len)
	{
		if (spill->scratch != NULL)
			pfree(spill->scratch);
		spill->scratch = MemoryContextAllocExtended(spill->context, len, MCXT_ALLOC_HUGE);
		spill->scratch_len = len;
	}
	return spill->scratch;
}

Size
tess_spill_write(TessSpill *spill, int partition, TessSpillKind kind,
				 uint32 number, const void *body, Size len,
				 TessSpillPosition *position)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	TessSpillHeader header = {0};
	uint64		bytes[TESS_SPILL_HEADER_SIZE / sizeof(uint64)];
	bool		pack;
	/*
	 * The body was packed in place, into the buffer at `at`: known from
	 * where it was packed, not from its address, since a block aside may
	 * lie right past the end of a full buffer, where `at` then points.
	 */
	bool		in_buffer = false;
	char	   *at;
	uint64		offset;
	Size		stored;

	check_spill(spill);
	check_partition(spill, partition);
	if (spill->finished)
		elog(ERROR, "Tessera spill set was finished; it takes no more blocks");
	if (body == NULL && len > 0)
		elog(ERROR, "Tessera spilled block has no body");
	header.kind = kind;
	header.number = number;
	header.partition = partition;
	header.level = spill->level;
	header.fingerprint = spill->fingerprint;
	header.len = len;
	header.packed = 0;
	at = spill->buffer + spill->buffered + sizeof(bytes);
	/*
	 * A chunk of columns always goes packed, into the buffer when its bytes
	 * and the slack fit what is left there, else into an empty buffer, and
	 * aside only when larger than the buffer; its header names the chunk
	 * it unpacks into, of its rows only.
	 */
	if (kind == TESS_SPILL_COLUMNS)
	{
		Size		packed;
		Size		unpacked;
		Size		room = len + TESS_SPILL_COLUMNS_SLACK(tess_spill_columns_words(body));
		char	   *out;

		if (spill->buffered + sizeof(bytes) + room > spill->buffer_len &&
			sizeof(bytes) + room <= spill->buffer_len)
		{
			flush(spill);
			at = spill->buffer + sizeof(bytes);
		}
		in_buffer = spill->buffered + sizeof(bytes) + room <= spill->buffer_len;
		out = in_buffer ? at : scratch(spill, room);

		if (spill->kernels->spill_columns_pack == NULL)
			elog(ERROR, "Tessera spill set requires the kernels of chunks of columns");
		if (spill->kernels->spill_columns_pack(body, len, out, room, &packed, &unpacked,
											   &status) != TESS_OK)
			tess_status_report(&status);
		if (packed > PG_UINT32_MAX)
			elog(ERROR, "Tessera chunk of columns packs into too many bytes");
		header.len = unpacked;
		header.packed = (uint32) packed;
		body = out;
		len = packed;
	}
	/* The header is checked before anything is written. */
	if (spill->kernels->spill_header_write(bytes, sizeof(bytes), &header,
										   spill->max_len, &status) != TESS_OK)
		tess_status_report(&status);
	/*
	 * A chunk of records goes packed when that makes it shorter: straight
	 * into the buffer when its raw bytes fit what is left there, else
	 * aside first.
	 */
	pack = kind == TESS_SPILL_RECORDS && len >= 64 && len <= PG_UINT32_MAX;
	if (pack)
	{
		Size		packed;
		bool		fits = spill->buffered + sizeof(bytes) + len <= spill->buffer_len;
		char	   *out = fits ? at : scratch(spill, len);

		if (spill->kernels->spill_pack(body, len, out, len, &packed,
									   &status) != TESS_OK)
			tess_status_report(&status);
		if (packed > 0)
		{
			header.packed = (uint32) packed;
			if (spill->kernels->spill_header_write(bytes, sizeof(bytes), &header,
												   spill->max_len, &status) != TESS_OK)
				tess_status_report(&status);
			in_buffer = fits;
			body = out;
			len = packed;
		}
	}
	stored = sizeof(bytes) + len;
	offset = spill->end;
	/*
	 * A block that does not fit what the buffer has left starts a new
	 * buffer. One packed in place fit, so only a block aside comes here.
	 */
	if (spill->buffered + stored > spill->buffer_len)
	{
		Assert(!in_buffer);
		flush(spill);
		at = spill->buffer + sizeof(bytes);
	}
	if (stored <= spill->buffer_len)
	{
		memcpy(spill->buffer + spill->buffered, bytes, sizeof(bytes));
		if (len > 0 && !in_buffer)
			memcpy(at, body, len);
		spill->buffered += stored;
	}
	else
	{
		/* Larger than the buffer, which is empty now: written as it is. */
		write_at(set_file(spill), (const char *) bytes, sizeof(bytes), offset);
		if (len > 0)
			write_at(spill->file, body, len, offset + sizeof(bytes));
	}
	spill->end += stored;
	add_block(spill, partition, offset, stored);
	if (position != NULL)
		position->offset = (int64) offset;
	spill->blocks++;
	spill->bytes += stored;
	return stored;
}

void
tess_spill_finish(TessSpill *spill)
{
	check_spill(spill);
	if (spill->finished)
		return;
	flush(spill);
	/* A shared set's list of blocks, for the other participants, after its blocks. */
	if (spill->shared != NULL && spill->file > 0)
	{
		SpillTrailer trailer;
		uint64		offset = spill->end;

		trailer.magic = SPILL_TRAILER_MAGIC;
		trailer.offset = offset;
		trailer.npartitions = (uint64) spill->npartitions;
		trailer.fingerprint = spill->fingerprint;
		for (int partition = 0; partition < spill->npartitions; partition++)
		{
			write_at(spill->file, (const char *) &spill->lists[partition].count,
					 sizeof(uint64), offset);
			offset += sizeof(uint64);
		}
		for (int partition = 0; partition < spill->npartitions; partition++)
		{
			Size		len = mul_size(spill->lists[partition].count, sizeof(SpillBlock));

			if (len > 0)
				write_at(spill->file, (const char *) spill->lists[partition].blocks, len, offset);
			offset += len;
		}
		write_at(spill->file, (const char *) &trailer, sizeof(trailer), offset);
	}
	if (spill->buffer != NULL)
		pfree(spill->buffer);
	spill->buffer = NULL;
	spill->finished = true;
}

/* The lists at the end of a shared file do not hold: damaged data. */
pg_noreturn static void
list_damaged(void)
{
	ereport(ERROR,
			errcode(ERRCODE_DATA_CORRUPTED),
			errmsg("Tessera spill file's list of blocks is damaged"));
}

/*
 * Read another participant's list of the partition's blocks from the end
 * of its file. Nothing read from the file is trusted past what the file
 * can hold: the lists lie between the blocks and the trailer, a count for
 * each partition and then the blocks' entries, and every block lies whole
 * before the lists.
 */
static bool
read_trailer(TessSpill *spill, File file, int partition, TessSpillReader *reader)
{
	SpillTrailer trailer;
	pgoff_t		size = FileSize(file);
	uint64		counts = sizeof(uint64) * (uint64) spill->npartitions;
	uint64		lists;
	uint64		most;
	uint64		before = 0;
	uint64		total = 0;
	uint64		count;
	uint64	   *all;

	if (size < 0)
		ereport(ERROR,
				errcode_for_file_access(),
				errmsg("could not size Tessera spill file: %m"));
	if ((uint64) size < sizeof(trailer))
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("Tessera spill file has no list of its blocks"));
	read_at(file, (char *) &trailer, sizeof(trailer), (uint64) size - sizeof(trailer));
	if (trailer.magic != SPILL_TRAILER_MAGIC ||
		trailer.npartitions != (uint64) spill->npartitions ||
		trailer.offset > (uint64) size - sizeof(trailer))
		list_damaged();
	if (trailer.fingerprint != spill->fingerprint)
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("Tessera spill file belongs to another table"));
	lists = (uint64) size - sizeof(trailer) - trailer.offset;
	if (lists < counts)
		list_damaged();
	/*
	 * The blocks the lists have room for. The counts of all the partitions
	 * are read at once and must fill that room exactly: a writer puts
	 * nothing else between its blocks and its trailer.
	 */
	if ((lists - counts) % sizeof(SpillBlock) != 0)
		list_damaged();
	most = (lists - counts) / sizeof(SpillBlock);
	all = palloc(counts);
	read_at(file, (char *) all, counts, trailer.offset);
	for (int other = 0; other < spill->npartitions; other++)
	{
		if (all[other] > most - total)
			list_damaged();
		if (other < partition)
			before += all[other];
		total += all[other];
	}
	count = all[partition];
	pfree(all);
	if (total != most)
		list_damaged();
	if (count == 0)
		return false;
	reader->blocks = MemoryContextAllocExtended(spill->context,
												mul_size(count, sizeof(SpillBlock)),
												MCXT_ALLOC_HUGE);
	reader->count = count;
	read_at(file, (char *) reader->blocks, mul_size(count, sizeof(SpillBlock)),
			trailer.offset + counts + sizeof(SpillBlock) * before);
	/*
	 * A partition's blocks lie in the order they were written, each whole
	 * before the next of them, and the last before the lists.
	 */
	for (uint64 index = 0; index < count; index++)
	{
		const SpillBlock *block = &reader->blocks[index];
		uint64		end = index + 1 < count ? reader->blocks[index + 1].offset : trailer.offset;

		if (end > trailer.offset || block->offset > end ||
			block->stored > end - block->offset)
			list_damaged();
	}
	return true;
}

TessSpillReader *
tess_spill_open(TessSpill *spill, int participant, int partition)
{
	TessSpillReader *reader;
	File		file;
	MemoryContext old;
	char		name[MAXPGPATH];

	check_spill(spill);
	check_partition(spill, partition);
	if (!spill->finished)
		elog(ERROR, "Tessera spill set is read only after it was finished");
	if (spill->shared == NULL && participant != 0)
		elog(ERROR, "Tessera serial spill set has only its own files");
	if (participant < 0)
		elog(ERROR, "Tessera spill participant %d is invalid", participant);
	reader = MemoryContextAllocZero(spill->context, sizeof(TessSpillReader));
	reader->spill = spill;
	reader->partition = partition;
	if (participant == spill->participant)
	{
		if (spill->lists[partition].count == 0)
		{
			pfree(reader);
			return NULL;
		}
		if (spill->shared == NULL && spill->reading[partition] > 0)
			elog(ERROR, "Tessera serial spill file of partition %d is already being read",
				 partition);
		spill->reading[partition]++;
		reader->file = spill->file;
		reader->own = true;
		reader->blocks = spill->lists[partition].blocks;
		reader->count = spill->lists[partition].count;
	}
	else
	{
		/* Another participant's file, a handle of its own, and its list of the partition's blocks. */
		file_name(spill, participant, name);
		old = MemoryContextSwitchTo(spill->context);
		file = FileSetOpen(&spill->shared->fs, name, O_RDONLY);
		MemoryContextSwitchTo(old);
		if (file <= 0)
		{
			pfree(reader);
			return NULL;
		}
		reader->file = file;
		if (!read_trailer(spill, file, partition, reader))
		{
			FileClose(file);
			pfree(reader);
			return NULL;
		}
	}
	/* A buffer of the largest block, from the start. */
	for (uint64 index = 0; index < reader->count; index++)
	{
		uint64		stored = reader->blocks[index].stored;

		/*
		 * A block is a header and at most the longest body, or the packed
		 * form of a chunk of columns, which may be longer than its body by
		 * the room of its descriptors.
		 */
		uint64		extra = TESS_SPILL_HEADER_SIZE +
			TESS_SPILL_COLUMNS_SLACK(TESS_SPILL_COLUMNS_MAX_WORDS);

		if (stored < TESS_SPILL_HEADER_SIZE ||
			(spill->max_len <= PG_UINT64_MAX - extra && stored > spill->max_len + extra))
			ereport(ERROR,
					errcode(ERRCODE_DATA_CORRUPTED),
					errmsg("Tessera spilled block of " UINT64_FORMAT " bytes is out of range",
						   stored));
		reader->buffer_len = Max(reader->buffer_len, TYPEALIGN(8, (Size) stored));
	}
	reader->buffer = MemoryContextAllocExtended(spill->context, Max(reader->buffer_len, 8),
												MCXT_ALLOC_HUGE);
	reader->next_reader = spill->readers;
	spill->readers = reader;
	return reader;
}

static void
check_reader(const TessSpillReader *reader)
{
	if (reader == NULL || reader->file <= 0)
		elog(ERROR, "Tessera spill reader is missing or closed");
}

bool
tess_spill_read_header(TessSpillReader *reader, TessSpillHeader *header)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	TessSpill  *spill;
	SpillBlock *block;
	Size		stored;

	check_reader(reader);
	if (header == NULL)
		elog(ERROR, "Tessera spill reader requires a header");
	if (reader->pending)
		elog(ERROR, "Tessera spilled block's body was not read");
	spill = reader->spill;
	if (reader->next >= reader->count)
		return false;
	block = &reader->blocks[reader->next];
	stored = (Size) block->stored;
	/* The whole block in one read: its header, then its stored body. */
	Assert(stored <= reader->buffer_len);
	read_at(reader->file, reader->buffer, stored, block->offset);
	if (spill->kernels->spill_header_read(reader->buffer, TESS_SPILL_HEADER_SIZE,
										  spill->fingerprint, spill->max_len,
										  header, &status) != TESS_OK)
		tess_status_report(&status);
	if (header->partition != (uint32) reader->partition ||
		header->level != spill->level)
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("Tessera spilled block of partition %u at level %u is in the file of partition %d at level %u",
					   header->partition, header->level, reader->partition,
					   spill->level));
	if ((header->packed > 0 ? header->packed : header->len) + TESS_SPILL_HEADER_SIZE != stored)
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("Tessera spilled block's header does not match its length on disk"));

	/*
	 * A packed body begins with the counts of what it unpacks into. The
	 * header's body length must be the length they give, before the caller
	 * allocates by it: records and their length, or rows and words a row.
	 */
	if (header->packed > 0)
	{
		const char *packed = reader->buffer + TESS_SPILL_HEADER_SIZE;
		uint32		count;
		uint32		each;
		bool		agrees;

		memcpy(&count, packed, sizeof(count));
		memcpy(&each, packed + sizeof(count), sizeof(each));
		if (header->kind == TESS_SPILL_COLUMNS)
		{
			/* A row takes 8 bytes in every lane; divided, nothing overflows. */
			uint64		row = 8 * ((uint64) tess_spill_columns_null_lanes(each) + each);
			uint64		lanes = header->len - TESS_SPILL_COLUMNS_HEADER;

			agrees = lanes % row == 0 && lanes / row == count;
		}
		else
			agrees = header->len == 8 + (uint64) count * each;
		if (!agrees)
			ereport(ERROR,
					errcode(ERRCODE_DATA_CORRUPTED),
					errmsg("Tessera spilled block's header does not match its packed body"));
	}
	reader->next++;
	reader->pending = true;
	reader->pending_kind = header->kind;
	reader->pending_len = header->len;
	reader->pending_packed = header->packed;
	return true;
}

void
tess_spill_read_body(TessSpillReader *reader, void *body, Size len)
{
	check_reader(reader);
	if (!reader->pending || len != reader->pending_len)
		elog(ERROR, "Tessera spilled block's body is read after its header, whole");
	if (len > 0)
	{
		const char *stored = reader->buffer + TESS_SPILL_HEADER_SIZE;

		if (body == NULL)
			elog(ERROR, "Tessera spilled block's body requires a buffer");
		if (reader->pending_kind == TESS_SPILL_COLUMNS)
		{
			TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

			if (reader->spill->kernels->spill_columns_unpack == NULL)
				elog(ERROR, "Tessera spill set requires the kernels of chunks of columns");
			if (reader->spill->kernels->spill_columns_unpack(stored, reader->pending_packed,
															 body, len, &status) != TESS_OK)
				tess_status_report(&status);
		}
		else if (reader->pending_packed > 0)
		{
			TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

			if (reader->spill->kernels->spill_unpack(stored, reader->pending_packed,
													 body, len, &status) != TESS_OK)
				tess_status_report(&status);
		}
		else
			memcpy(body, stored, len);
	}
	reader->pending = false;
}

void
tess_spill_seek(TessSpillReader *reader, TessSpillPosition position)
{
	uint64		low = 0;
	uint64		high;

	check_reader(reader);
	/* The partition's blocks are in the order of their offsets. */
	high = reader->count;
	while (low < high)
	{
		uint64		middle = low + (high - low) / 2;

		if (reader->blocks[middle].offset < (uint64) position.offset)
			low = middle + 1;
		else
			high = middle;
	}
	if (low >= reader->count ||
		reader->blocks[low].offset != (uint64) position.offset)
		elog(ERROR, "Tessera spill file has no block of partition %d at offset " INT64_FORMAT,
			 reader->partition, position.offset);
	reader->next = low;
	reader->pending = false;
}

void
tess_spill_close(TessSpillReader *reader)
{
	TessSpillReader **link;

	if (reader == NULL)
		return;
	for (link = &reader->spill->readers; *link != NULL; link = &(*link)->next_reader)
		if (*link == reader)
		{
			*link = reader->next_reader;
			break;
		}
	if (reader->own)
		reader->spill->reading[reader->partition]--;
	else
	{
		FileClose(reader->file);
		pfree(reader->blocks);
	}
	if (reader->buffer != NULL)
		pfree(reader->buffer);
	pfree(reader);
}

void
tess_spill_drop(TessSpill *spill, int partition)
{
	SpillList  *list;

	check_spill(spill);
	check_partition(spill, partition);
	list = &spill->lists[partition];
	if (list->count == 0)
		return;
	if (spill->reading[partition] > 0)
		elog(ERROR, "Tessera spill file of partition %d is dropped while read",
			 partition);
	/*
	 * The partition's blocks are forgotten; their bytes stay in the set's
	 * file until the set goes. A shared set's list at the end of the file
	 * has them still, for the others: they drop the partition only once
	 * every one of them is done with it.
	 */
	if (list->blocks != NULL)
		pfree(list->blocks);
	list->blocks = NULL;
	list->count = 0;
	list->slots = 0;
	spill->nfiles--;
}

void
tess_spill_stats(const TessSpill *spill, uint64 *blocks, uint64 *bytes,
				 int *files)
{
	check_spill(spill);
	if (blocks != NULL)
		*blocks = spill->blocks;
	if (bytes != NULL)
		*bytes = spill->bytes;
	if (files != NULL)
		*files = spill->nfiles;
}

Size
tess_spill_memory(const TessSpill *spill)
{
	Size		bytes;

	if (spill == NULL)
		return 0;
	bytes = spill->buffer != NULL ? spill->buffer_len : 0;
	bytes += spill->scratch_len;
	for (TessSpillReader *reader = spill->readers; reader != NULL; reader = reader->next_reader)
		bytes += reader->buffer_len;
	return bytes;
}

void
tess_spill_release(TessSpill *spill)
{
	if (spill == NULL)
		return;
	if (spill->shared == NULL)
	{
		tess_spill_free(spill);
		return;
	}
	while (spill->readers != NULL)
		tess_spill_close(spill->readers);
	/* Closing a file of a set keeps it; the set deletes it. */
	if (spill->file > 0)
		FileClose(spill->file);
	MemoryContextDelete(spill->context);
}

void
tess_spill_free(TessSpill *spill)
{
	char		name[MAXPGPATH];

	if (spill == NULL)
		return;
	while (spill->readers != NULL)
		tess_spill_close(spill->readers);
	/* Closing a serial temporary file deletes it; a shared one is deleted by name. */
	if (spill->file > 0)
	{
		FileClose(spill->file);
		if (spill->shared != NULL)
		{
			file_name(spill, spill->participant, name);
			FileSetDelete(&spill->shared->fs, name, true);
		}
	}
	MemoryContextDelete(spill->context);
}

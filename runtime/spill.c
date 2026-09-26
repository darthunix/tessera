/*
 * Temporary files of a node that spills.
 *
 * A set holds a file per partition of one level of partitioning, created
 * on the partition's first block: a serial set in PostgreSQL's temporary
 * files, a shared one in the participant's files of the query's
 * SharedFileSet. A block is a header, laid out and checked by the kernels
 * (tessera/spill.h), and a chunk of the node's table, written and read
 * whole through the file's buffer. This is the only code that calls
 * PostgreSQL's file layer for spilling; a core with another manager of
 * work files replaces it here. See docs/spill.md, "Files".
 */
#include "postgres.h"

#include <fcntl.h>

#include "commands/tablespace.h"
#include "storage/buffile.h"
#include "utils/memutils.h"

#include "tessera/runtime.h"

struct TessSpill
{
	/* Owns the set, the readers and the files' buffers. */
	MemoryContext context;
	const TessKernelOps *kernels;
	int			npartitions;
	uint32		level;
	uint64		fingerprint;
	uint64		max_len;
	SharedFileSet *shared;
	int			participant;
	char	   *name;
	/* This participant's files by partition, NULL before a block or after a drop. */
	BufFile   **files;
	/* A serial file's reader shares the file's position: one at a time. */
	bool	   *reading;
	bool		finished;
	/* The open readers, closed with the set. */
	TessSpillReader *readers;
	uint64		blocks;
	uint64		bytes;
	int			nfiles;
	/* A buffer for packing and unpacking chunks of records, grown as needed. */
	char	   *scratch;
	Size		scratch_len;
};

struct TessSpillReader
{
	TessSpill  *spill;
	BufFile    *file;
	int			partition;
	/* The file is the set's own handle, not one opened for this reader. */
	bool		own;
	/* The body length of the header just read, until the body is read, and its packed bytes. */
	bool		pending;
	uint64		pending_len;
	uint32		pending_packed;
	TessSpillReader *next;
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
file_name(const TessSpill *spill, int participant, int partition,
		  char name[MAXPGPATH])
{
	snprintf(name, MAXPGPATH, "%s.%d.%d", spill->name, participant, partition);
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

TessSpill *
tess_spill_create(const TessSpillConfig *config)
{
	MemoryContext context;
	TessSpill  *spill;

	if (config == NULL || config->struct_size < TESS_SPILL_CONFIG_MIN_SIZE ||
		config->parent_context == NULL)
		elog(ERROR, "Tessera spill set requires a config and a context");
	if (config->kernels == NULL ||
		!TESS_ABI_HAS_FIELD(config->kernels, TessKernelOps, spill_header_read) ||
		config->kernels->spill_header_write == NULL ||
		config->kernels->spill_header_read == NULL ||
		!TESS_ABI_HAS_FIELD(config->kernels, TessKernelOps, spill_unpack) ||
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
	spill->files = MemoryContextAllocZero(context,
										  mul_size(config->npartitions,
												   sizeof(BufFile *)));
	spill->reading = MemoryContextAllocZero(context,
											mul_size(config->npartitions,
													 sizeof(bool)));
	return spill;
}

static BufFile *
partition_file(TessSpill *spill, int partition)
{
	MemoryContext old;
	char		name[MAXPGPATH];

	if (spill->files[partition] != NULL)
		return spill->files[partition];
	old = MemoryContextSwitchTo(spill->context);
	if (spill->shared == NULL)
		spill->files[partition] = BufFileCreateTemp(false);
	else
	{
		file_name(spill, spill->participant, partition, name);
		spill->files[partition] = BufFileCreateFileSet(&spill->shared->fs,
													   name);
	}
	MemoryContextSwitchTo(old);
	spill->nfiles++;
	return spill->files[partition];
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
	BufFile    *file;
	int			segment;
	pgoff_t		offset;

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
	/* A chunk of records goes packed when that makes it shorter. */
	if (kind == TESS_SPILL_RECORDS && len >= 64 && len <= PG_UINT32_MAX)
	{
		Size		packed;

		if (spill->kernels->spill_pack(body, len, scratch(spill, len), len, &packed,
									   &status) != TESS_OK)
			tess_status_report(&status);
		if (packed > 0)
		{
			header.packed = (uint32) packed;
			body = spill->scratch;
		}
	}
	if (spill->kernels->spill_header_write(bytes, sizeof(bytes), &header,
										   spill->max_len, &status) != TESS_OK)
		tess_status_report(&status);
	file = partition_file(spill, partition);
	if (position != NULL)
	{
		BufFileTell(file, &segment, &offset);
		position->segment = segment;
		position->offset = offset;
	}
	if (header.packed > 0)
		len = header.packed;
	BufFileWrite(file, bytes, sizeof(bytes));
	if (len > 0)
		BufFileWrite(file, body, len);
	spill->blocks++;
	spill->bytes += sizeof(bytes) + len;
	return sizeof(bytes) + len;
}

void
tess_spill_finish(TessSpill *spill)
{
	check_spill(spill);
	if (spill->finished)
		return;
	if (spill->shared != NULL)
		for (int partition = 0; partition < spill->npartitions; partition++)
			if (spill->files[partition] != NULL)
				BufFileExportFileSet(spill->files[partition]);
	spill->finished = true;
}

static void
rewind_file(BufFile *file)
{
	if (BufFileSeek(file, 0, 0, SEEK_SET) != 0)
		ereport(ERROR,
				errcode_for_file_access(),
				errmsg("could not rewind Tessera spill file: %m"));
}

TessSpillReader *
tess_spill_open(TessSpill *spill, int participant, int partition)
{
	TessSpillReader *reader;
	BufFile    *file;
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
	if (spill->shared == NULL)
	{
		file = spill->files[partition];
		if (file == NULL)
		{
			pfree(reader);
			return NULL;
		}
		if (spill->reading[partition])
			elog(ERROR, "Tessera serial spill file of partition %d is already being read",
				 partition);
		rewind_file(file);
		spill->reading[partition] = true;
		reader->own = true;
	}
	else
	{
		/* A handle of its own, so that readers of one file do not share a position. */
		file_name(spill, participant, partition, name);
		old = MemoryContextSwitchTo(spill->context);
		file = BufFileOpenFileSet(&spill->shared->fs, name, O_RDONLY, true);
		MemoryContextSwitchTo(old);
		if (file == NULL)
		{
			pfree(reader);
			return NULL;
		}
	}
	reader->file = file;
	reader->next = spill->readers;
	spill->readers = reader;
	return reader;
}

static void
check_reader(const TessSpillReader *reader)
{
	if (reader == NULL || reader->file == NULL)
		elog(ERROR, "Tessera spill reader is missing or closed");
}

bool
tess_spill_read_header(TessSpillReader *reader, TessSpillHeader *header)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	uint64		bytes[TESS_SPILL_HEADER_SIZE / sizeof(uint64)];
	TessSpill  *spill;

	check_reader(reader);
	if (header == NULL)
		elog(ERROR, "Tessera spill reader requires a header");
	if (reader->pending)
		elog(ERROR, "Tessera spilled block's body was not read");
	spill = reader->spill;
	if (BufFileReadMaybeEOF(reader->file, bytes, sizeof(bytes), true) == 0)
		return false;
	if (spill->kernels->spill_header_read(bytes, sizeof(bytes),
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
	reader->pending = true;
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
		if (body == NULL)
			elog(ERROR, "Tessera spilled block's body requires a buffer");
		if (reader->pending_packed > 0)
		{
			TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
			TessSpill  *spill = reader->spill;
			char	   *packed = scratch(spill, reader->pending_packed);

			BufFileReadExact(reader->file, packed, reader->pending_packed);
			if (spill->kernels->spill_unpack(packed, reader->pending_packed, body, len,
											 &status) != TESS_OK)
				tess_status_report(&status);
		}
		else
			BufFileReadExact(reader->file, body, len);
	}
	reader->pending = false;
}

void
tess_spill_seek(TessSpillReader *reader, TessSpillPosition position)
{
	check_reader(reader);
	if (BufFileSeek(reader->file, position.segment, (pgoff_t) position.offset,
					SEEK_SET) != 0)
		ereport(ERROR,
				errcode_for_file_access(),
				errmsg("could not seek in Tessera spill file to segment %d, offset " INT64_FORMAT ": %m",
					   position.segment, position.offset));
	reader->pending = false;
}

void
tess_spill_close(TessSpillReader *reader)
{
	TessSpillReader **link;

	if (reader == NULL)
		return;
	for (link = &reader->spill->readers; *link != NULL; link = &(*link)->next)
		if (*link == reader)
		{
			*link = reader->next;
			break;
		}
	if (reader->own)
		reader->spill->reading[reader->partition] = false;
	else
		BufFileClose(reader->file);
	pfree(reader);
}

void
tess_spill_drop(TessSpill *spill, int partition)
{
	char		name[MAXPGPATH];

	check_spill(spill);
	check_partition(spill, partition);
	if (spill->files[partition] == NULL)
		return;
	if (spill->reading[partition])
		elog(ERROR, "Tessera spill file of partition %d is dropped while read",
			 partition);
	/* Closing a serial temporary file deletes it. */
	BufFileClose(spill->files[partition]);
	spill->files[partition] = NULL;
	spill->nfiles--;
	if (spill->shared != NULL)
	{
		file_name(spill, spill->participant, partition, name);
		BufFileDeleteFileSet(&spill->shared->fs, name, true);
	}
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
	for (int partition = 0; partition < spill->npartitions; partition++)
		if (spill->files[partition] != NULL)
			BufFileClose(spill->files[partition]);
	MemoryContextDelete(spill->context);
}

void
tess_spill_free(TessSpill *spill)
{
	if (spill == NULL)
		return;
	while (spill->readers != NULL)
		tess_spill_close(spill->readers);
	for (int partition = 0; partition < spill->npartitions; partition++)
		tess_spill_drop(spill, partition);
	MemoryContextDelete(spill->context);
}

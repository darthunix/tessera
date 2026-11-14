/*
 * Temporary files of a node that spills.
 *
 * A set writes one file, created on its first block: a serial set a
 * PostgreSQL temporary file, a shared one the participant's file of the
 * query's SharedFileSet. The kernels' writer of the set (tessera/spill.h)
 * lays the blocks of every partition out one after another in the set's
 * write buffer, so that the file is written in large pieces however small
 * the blocks; it keeps, per partition, where each of its blocks starts
 * and the bytes it takes, and at the finish it lays those lists and a
 * trailer out at the end of the file, from which every reader takes
 * them, a page at a time. This file writes the ranges the writer names and
 * gives it the memory it asks for. A block is a header, laid out and
 * checked by the kernels, and a chunk of the node's table; a reader reads
 * a block whole with one read. This is the only
 * code that calls PostgreSQL's file layer for spilling; a core with
 * another manager of work files replaces it here. See
 * openspec/specs/spill-format/design.md, "The files".
 */
#include "postgres.h"

#include <fcntl.h>

#include "commands/tablespace.h"
#include "storage/fd.h"
#include "storage/fileset.h"
#include "utils/injection_point.h"
#include "utils/memutils.h"
#include "utils/wait_event.h"

#include "tessera/kernel_ops.h"
#include "tessera/runtime.h"

/* The write buffer when the config gives none. */
#define SPILL_DEFAULT_BUFFER (64 * 1024)

/* Entries of a partition's list a reader reads at a time. */
#define SPILL_PAGE_ENTRIES 256

struct TessSpill
{
	/* Owns the set, the readers and the buffers. */
	MemoryContext context;
	const TessKernelOps *kernels;
	int			npartitions;
	uint32		level;
	uint64		fingerprint;
	/* The longest body of a chunk of records or of columns. */
	uint64		max_chunk;
	/* The bounds a reader of another participant's file takes, once agreed. */
	uint64		agreed_chunk;
	uint64		agreed_values;
	bool		agreed;
	SharedFileSet *shared;
	int			participant;
	char	   *name;
	/* The set's file. */
	File		file;
	/* The kernels' writer of the file, which lays blocks out and keeps their lists. */
	TessSpillWriter writer;
	/* The write buffer, until the set finishes. */
	char	   *buffer;
	Size		buffer_len;
	/* This participant's blocks by partition, the writer's lists, and their bytes. */
	TessSpillList *lists;
	Size		list_bytes;
	/* This participant's readers of each partition: a serial one has one at a time, as ever. */
	int		   *reading;
	bool		finished;
	/* The open readers, closed with the set. */
	TessSpillReader *readers;
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
	/* The kernels' reader of the partition, which checks what it reads. */
	TessSpillReadState state;
	/* The page of the partition's list the reader read last. */
	TessSpillEntry *page;
	Size		page_len;
	/* The block just read, header and stored body, until its body is taken. */
	char	   *buffer;
	Size		buffer_len;
	uint32		kind;
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

/* Of the TOAST pointers only one to data on disk means the same in another process. */
/*
 * tessera-capi sizes a TOAST pointer by these tags and bodies (ondisk_body,
 * varlena_size): its writer copies one, its check ends one.
 */
StaticAssertDecl(VARTAG_INDIRECT == 1 && sizeof(varatt_indirect) == 8 &&
				 VARTAG_ONDISK_OID == 18 && sizeof(varatt_external_oid) == 16 &&
				 VARTAG_ONDISK_OID8 == 4 && sizeof(varatt_external_oid8) == 20,
				 "the TOAST pointers as tessera-capi sizes them");

bool
tess_spill_value_fits_other(const char *value, Size room, int16 typlen)
{
	if (typlen == -2)
		return memchr(value, '\0', room) != NULL;
	if (typlen != -1 || room == 0 || !VARATT_IS_1B_E(value))
		return false;
	return room >= VARHDRSZ_EXTERNAL && VARATT_IS_EXTERNAL_ONDISK(value) &&
		VARSIZE_EXTERNAL(value) <= room;
}

void
tess_spill_expect_kind(const TessSpillHeader *header, TessSpillKind kind)
{
	if (header->kind != (uint32) kind)
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("Tessera read back a spilled block of kind %u where it wrote kind %u",
					   header->kind, (uint32) kind));
}

void
tess_spill_expect_number(const TessSpillHeader *header, uint64 count)
{
	if (header->number >= count)
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("Tessera read back spilled block %u of %llu written",
					   header->number, (unsigned long long) count));
}

void
tess_spill_table_report(const TessStatus *status)
{
	if (status->code == TESS_ERROR_INVALID_ARGUMENT)
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("Tessera read back a damaged chunk of records: %.*s",
					   TESS_STATUS_MESSAGE_SIZE, status->message));
	tess_status_report(status);
}

TessSpill *
tess_spill_create(const TessSpillConfig *config)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	MemoryContext context;
	TessSpill  *spill;

	if (config == NULL || config->struct_size < TESS_SPILL_CONFIG_MIN_SIZE ||
		config->parent_context == NULL)
		elog(ERROR, "Tessera spill set requires a config and a context");
	if (config->kernels == NULL ||
		config->kernels->spill_header_read == NULL ||
		config->kernels->spill_unpack == NULL ||
		config->kernels->spill_set_init == NULL ||
		config->kernels->spill_set_write == NULL ||
		config->kernels->spill_set_written == NULL ||
		config->kernels->spill_set_finish == NULL ||
		config->kernels->spill_set_drop == NULL ||
		config->kernels->spill_set_stats == NULL ||
		config->kernels->spill_reader_own == NULL ||
		config->kernels->spill_reader_file == NULL ||
		config->kernels->spill_reader_open == NULL ||
		config->kernels->spill_reader_next == NULL ||
		config->kernels->spill_reader_body == NULL ||
		config->kernels->spill_reader_seek == NULL)
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
	spill->max_chunk = config->max_chunk;
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
	spill->buffer = MemoryContextAllocExtended(context, spill->buffer_len,
											   MCXT_ALLOC_HUGE | MCXT_ALLOC_ZERO);
	spill->lists = MemoryContextAllocZero(context,
										  mul_size(config->npartitions,
												   sizeof(TessSpillList)));
	spill->reading = MemoryContextAllocZero(context,
											mul_size(config->npartitions,
													 sizeof(int)));
	/* A chunk of values is as long as its rows need: its readers take the longest written. */
	if (spill->kernels->spill_set_init(&spill->writer, spill->npartitions, spill->level,
									   spill->fingerprint, spill->max_chunk,
									   (uint64) MaxAllocHugeSize, spill->buffer_len,
									   spill->shared != NULL, &status) != TESS_OK)
		tess_status_report(&status);
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

/* A list of the partition with room for slots blocks, as the writer asked. */
static void
grow_list(TessSpill *spill, int partition, uint64 slots)
{
	TessSpillList *list = &spill->lists[partition];
	Size		bytes = mul_size((Size) slots, sizeof(TessSpillEntry));
	Size		old = (Size) list->slots * sizeof(TessSpillEntry);

	/* Zeroed past the old entries: the kernels take the whole region as initialized. */
	list->entries = list->entries == NULL ?
		MemoryContextAllocExtended(spill->context, bytes, MCXT_ALLOC_HUGE | MCXT_ALLOC_ZERO) :
		repalloc_huge(list->entries, bytes);
	if (old > 0)
		memset((char *) list->entries + old, 0, bytes - old);
	list->slots = slots;
	spill->list_bytes += bytes - old;
}

/* The set's buffer for packing aside, of at least len bytes, as the writer asked. */
static void
grow_scratch(TessSpill *spill, Size len)
{
	if (spill->scratch_len < len)
	{
		if (spill->scratch != NULL)
			pfree(spill->scratch);
		spill->scratch = MemoryContextAllocExtended(spill->context, len,
													MCXT_ALLOC_HUGE | MCXT_ALLOC_ZERO);
		spill->scratch_len = len;
	}
}

/*
 * Write to the set's file the ranges a step of the writer names, in order,
 * and report them written; body is the block's, when the step writes one.
 */
static void
write_step(TessSpill *spill, const TessSpillStep *step, const void *body)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	for (uint32 index = 0; index < step->nranges; index++)
	{
		const TessSpillRange *range = &step->ranges[index];
		const char *from;

		switch (range->region)
		{
			case TESS_SPILL_REGION_BUFFER:
				from = spill->buffer;
				break;
			case TESS_SPILL_REGION_SCRATCH:
				from = spill->scratch;
				break;
			case TESS_SPILL_REGION_BODY:
				from = body;
				break;
			case TESS_SPILL_REGION_LIST:
				if (range->partition >= (uint32) spill->npartitions)
					elog(ERROR, "Tessera spill set wrote the list of partition %u",
						 range->partition);
				from = (const char *) spill->lists[range->partition].entries;
				break;
			default:
				elog(ERROR, "Tessera spill set wrote from region %u", range->region);
		}
		write_at(set_file(spill), from + range->start, (Size) range->len, range->at);
	}
	if (step->nranges > 0 &&
		spill->kernels->spill_set_written(&spill->writer, &status) != TESS_OK)
		tess_status_report(&status);
}

/*
 * The writer lays the block out in the buffer, packed when that pays, and
 * notes it in its partition's list; a step writes the buffer when the block
 * does not fit what it has left, or a block larger than the buffer on its
 * own. The regions it asks for come from the set's context.
 */
Size
tess_spill_write(TessSpill *spill, int partition, TessSpillKind kind,
				 uint32 number, const void *body, Size len,
				 TessSpillPosition *position)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	TessSpillStep step;

	check_spill(spill);
	check_partition(spill, partition);
	if (spill->finished)
		elog(ERROR, "Tessera spill set was finished; it takes no more blocks");
	if (body == NULL && len > 0)
		elog(ERROR, "Tessera spilled block has no body");
	for (;;)
	{
		if (spill->kernels->spill_set_write(&spill->writer, spill->buffer, spill->buffer_len,
											spill->scratch, spill->scratch_len,
											spill->lists, spill->npartitions, partition,
											kind, number, body, len, &step,
											&status) != TESS_OK)
			tess_status_report(&status);
		if (step.kind == TESS_SPILL_STEP_SCRATCH)
			grow_scratch(spill, (Size) step.need);
		else if (step.kind == TESS_SPILL_STEP_LIST)
			grow_list(spill, partition, step.need);
		else
		{
			write_step(spill, &step, body);
			if (step.kind == TESS_SPILL_STEP_DONE)
				break;
		}
	}
	if (position != NULL)
		position->offset = (int64) step.offset;
	return (Size) step.stored;
}

/* What the buffer holds, then the set's lists and trailer, for its readers. */
void
tess_spill_finish(TessSpill *spill)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	TessSpillStep step;

	check_spill(spill);
	if (spill->finished)
		return;
	do
	{
		if (spill->kernels->spill_set_finish(&spill->writer, spill->buffer, spill->buffer_len,
											 spill->lists, spill->npartitions, &step,
											 &status) != TESS_OK)
			tess_status_report(&status);
		if (step.kind != TESS_SPILL_STEP_AGAIN && step.kind != TESS_SPILL_STEP_DONE)
			elog(ERROR, "Tessera spill set's finish asked for step %u", step.kind);
		write_step(spill, &step, NULL);
	} while (step.kind != TESS_SPILL_STEP_DONE);
	if (spill->buffer != NULL)
		pfree(spill->buffer);
	spill->buffer = NULL;
	/* Nothing is packed aside after the writes. */
	if (spill->scratch != NULL)
		pfree(spill->scratch);
	spill->scratch = NULL;
	spill->scratch_len = 0;
	/* The lists are at the end of the file now, which readers take a page at a time. */
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		TessSpillList *list = &spill->lists[partition];

		if (list->entries != NULL)
			pfree(list->entries);
		list->entries = NULL;
		list->slots = 0;
	}
	spill->list_bytes = 0;
	spill->finished = true;
}

/* Read into the reader's page or block buffer what a step of its reader asks. */
static void
read_step(TessSpillReader *reader, const TessSpillReadStep *step)
{
	if (step->kind == TESS_SPILL_READ_PAGE)
	{
		if (step->len > reader->page_len * sizeof(TessSpillEntry))
			elog(ERROR, "Tessera spill reader asked for a page of " UINT64_FORMAT " bytes",
				 step->len);
		read_at(reader->file, (char *) reader->page, (Size) step->len, step->at);
	}
	else if (step->kind == TESS_SPILL_READ_BLOCK)
	{
		if (step->len > reader->buffer_len)
			elog(ERROR, "Tessera spill reader asked for a block of " UINT64_FORMAT " bytes",
				 step->len);
		read_at(reader->file, reader->buffer, (Size) step->len, step->at);
	}
	else
		elog(ERROR, "Tessera spill reader took step %u where it reads", step->kind);
}

/*
 * The kernels' reader checks what it reads, of the set's own file or of
 * another participant's: the trailer at its end, the counts and the
 * partition's entries, a page at a time, each before what it names is
 * read. The reader's block buffer is as long as the partition's largest
 * block, from the start.
 */
TessSpillReader *
tess_spill_open(TessSpill *spill, int participant, int partition)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	TessSpillReader *reader;
	TessSpillReadStep step;
	MemoryContext old;
	char		name[MAXPGPATH];
	uint64		max_chunk;
	uint64		max_values;
	pgoff_t		size;

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
		/* Its count stays in memory: a partition dropped since the finish is gone. */
		if (spill->lists[partition].count == 0)
		{
			pfree(reader);
			return NULL;
		}
		if (spill->shared == NULL && spill->reading[partition] > 0)
			elog(ERROR, "Tessera serial spill file of partition %d is already being read",
				 partition);
		reader->file = spill->file;
		reader->own = true;
		tess_spill_bounds(spill, &max_chunk, &max_values);
	}
	else
	{
		if (!spill->agreed)
			elog(ERROR, "Tessera spill set reads another participant's file only after their bounds are agreed");
		/* Another participant's file, a handle of its own. */
		file_name(spill, participant, name);
		old = MemoryContextSwitchTo(spill->context);
		reader->file = FileSetOpen(&spill->shared->fs, name, O_RDONLY);
		MemoryContextSwitchTo(old);
		if (reader->file <= 0)
		{
			pfree(reader);
			return NULL;
		}
		max_chunk = spill->agreed_chunk;
		max_values = spill->agreed_values;
	}
	size = FileSize(reader->file);
	if (size < 0)
		ereport(ERROR,
				errcode_for_file_access(),
				errmsg("could not size Tessera spill file: %m"));
	if (spill->kernels->spill_reader_file(&reader->state, spill->npartitions,
										  spill->level, partition, spill->fingerprint,
										  max_chunk, max_values, (uint64) size,
										  &status) != TESS_OK)
		tess_status_report(&status);
	reader->page_len = SPILL_PAGE_ENTRIES;
	reader->page = MemoryContextAllocZero(spill->context,
										  sizeof(TessSpillEntry) * SPILL_PAGE_ENTRIES);
	for (;;)
	{
		if (spill->kernels->spill_reader_open(&reader->state, reader->page,
											  reader->page_len, &step,
											  &status) != TESS_OK)
			tess_status_report(&status);
		if (step.kind == TESS_SPILL_READ_EMPTY)
		{
			/* The set's own blocks, which its count in memory names, are not lost. */
			if (reader->own)
				ereport(ERROR,
						errcode(ERRCODE_DATA_CORRUPTED),
						errmsg("Tessera spill file lists no block of partition %d", partition));
			FileClose(reader->file);
			pfree(reader->page);
			pfree(reader);
			return NULL;
		}
		if (step.kind == TESS_SPILL_READ_READY)
			break;
		read_step(reader, &step);
	}
	if (reader->own)
		spill->reading[partition]++;
	reader->buffer_len = TYPEALIGN(8, (Size) step.largest);
	reader->buffer = MemoryContextAllocExtended(spill->context, Max(reader->buffer_len, 8),
												MCXT_ALLOC_HUGE | MCXT_ALLOC_ZERO);
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

/* The next block whole, its header checked by the kernels' reader. */
bool
tess_spill_read_header(TessSpillReader *reader, TessSpillHeader *header)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	TessSpillReadStep step;
	TessSpill  *spill;

	check_reader(reader);
	if (header == NULL)
		elog(ERROR, "Tessera spill reader requires a header");
	spill = reader->spill;
	for (;;)
	{
		if (spill->kernels->spill_reader_next(&reader->state, reader->page, reader->page_len,
											  reader->buffer, reader->buffer_len, &step,
											  &status) != TESS_OK)
			tess_status_report(&status);
		if (step.kind == TESS_SPILL_READ_END)
			return false;
		if (step.kind == TESS_SPILL_READ_HEADER)
			break;
		read_step(reader, &step);
	}
	*header = step.header;
	reader->kind = header->kind;
	/* A test damages here a header a node reads, past the reader's checks. */
	INJECTION_POINT("tessera-spill-header", header);
	return true;
}

void
tess_spill_read_body(TessSpillReader *reader, void *body, Size len)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	check_reader(reader);
	if (body == NULL && len > 0)
		elog(ERROR, "Tessera spilled block's body requires a buffer");
	if (reader->spill->kernels->spill_reader_body(&reader->state, reader->buffer,
												  reader->buffer_len, body, len,
												  &status) != TESS_OK)
		tess_status_report(&status);
#ifdef USE_INJECTION_POINTS
	if (len > 0)
	{
		TessSpillBodyRead read = {(TessSpillKind) reader->kind, body, len};

		/* A test damages here a body a node reads, past the reader's checks. */
		INJECTION_POINT("tessera-spill-body", &read);
	}
#endif
}

void
tess_spill_seek(TessSpillReader *reader, TessSpillPosition position)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	TessSpillReadStep step;

	check_reader(reader);
	for (;;)
	{
		if (reader->spill->kernels->spill_reader_seek(&reader->state, reader->page,
													  reader->page_len,
													  (uint64) position.offset, &step,
													  &status) != TESS_OK)
			tess_status_report(&status);
		if (step.kind == TESS_SPILL_READ_FOUND)
			break;
		read_step(reader, &step);
	}
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
		FileClose(reader->file);
	pfree(reader->page);
	if (reader->buffer != NULL)
		pfree(reader->buffer);
	pfree(reader);
}

void
tess_spill_drop(TessSpill *spill, int partition)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	TessSpillList *list;

	check_spill(spill);
	check_partition(spill, partition);
	/* The list at the end of the file, written at the finish, would lose the blocks. */
	if (spill->shared != NULL && !spill->finished)
		elog(ERROR, "Tessera shared spill set dropped partition %d before it was finished",
			 partition);
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
	if (spill->kernels->spill_set_drop(&spill->writer, spill->lists, spill->npartitions,
									   partition, &status) != TESS_OK)
		tess_status_report(&status);
	if (list->entries != NULL)
		pfree(list->entries);
	spill->list_bytes -= (Size) list->slots * sizeof(TessSpillEntry);
	list->entries = NULL;
	list->slots = 0;
}

void
tess_spill_stats(const TessSpill *spill, uint64 *blocks, uint64 *bytes,
				 int *files)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	check_spill(spill);
	if (spill->kernels->spill_set_stats(&spill->writer, blocks, bytes, files, NULL,
										&status) != TESS_OK)
		tess_status_report(&status);
}

void
tess_spill_bounds(const TessSpill *spill, uint64 *max_chunk, uint64 *max_values)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	check_spill(spill);
	if (max_chunk != NULL)
		*max_chunk = spill->max_chunk;
	if (spill->kernels->spill_set_stats(&spill->writer, NULL, NULL, NULL, max_values,
										&status) != TESS_OK)
		tess_status_report(&status);
}

void
tess_spill_agree(TessSpill *spill, uint64 max_chunk, uint64 max_values)
{
	uint64		blocks;
	uint64		own_chunk;
	uint64		own_values;

	check_spill(spill);
	if (spill->shared == NULL)
		elog(ERROR, "Tessera serial spill set has no participants to agree with");
	/* Every participant that wrote a block gave its bounds to the agreement. */
	tess_spill_stats(spill, &blocks, NULL, NULL);
	tess_spill_bounds(spill, &own_chunk, &own_values);
	if (blocks > 0 && (max_chunk < own_chunk || max_values < own_values))
		elog(ERROR, "Tessera spill set agreed on bounds of " UINT64_FORMAT " and " UINT64_FORMAT
			 " bytes, below its own " UINT64_FORMAT " and " UINT64_FORMAT,
			 max_chunk, max_values, own_chunk, own_values);
	spill->agreed_chunk = max_chunk;
	spill->agreed_values = max_values;
	spill->agreed = true;
}

Size
tess_spill_memory(const TessSpill *spill)
{
	Size		bytes;

	if (spill == NULL)
		return 0;
	bytes = spill->buffer != NULL ? spill->buffer_len : 0;
	bytes += spill->scratch_len + spill->list_bytes;
	for (TessSpillReader *reader = spill->readers; reader != NULL; reader = reader->next_reader)
		bytes += reader->buffer_len + sizeof(TessSpillEntry) * reader->page_len;
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

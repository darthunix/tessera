/*
 * The one writer and the one reader of chunks of columns of the nodes
 * (tessera/runtime_spill.h): their chunk and values in memory contexts,
 * the pair of blocks a chunk is written as in a set, and the check of a
 * chunk read back (tess_spill_columns_check), the format's and each
 * value's by its type, in one pass.
 */
#include "postgres.h"

#include "utils/memutils.h"

#include "tessera/kernel_ops.h"
#include "tessera/runtime.h"

void
tess_columns_writer_reset(TessColumnsWriter *writer)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	Size		capacity;

	tess_status_check(writer->kernels->spill_columns_init(writer->chunk, writer->chunk_len,
														  writer->words, &capacity, &status),
					  &status);
	/* A chunk of no row would take no row however its values grew. */
	if (capacity == 0)
		elog(ERROR, "Tessera writer of a chunk of columns of %zu bytes holds no row of %d words",
			 writer->chunk_len, writer->words);
	writer->values_used = 0;
}

void
tess_columns_writer_init(TessColumnsWriter *writer, const TessKernelOps *kernels,
						 MemoryContext context, Size chunk_len, int words, int ncolumns,
						 const bool *byvals, const int16 *typlens, Size values_len)
{
	writer->kernels = kernels;
	writer->chunk_len = chunk_len;
	writer->chunk = MemoryContextAllocExtended(context, chunk_len, MCXT_ALLOC_HUGE);
	writer->values_len = Max(values_len, 8);
	writer->values = MemoryContextAllocExtended(context, writer->values_len, MCXT_ALLOC_HUGE);
	writer->words = words;
	writer->ncolumns = ncolumns;
	writer->byvals = byvals;
	writer->typlens = typlens;
	tess_columns_writer_reset(writer);
}

int
tess_columns_writer_add(TessColumnsWriter *writer, const TessDatumColumn *columns,
						TessRowMask *pending)
{
	int			total = 0;

	for (;;)
	{
		TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
		int			appended;
		Size		need;

		tess_status_check(writer->kernels->spill_columns_append(writer->chunk, writer->chunk_len,
																writer->ncolumns, columns,
																writer->byvals, writer->typlens,
																pending, writer->values,
																writer->values_len,
																&writer->values_used,
																&appended, &need, &status),
						  &status);
		total += appended;
		if (tess_row_mask_count(pending) == 0 || tess_columns_writer_rows(writer) > 0)
			return total;
		/*
		 * An empty chunk: the next row's values pass its chunk of values,
		 * which grows to the 4 GiB a reference names at most; a row whose
		 * values pass even that is an ERROR, not a loop.
		 */
		if (need > ((Size) 1 << TESS_SPILL_VALUE_BYTE_BITS) - writer->values_used)
			elog(ERROR, "Tessera writer of a chunk of columns takes no row of %zu bytes of values",
				 need);
		writer->values_len = Min(Max(writer->values_len * 2, writer->values_used + need),
								 (Size) 1 << TESS_SPILL_VALUE_BYTE_BITS);
		writer->values = repalloc_huge(writer->values, writer->values_len);
	}
}

Size
tess_columns_writer_write(TessColumnsWriter *writer, TessSpill *spill, int partition,
						  uint32 number, TessSpillPosition *position)
{
	Size		bytes;

	bytes = tess_spill_write(spill, partition, TESS_SPILL_VALUES, number, writer->values,
							 writer->values_used, position);
	bytes += tess_spill_write(spill, partition, TESS_SPILL_COLUMNS, number, writer->chunk,
							  writer->chunk_len, NULL);
	tess_columns_writer_reset(writer);
	return bytes;
}

void
tess_columns_writer_free(TessColumnsWriter *writer)
{
	pfree(writer->chunk);
	pfree(writer->values);
	writer->chunk = NULL;
	writer->values = NULL;
}

void
tess_columns_reader_init(TessColumnsReader *reader, const TessKernelOps *kernels,
						 MemoryContext context, int words, int ncolumns, const bool *byvals,
						 const int16 *typlens)
{
	memset(reader, 0, sizeof(TessColumnsReader));
	reader->kernels = kernels;
	reader->context = context;
	reader->words = words;
	reader->ncolumns = ncolumns;
	reader->byvals = byvals;
	reader->typlens = typlens;
}

/* Room for len bytes at *buffer of *capacity, grown in context to twice at least. */
static void
reader_room(MemoryContext context, void **buffer, Size *capacity, Size len)
{
	if (*buffer != NULL && len <= *capacity)
		return;
	*capacity = Max(Max(len, 8), *capacity * 2);
	if (*buffer != NULL)
		pfree(*buffer);
	*buffer = MemoryContextAllocExtended(context, *capacity, MCXT_ALLOC_HUGE);
}

/* The next block of the pair: block at, numbered number, its header in *header. */
static void
reader_block(TessColumnsReader *reader, TessSpillReader *file, int at, uint32 number,
			 TessSpillHeader *header)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	if (!tess_spill_read_header(file, header))
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("Tessera read back the values of a chunk of columns without its rows"));
	tess_status_check(reader->kernels->spill_columns_pair(header, at, number, &status),
					  &status);
}

bool
tess_columns_reader_load(TessColumnsReader *reader, TessSpillReader *file, uint32 number,
						 int64 rows)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	TessSpillHeader header;

	/* Nothing to show until the pair is read and checked whole. */
	reader->rows = 0;
	reader->chunk = NULL;
	reader->values = NULL;
	if (!tess_spill_read_header(file, &header))
		return false;
	tess_status_check(reader->kernels->spill_columns_pair(&header, 0, number, &status),
					  &status);
	reader_room(reader->context, (void **) &reader->own_values, &reader->own_values_len,
				header.len);
	tess_spill_read_body(file, reader->own_values, header.len);
	reader->values_used = header.len;
	reader_block(reader, file, 1, number, &header);
	reader_room(reader->context, &reader->own_chunk, &reader->own_chunk_len, header.len);
	tess_spill_read_body(file, reader->own_chunk, header.len);
	reader->chunk_used = header.len;
	tess_status_check(reader->kernels->spill_columns_check(reader->own_chunk,
														   reader->chunk_used,
														   reader->words, rows,
														   reader->ncolumns, reader->byvals,
														   reader->typlens, reader->own_values,
														   reader->values_used, &status),
					  &status);
	reader->chunk = reader->own_chunk;
	reader->values = reader->own_values;
	reader->rows = tess_spill_columns_rows(reader->chunk);
	return true;
}

void
tess_columns_reader_show(TessColumnsReader *reader, void *chunk, Size chunk_len, char *values,
						 Size values_len)
{
	reader->chunk = chunk;
	reader->chunk_used = chunk_len;
	reader->values = values;
	reader->values_used = values_len;
	reader->rows = tess_spill_columns_rows(chunk);
}

void
tess_columns_reader_window(TessColumnsReader *reader, uint32 first, uint32 count,
						   Datum *const *values, bool *const *isnull)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	tess_status_check(reader->kernels->spill_columns_window(reader->chunk, reader->chunk_used,
															reader->ncolumns, reader->byvals,
															reader->values, first, count,
															values, isnull, &status),
					  &status);
}

void
tess_columns_reader_detach(TessColumnsReader *reader, void **chunk, char **values)
{
	*chunk = reader->own_chunk;
	*values = reader->own_values;
	reader->own_chunk = NULL;
	reader->own_chunk_len = 0;
	reader->own_values = NULL;
	reader->own_values_len = 0;
	reader->chunk = NULL;
	reader->chunk_used = 0;
	reader->values = NULL;
	reader->values_used = 0;
	reader->rows = 0;
}

void
tess_columns_reader_free(TessColumnsReader *reader)
{
	void	   *chunk;
	char	   *values;

	tess_columns_reader_detach(reader, &chunk, &values);
	if (chunk != NULL)
		pfree(chunk);
	if (values != NULL)
		pfree(values);
}

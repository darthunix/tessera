/*
 * Rows a node keeps, as records of the kernels' table format.
 *
 * The records lie in chunks the rows allocate, the first of ROWS_FIRST_CHUNK
 * bytes and the others of the most a chunk may have, appended by the
 * kernels from the columns themselves (tess_table_append_columns) and read
 * back by reference, prefetched, since a sort reads them in no order
 * (tess_table_gather_scattered). A record's payload is a word of its kept
 * columns' NULL bits, then a word per column: a by-value Datum, or the reference of
 * a by-reference value's copy in the value chunks, the first of
 * ROWS_VALUE_FIRST bytes and the others of ROWS_VALUE_CHUNK, a value
 * larger than a quarter of one getting a chunk of its own. A reference is
 * the chunk's number plus one and the byte, 0 standing for NULL, as a
 * TessHashJoin's table has it, so that a spilled chunk of either reads the
 * same. The index holds only the layout, which the kernels check on every
 * call: the records are never linked.
 */
#include "postgres.h"

#include "utils/datum.h"
#include "utils/expandeddatum.h"
#include "port/pg_bitutils.h"
#include "utils/memutils.h"

#include "tessera/kernel_ops.h"
#include "tessera/runtime.h"

#define ROWS_FIRST_CHUNK (64 * 1024)
#define ROWS_CHUNK_LEN TESS_TABLE_MAX_CHUNK_LEN
#define ROWS_VALUE_FIRST (64 * 1024)
#define ROWS_VALUE_CHUNK (1024 * 1024)
#define ROWS_VALUE_REF(number, byte) ((((uint64) (number) + 1) << 32) | (uint64) (byte))

struct TessRows
{
	/* Owns everything below; the chunks and values in their own. */
	MemoryContext context;
	MemoryContext chunk_context;
	const TessKernelOps *kernels;
	int			nkeys;
	TessTableKeyKind kinds[TESS_TABLE_MAX_KEYS];
	int			ncolumns;
	int16	   *typlens;
	bool	   *typbyvals;
	Size		payload_size;
	/* The index of the layout, and the chunks by number with room for slots. */
	TessTableRef table;
	void	  **bases;
	Size	   *lens;
	int			slots;
	/* The value chunks by number, with room for value_slots; the one being filled. */
	char	  **values;
	int			nvalues;
	int			value_slots;
	int			value_current;
	Size		value_len;
	Size		value_used;
	/*
	 * Buffers of one append, for capacity rows: zero hashes, the columns the
	 * kernels read the payload from, and for each by-reference column the
	 * references of its copies.
	 */
	int			capacity;
	uint32	   *hashes;
	TessDatumColumn *payload;
	Datum	  **copies;
	uint64	   *pending_bits;
	/* A gather's NULL words, for capacity rows. */
	Datum	   *null_words;
	/* Bit c: kept column c holds a NULL somewhere. */
	uint64		null_columns;
	uint64		records;
	Size		bytes;
	TessStatus	status;
};

static void
check(TessRows *rows, TessStatusCode code)
{
	if (code != TESS_OK)
		tess_status_report(&rows->status);
}

static void
check_rows(const TessRows *rows)
{
	if (rows == NULL)
		elog(ERROR, "Tessera rows are missing");
}

/* The index of the layout alone, and no chunks. */
static void
make_index(TessRows *rows)
{
	Size		size;

	check(rows, rows->kernels->table_size(rows->nkeys, rows->kinds,
										  rows->payload_size, 0, &size,
										  &rows->status));
	rows->table.index = MemoryContextAlloc(rows->chunk_context, size);
	rows->table.index_len = size;
	check(rows, rows->kernels->table_create(rows->table.index, size,
											rows->nkeys, rows->kinds,
											rows->payload_size, 0,
											&rows->status));
	rows->table.nchunks = 0;
	rows->bytes = size;
}

TessRows *
tess_rows_create(const TessRowsConfig *config)
{
	MemoryContext context;
	TessRows   *rows;

	if (config == NULL || config->struct_size < TESS_ROWS_CONFIG_MIN_SIZE ||
		config->parent_context == NULL)
		elog(ERROR, "Tessera rows require a config and a context");
	if (config->kernels == NULL ||
		config->kernels->table_size == NULL ||
		config->kernels->table_create == NULL ||
		config->kernels->table_chunk_init == NULL ||
		!TESS_ABI_HAS_FIELD(config->kernels, TessKernelOps, table_gather_scattered) ||
		config->kernels->table_append_columns == NULL ||
		config->kernels->table_gather_scattered == NULL)
		elog(ERROR, "Tessera rows require the kernels of the table");
	if (config->nkeys < 1 || config->nkeys > TESS_TABLE_MAX_KEYS ||
		config->kinds == NULL)
		elog(ERROR, "Tessera rows take 1..%d keys", TESS_TABLE_MAX_KEYS);
	if (config->ncolumns < 0 || config->ncolumns > TESS_ROWS_MAX_COLUMNS ||
		(config->ncolumns > 0 &&
		 (config->typlens == NULL || config->typbyvals == NULL)))
		elog(ERROR, "Tessera rows keep 0..%d columns with their types",
			 TESS_ROWS_MAX_COLUMNS);
	context = AllocSetContextCreate(config->parent_context, "Tessera rows",
									ALLOCSET_DEFAULT_SIZES);
	rows = MemoryContextAllocZero(context, sizeof(TessRows));
	rows->context = context;
	rows->chunk_context = AllocSetContextCreate(context, "Tessera rows chunks",
												ALLOCSET_DEFAULT_SIZES);
	rows->kernels = config->kernels;
	rows->nkeys = config->nkeys;
	for (int key = 0; key < config->nkeys; key++)
		rows->kinds[key] = config->kinds[key];
	rows->ncolumns = config->ncolumns;
	rows->typlens = MemoryContextAlloc(context, sizeof(int16) * Max(config->ncolumns, 1));
	rows->typbyvals = MemoryContextAlloc(context, sizeof(bool) * Max(config->ncolumns, 1));
	for (int column = 0; column < config->ncolumns; column++)
	{
		rows->typlens[column] = config->typlens[column];
		rows->typbyvals[column] = config->typbyvals[column];
	}
	rows->payload_size = sizeof(uint64) * (1 + config->ncolumns);
	rows->status = (TessStatus) TESS_STRUCT_INITIALIZER(TessStatus);
	rows->slots = 16;
	rows->bases = MemoryContextAlloc(context, sizeof(void *) * rows->slots);
	rows->lens = MemoryContextAlloc(context, sizeof(Size) * rows->slots);
	rows->table.chunks = rows->bases;
	rows->table.chunk_lens = rows->lens;
	rows->value_current = -1;
	make_index(rows);
	return rows;
}

/* Another chunk of records, the last one being full. */
static void
add_chunk(TessRows *rows)
{
	int			chunk = rows->table.nchunks;
	Size		len = chunk == 0 ? ROWS_FIRST_CHUNK : ROWS_CHUNK_LEN;
	void	   *base;

	if (chunk == TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("Tessera rows cannot hold more than %d chunks",
						TESS_TABLE_MAX_CHUNKS)));
	if (chunk == rows->slots)
	{
		rows->slots *= 2;
		rows->bases = repalloc(rows->bases, sizeof(void *) * rows->slots);
		rows->lens = repalloc(rows->lens, sizeof(Size) * rows->slots);
		rows->table.chunks = rows->bases;
		rows->table.chunk_lens = rows->lens;
	}
	base = MemoryContextAlloc(rows->chunk_context, len);
	check(rows, rows->kernels->table_chunk_init(base, len, &rows->status));
	rows->bases[chunk] = base;
	rows->lens[chunk] = len;
	rows->table.nchunks++;
	rows->bytes += len;
}

/* A value chunk of len bytes: its number. */
static int
new_value_chunk(TessRows *rows, Size len)
{
	int			number = rows->nvalues;

	if (number >= INT_MAX - 1)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("Tessera rows cannot hold more chunks of values")));
	if (number == rows->value_slots)
	{
		int			slots = Max(rows->value_slots * 2, 16);

		rows->values = rows->values == NULL ?
			MemoryContextAlloc(rows->context, sizeof(char *) * slots) :
			repalloc(rows->values, sizeof(char *) * slots);
		rows->value_slots = slots;
	}
	rows->values[number] = MemoryContextAllocExtended(rows->chunk_context, len,
													  MCXT_ALLOC_HUGE);
	rows->nvalues++;
	rows->bytes = add_size(rows->bytes, len);
	return number;
}

/*
 * Copy a by-reference value into the value chunks and return its
 * reference: the bytes datumCopy would copy, an expanded object flattened.
 */
static uint64
store_value(TessRows *rows, Datum value, int16 typlen)
{
	ExpandedObjectHeader *expanded = NULL;
	Size		size;
	Size		aligned;
	int			number;
	Size		byte;

	if (typlen == -1 && VARATT_IS_EXTERNAL_EXPANDED(DatumGetPointer(value)))
	{
		expanded = DatumGetEOHP(value);
		size = EOH_get_flat_size(expanded);
	}
	else
		size = datumGetSize(value, false, typlen);
	aligned = MAXALIGN(size);
	if (aligned > ROWS_VALUE_CHUNK / 4)
	{
		number = new_value_chunk(rows, aligned);
		byte = 0;
	}
	else
	{
		if (rows->value_current < 0 ||
			rows->value_used + aligned > rows->value_len)
		{
			/* The first chunk of small values is small, for rows with few. */
			rows->value_len = rows->value_current < 0 ?
				ROWS_VALUE_FIRST : ROWS_VALUE_CHUNK;
			rows->value_current = new_value_chunk(rows, rows->value_len);
			rows->value_used = 0;
		}
		number = rows->value_current;
		byte = rows->value_used;
		rows->value_used += aligned;
	}
	if (expanded != NULL)
		EOH_flatten_into(expanded, rows->values[number] + byte, size);
	else
		memcpy(rows->values[number] + byte, DatumGetPointer(value), size);
	return ROWS_VALUE_REF(number, byte);
}

/* Room in the buffers for nrows rows. */
static void
reserve(TessRows *rows, int nrows)
{
	if (nrows <= rows->capacity)
		return;
	if (rows->hashes != NULL)
	{
		pfree(rows->hashes);
		pfree(rows->pending_bits);
		pfree(rows->null_words);
		for (int column = 0; column < rows->ncolumns; column++)
			if (rows->copies[column] != NULL)
				pfree(rows->copies[column]);
	}
	else
	{
		rows->payload = MemoryContextAllocZero(rows->context,
											   sizeof(TessDatumColumn) *
											   Max(rows->ncolumns, 1));
		rows->copies = MemoryContextAllocZero(rows->context,
											  sizeof(Datum *) * Max(rows->ncolumns, 1));
	}
	rows->capacity = Max(nrows, 64);
	rows->hashes = MemoryContextAllocZero(rows->context,
										  sizeof(uint32) * rows->capacity);
	/* Zeroed: a NULL or unselected row's word is initialized memory. */
	for (int column = 0; column < rows->ncolumns; column++)
		rows->copies[column] = rows->typbyvals[column] ? NULL :
			MemoryContextAllocZero(rows->context, sizeof(Datum) * rows->capacity);
	rows->pending_bits = MemoryContextAlloc(rows->context,
											sizeof(uint64) *
											tess_row_mask_word_count(rows->capacity));
	rows->null_words = MemoryContextAlloc(rows->context,
										  sizeof(Datum) * rows->capacity);
}

/*
 * A full word of rows has its 64 flags scanned at once by memchr; a word
 * with fewer rows selected, as a filter leaves them, has only those
 * looked at.
 */
bool
tess_rows_selected_null(const TessRowMask *mask, const bool *isnull)
{
	int			nwords = tess_row_mask_word_count(mask->nrows);

	for (int word = 0; word < nwords; word++)
	{
		uint64		bits = mask->bits[word];
		const bool *flags = isnull + (Size) word * 64;

		if (bits == ~UINT64CONST(0))
		{
			if (memchr(flags, true, 64) != NULL)
				return true;
			continue;
		}
		for (; bits != 0; bits &= bits - 1)
			if (flags[pg_rightmost_one_pos64(bits)])
				return true;
	}
	return false;
}

/*
 * The records are written by the kernels from the columns themselves: a
 * by-value column as it is, a by-reference one as the references of its
 * values' copies, made here for the selected rows.
 */
void
tess_rows_append(TessRows *rows, const TessTableKey *keys,
				 const TessDatumColumn *columns, const TessRowMask *mask,
				 uint32 *refs)
{
	int			count;
	TessRowMask pending;
	bool		fresh = false;

	check_rows(rows);
	if (mask == NULL || refs == NULL || keys == NULL ||
		(rows->ncolumns > 0 && columns == NULL))
		elog(ERROR, "Tessera rows append requires keys, columns, a mask and references");
	count = tess_row_mask_count(mask);
	if (count == 0)
		return;
	for (int column = 0; column < rows->ncolumns; column++)
		if (columns[column].nrows != mask->nrows)
			elog(ERROR, "Tessera rows column %d has %d rows, not %d",
				 column, columns[column].nrows, mask->nrows);
	reserve(rows, mask->nrows);
	for (int column = 0; column < rows->ncolumns; column++)
	{
		const TessDatumColumn *values = &columns[column];
		TessDatumColumn *payload = &rows->payload[column];
		uint64		bit = UINT64CONST(1) << column;

		if ((rows->null_columns & bit) == 0 &&
			tess_rows_selected_null(mask, values->isnull))
			rows->null_columns |= bit;
		*payload = *values;
		if (!rows->typbyvals[column])
		{
			Datum	   *copies = rows->copies[column];
			int16		typlen = rows->typlens[column];
			int			row = -1;

			while ((row = tess_row_mask_next(mask, row)) >= 0)
				if (!values->isnull[row])
					copies[row] = store_value(rows, values->values[row], typlen);
			payload->values = copies;
		}
	}
	memcpy(rows->pending_bits, mask->bits,
		   sizeof(uint64) * tess_row_mask_word_count(mask->nrows));
	pending = (TessRowMask) {mask->nrows, rows->pending_bits};
	if (rows->table.nchunks == 0)
	{
		add_chunk(rows);
		fresh = true;
	}
	for (;;)
	{
		int			before = tess_row_mask_count(&pending);

		check(rows, rows->kernels->table_append_columns(&rows->table,
														rows->table.nchunks - 1,
														rows->hashes, rows->nkeys,
														keys, rows->ncolumns,
														rows->payload,
														&pending, refs,
														&rows->status));
		if (tess_row_mask_count(&pending) == 0)
			break;
		if (tess_row_mask_count(&pending) == before && fresh)
			elog(ERROR, "Tessera rows cannot fit a row in a chunk");
		add_chunk(rows);
		fresh = true;
	}
	rows->records += count;
}

void
tess_rows_gather(TessRows *rows, int column, const uint32 *refs,
				 const TessRowMask *mask, Datum *values, bool *isnull)
{
	int			nwords;
	char	  **bases;

	check_rows(rows);
	if (column < 0 || column >= rows->ncolumns)
		elog(ERROR, "Tessera rows have no column %d", column);
	if (refs == NULL || mask == NULL || values == NULL || isnull == NULL)
		elog(ERROR, "Tessera rows gather requires references, a mask and outputs");
	nwords = tess_row_mask_word_count(mask->nrows);
	check(rows, rows->kernels->table_gather_scattered(&rows->table, refs, mask,
													  sizeof(uint64) * (1 + column),
													  values, &rows->status));
	/* A by-reference value's word is its reference: its address here. */
	bases = rows->values;
	if (!rows->typbyvals[column])
		for (int word = 0; word < nwords; word++)
			for (uint64 bits = mask->bits[word]; bits != 0; bits &= bits - 1)
			{
				int			row = word * 64 + pg_rightmost_one_pos64(bits);
				uint64		ref = DatumGetUInt64(values[row]);

				if (ref == 0)
					continue;
				Assert((ref >> 32) - 1 < (uint64) rows->nvalues);
				values[row] = PointerGetDatum(bases[(ref >> 32) - 1] +
											  (ref & 0xFFFFFFFF));
			}
	/*
	 * A column no row left NULL needs no bits: every flag is false, the
	 * rows outside the mask's too, which a caller may set to anything.
	 */
	if (((rows->null_columns >> column) & 1) == 0)
	{
		memset(isnull, 0, sizeof(bool) * mask->nrows);
		return;
	}
	reserve(rows, mask->nrows);
	check(rows, rows->kernels->table_gather_scattered(&rows->table, refs, mask, 0,
													  rows->null_words, &rows->status));
	for (int word = 0; word < nwords; word++)
		for (uint64 bits = mask->bits[word]; bits != 0; bits &= bits - 1)
		{
			int			row = word * 64 + pg_rightmost_one_pos64(bits);

			isnull[row] = (DatumGetUInt64(rows->null_words[row]) >> column) & 1;
		}
}

void
tess_rows_sort(TessRows *rows, const TessSortKey *keys, uint32 *refs)
{
	int			words;
	uint64	   *items;
	uint64		count;
	Size		nwords;

	check_rows(rows);
	if (keys == NULL || refs == NULL)
		elog(ERROR, "Tessera rows sort requires keys and references");
	if (!TESS_ABI_HAS_FIELD(rows->kernels, TessKernelOps, sort) ||
		rows->kernels->sort_item_words == NULL ||
		rows->kernels->sort_items == NULL || rows->kernels->sort == NULL)
		elog(ERROR, "Tessera rows sort requires the kernels of the sort");
	if (rows->records == 0)
		return;
	check(rows, rows->kernels->sort_item_words(rows->nkeys, keys, &words,
											   &rows->status));
	nwords = mul_size((Size) rows->records, (Size) words);
	items = MemoryContextAllocExtended(rows->context, mul_size(nwords, sizeof(uint64)),
									   MCXT_ALLOC_HUGE);
	check(rows, rows->kernels->sort_items(&rows->table, rows->nkeys, keys,
										  items, nwords, &count, &rows->status));
	if (count != rows->records)
		elog(ERROR, "Tessera rows hold " UINT64_FORMAT " records, the sort found " UINT64_FORMAT,
			 rows->records, count);
	check(rows, rows->kernels->sort(items, (Size) count, words, refs,
									&rows->status));
	pfree(items);
}

uint64
tess_rows_count(const TessRows *rows)
{
	check_rows(rows);
	return rows->records;
}

Size
tess_rows_memory(const TessRows *rows)
{
	check_rows(rows);
	return rows->bytes;
}

void
tess_rows_reset(TessRows *rows)
{
	check_rows(rows);
	MemoryContextReset(rows->chunk_context);
	rows->nvalues = 0;
	rows->value_current = -1;
	rows->value_len = 0;
	rows->value_used = 0;
	rows->null_columns = 0;
	rows->records = 0;
	make_index(rows);
}

void
tess_rows_free(TessRows *rows)
{
	if (rows == NULL)
		return;
	MemoryContextDelete(rows->context);
}

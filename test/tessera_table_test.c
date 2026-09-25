#include "postgres.h"

#include <string.h>

#include "fmgr.h"

#include "tessera/kernels.h"
#include "tessera/table.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_table_layout);
PG_FUNCTION_INFO_V1(tessera_test_table_cycle);
PG_FUNCTION_INFO_V1(tessera_test_table_groups);
PG_FUNCTION_INFO_V1(tessera_test_table_grow);
PG_FUNCTION_INFO_V1(tessera_test_table_errors);

#define NROWS 200
#define NWORDS 4

static const TessTableKeyKind one_int4[1] = {TESS_TABLE_KEY_INT4};

/*
 * A batch of NROWS int4 keys with every fifth row NULL and each value
 * held by several rows, with its hashes and the valid mask under a NULL
 * policy, and the payload of every row: its row number.
 */
typedef struct Batch
{
	Datum		values[NROWS];
	bool		isnull[NROWS];
	uint64		all[NWORDS];
	uint64		valid[NWORDS];
	uint32		hashes[NROWS];
	uint64		payload[NROWS];
	TessDatumColumn column;
	TessTableKey key;
} Batch;

static int
count_bits(const uint64 *words)
{
	int			count = 0;
	int			word;

	for (word = 0; word < NWORDS; word++)
		count += pg_popcount64(words[word]);
	return count;
}

static bool
has_bit(const uint64 *words, int row)
{
	return (words[row / 64] & (UINT64CONST(1) << (row % 64))) != 0;
}

/*
 * Fill a batch: values (row * 7919) % 50 - 25 + shift, or row % 10 when
 * grouped, and hashes under the policy.
 */
static bool
prepare(Batch *batch, TessNullKeys nulls, int shift, bool grouped)
{
	TessRowMask rows = {NROWS, batch->all};
	TessRowMask valid = {NROWS, batch->valid};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	int			row;

	memset(batch->all, 0, sizeof(batch->all));
	memset(batch->valid, 0, sizeof(batch->valid));
	for (row = 0; row < NROWS; row++)
	{
		int32		value = grouped ? row % 10 : (row * 7919) % 50 - 25 + shift;

		batch->values[row] = Int32GetDatum(value);
		batch->isnull[row] = !grouped && row % 5 == 0;
		batch->all[row / 64] |= UINT64CONST(1) << (row % 64);
		batch->payload[row] = row;
	}
	batch->column.struct_size = sizeof(TessDatumColumn);
	batch->column.values = batch->values;
	batch->column.isnull = batch->isnull;
	batch->column.nrows = NROWS;
	batch->key.kind = TESS_TABLE_KEY_INT4;
	batch->key.column = &batch->column;
	batch->key.prepared = NULL;
	return tess_int4_hash(&batch->column, NULL, &rows, nulls, batch->hashes,
						  &valid, &status) == TESS_OK;
}

/* A region for capacity records of one int4 key and an 8-byte payload. */
static void *
make_region(uint64 capacity, Size *size)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	void	   *region;

	if (tess_table_size(1, one_int4, 8, capacity, size, &status) != TESS_OK)
		return NULL;
	region = palloc(*size);
	if (tess_table_create(region, *size, 1, one_int4, 8, capacity,
						  &status) != TESS_OK)
		return NULL;
	return region;
}

/*
 * Insert the rows of pending with their row numbers as payload; the rows
 * still pending afterwards, or -1 on an error.
 */
static int
insert_batch(void *region, Size size, Batch *batch, uint64 *pending_words,
			 uint32 *offsets)
{
	TessRowMask pending = {NROWS, pending_words};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	if (tess_table_insert(region, size, batch->hashes, 1, &batch->key,
						  (const uint8 *) batch->payload, &pending, offsets,
						  &status) != TESS_OK)
		return -1;
	return count_bits(pending_words);
}

/* Probe the valid rows of a batch. */
static bool
probe_batch(const void *region, Size size, const Batch *batch,
			uint64 *found_words, uint32 *matches)
{
	TessRowMask rows = {NROWS, (uint64 *) batch->valid};
	TessRowMask found = {NROWS, found_words};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	memset(found_words, 0, NWORDS * sizeof(uint64));
	return tess_table_probe(region, size, batch->hashes, 1, &batch->key,
							&rows, matches, &found, &status) == TESS_OK;
}

static bool
stats_of(const void *region, Size size, TessTableStats *stats)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	stats->struct_size = sizeof(TessTableStats);
	return tess_table_stats(region, size, stats, &status) == TESS_OK;
}

/* The record at an offset holds the key of a row and the payload of one with that key. */
static bool
record_matches(const void *region, Size size, const Batch *batch,
			   int row, uint32 offset)
{
	TessTableRecord record;
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	uint64		stored;

	record.struct_size = sizeof(TessTableRecord);
	if (tess_table_record(region, size, offset, &record, &status) != TESS_OK ||
		record.hash != batch->hashes[row] ||
		record.payload_size != 8)
		return false;
	memcpy(&stored, record.payload, sizeof(stored));
	if (stored >= NROWS || batch->isnull[stored] != batch->isnull[row])
		return false;
	if (batch->isnull[row])
		return record.null_bits == 1 && record.keys[0] == 0;
	return record.null_bits == 0 &&
		record.keys[0] == DatumGetInt32(batch->values[row]) &&
		DatumGetInt32(batch->values[stored]) == DatumGetInt32(batch->values[row]);
}

Datum
tessera_test_table_layout(PG_FUNCTION_ARGS)
{
	PG_RETURN_BOOL(tess_table_format_version() == TESS_TABLE_FORMAT_VERSION &&
				   tess_table_layout(TESS_TABLE_LAYOUT_HEADER_SIZE) == 96 &&
				   tess_table_layout(TESS_TABLE_LAYOUT_VERSION_OFFSET) == 8 &&
				   tess_table_layout(TESS_TABLE_LAYOUT_KEY_SIZE) ==
				   sizeof(TessTableKey) &&
				   tess_table_layout(TESS_TABLE_LAYOUT_KEY_PREPARED_OFFSET) ==
				   offsetof(TessTableKey, prepared) &&
				   tess_table_layout(TESS_TABLE_LAYOUT_STATS_SIZE) ==
				   sizeof(TessTableStats) &&
				   tess_table_layout(TESS_TABLE_LAYOUT_STATS_REGION_LEN_OFFSET) ==
				   offsetof(TessTableStats, region_len) &&
				   tess_table_layout(TESS_TABLE_LAYOUT_RECORD_SIZE) ==
				   sizeof(TessTableRecord) &&
				   tess_table_layout(TESS_TABLE_LAYOUT_RECORD_PAYLOAD_OFFSET) ==
				   offsetof(TessTableRecord, payload) &&
				   tess_table_layout((TessTableLayoutKind) 99) == 0);
}

/*
 * Insert the valid rows of a batch, find every one of them, walk the
 * records with equal keys, miss absent keys; then the same under the
 * group policy, where NULL keys are rows too.
 */
Datum
tessera_test_table_cycle(PG_FUNCTION_ARGS)
{
	Batch	   *batch = palloc(sizeof(Batch));
	Batch	   *absent = palloc(sizeof(Batch));
	uint64		pending[NWORDS];
	uint64		found[NWORDS];
	uint32		offsets[NROWS];
	uint32		matches[NROWS];
	uint32		chain[NROWS];
	Datum		gathered[NROWS];
	TessTableStats stats;
	TessRowMask valid_rows = {NROWS, pending};
	TessRowMask found_mask = {NROWS, found};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	Size		size;
	void	   *region;
	int			row;
	int			twins = 0;

	if (!prepare(batch, TESS_NULL_KEYS_REJECT, 0, false) ||
		count_bits(batch->valid) != 160)
		PG_RETURN_BOOL(false);
	region = make_region(NROWS, &size);
	if (region == NULL ||
		tess_table_attach(region, size, &status) != TESS_OK ||
		!stats_of(region, size, &stats) || stats.records != 0 ||
		stats.region_len != size)
		PG_RETURN_BOOL(false);

	memcpy(pending, batch->valid, sizeof(pending));
	if (insert_batch(region, size, batch, pending, offsets) != 0 ||
		!stats_of(region, size, &stats) || stats.records != 160 ||
		stats.buckets != 1024 ||
		stats.bytes_used != 96 + 160 * 32 + 1024 * 4)
		PG_RETURN_BOOL(false);

	if (!probe_batch(region, size, batch, found, matches) ||
		memcmp(found, batch->valid, sizeof(found)) != 0)
		PG_RETURN_BOOL(false);
	for (row = 0; row < NROWS; row++)
	{
		if (!has_bit(batch->valid, row))
			continue;
		if (!record_matches(region, size, batch, row, offsets[row]) ||
			!record_matches(region, size, batch, row, matches[row]))
			PG_RETURN_BOOL(false);
	}

	/* Every value is held by several rows: each has a next record. */
	memcpy(chain, matches, sizeof(chain));
	memcpy(pending, batch->valid, sizeof(pending));
	if (tess_table_next_match(region, size, chain, &valid_rows, &found_mask,
							  &status) != TESS_OK)
		PG_RETURN_BOOL(false);
	for (row = 0; row < NROWS; row++)
	{
		if (!has_bit(found, row))
			continue;
		twins++;
		if (chain[row] == matches[row] ||
			!record_matches(region, size, batch, row, chain[row]))
			PG_RETURN_BOOL(false);
	}
	if (twins != 160)
		PG_RETURN_BOOL(false);

	/*
	 * The payload word of every match in one call: the row that inserted
	 * it, which holds the same key; other rows keep their values, and a
	 * word past the payload is refused.
	 */
	for (row = 0; row < NROWS; row++)
		gathered[row] = (Datum) -1;
	if (tess_table_gather(region, size, matches, &valid_rows, 0, gathered,
						  &status) != TESS_OK ||
		tess_table_gather(region, size, matches, &valid_rows, 1, gathered,
						  &status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);
	for (row = 0; row < NROWS; row++)
	{
		if (!has_bit(batch->valid, row))
		{
			if (gathered[row] != (Datum) -1)
				PG_RETURN_BOOL(false);
			continue;
		}
		if (gathered[row] >= NROWS ||
			DatumGetInt32(batch->values[gathered[row]]) !=
			DatumGetInt32(batch->values[row]))
			PG_RETURN_BOOL(false);
	}

	/* Keys nobody inserted find nothing. */
	if (!prepare(absent, TESS_NULL_KEYS_REJECT, 1000, false) ||
		!probe_batch(region, size, absent, found, matches) ||
		count_bits(found) != 0)
		PG_RETURN_BOOL(false);
	pfree(region);

	/* Under the group policy the NULL rows are keys of their own. */
	if (!prepare(batch, TESS_NULL_KEYS_GROUP, 0, false) ||
		count_bits(batch->valid) != NROWS)
		PG_RETURN_BOOL(false);
	region = make_region(NROWS, &size);
	memcpy(pending, batch->valid, sizeof(pending));
	if (region == NULL ||
		insert_batch(region, size, batch, pending, offsets) != 0 ||
		!stats_of(region, size, &stats) || stats.records != NROWS ||
		!probe_batch(region, size, batch, found, matches) ||
		count_bits(found) != NROWS)
		PG_RETURN_BOOL(false);
	for (row = 0; row < NROWS; row++)
		if (!record_matches(region, size, batch, row, matches[row]))
			PG_RETURN_BOOL(false);
	pfree(region);
	pfree(batch);
	pfree(absent);
	PG_RETURN_BOOL(true);
}

/*
 * Rows resolve to the record of their key, created once per key; payloads
 * change in place; a walk visits the records in insertion order.
 */
Datum
tessera_test_table_groups(PG_FUNCTION_ARGS)
{
	Batch	   *batch = palloc(sizeof(Batch));
	uint64		pending[NWORDS];
	uint64		inserted[NWORDS];
	uint32		offsets[NROWS];
	uint32		walked[64];
	TessRowMask pending_mask = {NROWS, pending};
	TessRowMask inserted_mask = {NROWS, inserted};
	TessTableStats stats;
	TessTableRecord record;
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	uint64		cursor = 0;
	Size		size;
	void	   *region;
	int			count;
	int			row;
	int			key;

	if (!prepare(batch, TESS_NULL_KEYS_GROUP, 0, true))
		PG_RETURN_BOOL(false);
	region = make_region(20, &size);
	memcpy(pending, batch->all, sizeof(pending));
	if (region == NULL ||
		tess_table_find_or_insert(region, size, batch->hashes, 1, &batch->key,
								  &pending_mask, offsets, &inserted_mask,
								  &status) != TESS_OK ||
		count_bits(pending) != 0 || inserted[0] != 0x3ff ||
		count_bits(inserted) != 10 ||
		!stats_of(region, size, &stats) || stats.records != 10)
		PG_RETURN_BOOL(false);
	for (row = 0; row < NROWS; row++)
	{
		uint8	   *payload;
		uint64		counter;

		if (offsets[row] != offsets[row % 10] ||
			tess_table_payload(region, size, offsets[row], &payload,
							   &status) != TESS_OK)
			PG_RETURN_BOOL(false);
		memcpy(&counter, payload, sizeof(counter));
		counter += row;
		memcpy(payload, &counter, sizeof(counter));
	}
	record.struct_size = sizeof(TessTableRecord);
	for (key = 0; key < 10; key++)
	{
		uint64		sum;

		if (tess_table_record(region, size, offsets[key], &record,
							  &status) != TESS_OK || record.keys[0] != key)
			PG_RETURN_BOOL(false);
		memcpy(&sum, record.payload, sizeof(sum));
		if (sum != 20 * (uint64) key + 1900)
			PG_RETURN_BOOL(false);
	}

	/* The walk: the ten records in insertion order, then nothing. */
	if (tess_table_scan(region, size, &cursor, walked, 64, &count,
						&status) != TESS_OK || count != 10 ||
		memcmp(walked, offsets, 10 * sizeof(uint32)) != 0 ||
		tess_table_scan(region, size, &cursor, walked, 64, &count,
						&status) != TESS_OK || count != 0)
		PG_RETURN_BOOL(false);

	/* A second pass creates nothing. */
	memcpy(pending, batch->all, sizeof(pending));
	if (tess_table_find_or_insert(region, size, batch->hashes, 1, &batch->key,
								  &pending_mask, offsets, &inserted_mask,
								  &status) != TESS_OK ||
		count_bits(pending) != 0 || count_bits(inserted) != 0 ||
		!stats_of(region, size, &stats) || stats.records != 10)
		PG_RETURN_BOOL(false);

	/*
	 * count(*) and count(x) of every row into the first payload word, then
	 * the keys of the ten groups by their offsets.
	 */
	{
		TessRowMask all = {NROWS, batch->all};
		uint64		first_ten = 0x3ff;
		TessRowMask groups = {10, &first_ten};
		Datum		keys[10];
		bool		nulls[10];

		if (tess_table_accumulate(region, size, offsets, &all, TESS_TABLE_COUNT_ROWS,
								  NULL, NULL, 0, 0, 0, &status) != TESS_OK ||
			tess_table_accumulate(region, size, offsets, &all, TESS_TABLE_COUNT,
								  &batch->column, NULL, 0, 0, 0, &status) != TESS_OK ||
			tess_table_gather_key(region, size, offsets, &groups, 0, keys, nulls,
								  &status) != TESS_OK)
			PG_RETURN_BOOL(false);
		for (key = 0; key < 10; key++)
		{
			uint64		sum;

			if (tess_table_record(region, size, offsets[key], &record,
								  &status) != TESS_OK)
				PG_RETURN_BOOL(false);
			memcpy(&sum, record.payload, sizeof(sum));
			if (sum != 20 * (uint64) key + 1940 || DatumGetInt32(keys[key]) != key ||
				nulls[key])
				PG_RETURN_BOOL(false);
		}
		/* A word past the payload is refused. */
		if (tess_table_accumulate(region, size, offsets, &all, TESS_TABLE_COUNT_ROWS,
								  NULL, NULL, 8, 0, 0,
								  &status) != TESS_ERROR_INVALID_ARGUMENT)
			PG_RETURN_BOOL(false);
	}
	pfree(region);
	pfree(batch);
	PG_RETURN_BOOL(true);
}

/*
 * A region for 16 records takes 16 rows and leaves the rest pending;
 * repalloc and tess_table_grow make room for them all.
 */
Datum
tessera_test_table_grow(PG_FUNCTION_ARGS)
{
	Batch	   *batch = palloc(sizeof(Batch));
	uint64		pending[NWORDS];
	uint64		found[NWORDS];
	uint32		offsets[NROWS];
	uint32		matches[NROWS];
	uint32		walked[64];
	TessTableStats stats;
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	uint64		cursor = 0;
	Size		size;
	Size		grown;
	void	   *region;
	int			count;
	int			total = 0;
	int			row;

	if (!prepare(batch, TESS_NULL_KEYS_REJECT, 0, false))
		PG_RETURN_BOOL(false);
	region = make_region(16, &size);
	memcpy(pending, batch->valid, sizeof(pending));
	if (region == NULL ||
		insert_batch(region, size, batch, pending, offsets) != 160 - 16 ||
		!stats_of(region, size, &stats) || stats.records != 16 ||
		stats.bytes_used != stats.region_len)
		PG_RETURN_BOOL(false);

	if (tess_table_size(1, one_int4, 8, 512, &grown, &status) != TESS_OK)
		PG_RETURN_BOOL(false);
	region = repalloc(region, grown);
	if (tess_table_grow(region, grown, &status) != TESS_OK ||
		!stats_of(region, grown, &stats) || stats.records != 16 ||
		stats.region_len != grown || stats.buckets != 1024)
		PG_RETURN_BOOL(false);
	if (insert_batch(region, grown, batch, pending, offsets) != 0 ||
		!stats_of(region, grown, &stats) || stats.records != 160 ||
		!probe_batch(region, grown, batch, found, matches) ||
		memcmp(found, batch->valid, sizeof(found)) != 0)
		PG_RETURN_BOOL(false);
	for (row = 0; row < NROWS; row++)
		if (has_bit(batch->valid, row) &&
			!record_matches(region, grown, batch, row, offsets[row]))
			PG_RETURN_BOOL(false);
	do
	{
		if (tess_table_scan(region, grown, &cursor, walked, 64, &count,
							&status) != TESS_OK)
			PG_RETURN_BOOL(false);
		total += count;
	} while (count > 0);
	if (total != 160 ||
		tess_table_grow(region, grown - 8, &status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);
	pfree(region);
	pfree(batch);
	PG_RETURN_BOOL(true);
}

/*
 * Garbage, a foreign version, a misaligned or short region, the wrong key
 * count or kind and undersized structures are statuses, and the table
 * works after a caught panic.
 */
Datum
tessera_test_table_errors(PG_FUNCTION_ARGS)
{
	Batch	   *batch = palloc(sizeof(Batch));
	uint64		pending[NWORDS];
	uint64		found[NWORDS];
	uint32		offsets[NROWS];
	uint32		matches[NROWS];
	TessTableKey two[2];
	TessTableKey odd;
	TessRowMask rows = {NROWS, batch->valid};
	TessRowMask found_mask = {NROWS, found};
	TessTableStats stats;
	TessTableRecord record;
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	void	   *zeros = palloc0(4096);
	uint32	   *version;
	uint64		cursor = 4;
	Size		size;
	void	   *region;
	int			count;

	if (tess_table_attach(zeros, 4096, &status) != TESS_ERROR_INVALID_ARGUMENT ||
		strstr(status.message, "does not hold a table") == NULL ||
		strcmp(status.sqlstate, "XX000") != 0)
		PG_RETURN_BOOL(false);

	if (!prepare(batch, TESS_NULL_KEYS_REJECT, 0, false))
		PG_RETURN_BOOL(false);
	region = make_region(NROWS, &size);
	memcpy(pending, batch->valid, sizeof(pending));
	if (region == NULL ||
		insert_batch(region, size, batch, pending, offsets) != 0)
		PG_RETURN_BOOL(false);

	version = (uint32 *) ((char *) region +
						  tess_table_layout(TESS_TABLE_LAYOUT_VERSION_OFFSET));
	*version = TESS_TABLE_FORMAT_VERSION + 1;
	if (tess_table_attach(region, size, &status) != TESS_ERROR_INVALID_ARGUMENT ||
		strstr(status.message, "version") == NULL)
		PG_RETURN_BOOL(false);
	*version = TESS_TABLE_FORMAT_VERSION;
	if (tess_table_attach(region, size, &status) != TESS_OK ||
		tess_table_attach((char *) region + 4, size - 4,
						  &status) != TESS_ERROR_INVALID_ARGUMENT ||
		tess_table_attach(region, size - 8,
						  &status) != TESS_ERROR_INVALID_ARGUMENT ||
		tess_table_attach(NULL, size, &status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);

	two[0] = batch->key;
	two[1] = batch->key;
	odd = batch->key;
	odd.kind = (TessTableKeyKind) 3;
	if (tess_table_probe(region, size, batch->hashes, 2, two, &rows, matches,
						 &found_mask, &status) != TESS_ERROR_INVALID_ARGUMENT ||
		strstr(status.message, "keys") == NULL ||
		tess_table_probe(region, size, batch->hashes, 1, &odd, &rows, matches,
						 &found_mask, &status) != TESS_ERROR_INVALID_ARGUMENT ||
		tess_table_size(1, &odd.kind, 8, 1, &size,
						&status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);

	stats.struct_size = 8;
	record.struct_size = 8;
	if (tess_table_stats(region, size, &stats,
						 &status) != TESS_ERROR_INVALID_ARGUMENT ||
		tess_table_record(region, size, offsets[1], &record,
						  &status) != TESS_ERROR_INVALID_ARGUMENT ||
		tess_table_record(region, size, 0, &record,
						  &status) != TESS_ERROR_INVALID_ARGUMENT ||
		tess_table_scan(region, size, &cursor, matches, 1, &count,
						&status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);

	/* The library works after a caught panic. */
	if (tess_kernels_test_panic(&status) != TESS_ERROR_PANIC ||
		!probe_batch(region, size, batch, found, matches) ||
		memcmp(found, batch->valid, sizeof(found)) != 0)
		PG_RETURN_BOOL(false);
	pfree(region);
	pfree(zeros);
	pfree(batch);
	PG_RETURN_BOOL(true);
}

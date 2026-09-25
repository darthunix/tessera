#include "postgres.h"

#include <string.h>

#include "fmgr.h"

#include "tessera/kernels.h"
#include "tessera/table.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_table_layout);
PG_FUNCTION_INFO_V1(tessera_test_table_cycle);
PG_FUNCTION_INFO_V1(tessera_test_table_groups);
PG_FUNCTION_INFO_V1(tessera_test_table_regrow);
PG_FUNCTION_INFO_V1(tessera_test_table_errors);

#define NROWS 200
#define NWORDS 4

/* Chunks of 64 records of 32 bytes: the valid rows of a batch take three. */
#define CHUNK_LEN (TESS_TABLE_CHUNK_HEADER + 64 * 32)
#define MAX_TEST_CHUNKS 8

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

/*
 * A table of one int4 key and an 8-byte payload: its index, its chunks in
 * palloc'd memory, and for each chunk the byte linking has reached.
 */
typedef struct Table
{
	TessTableRef ref;
	void	   *chunks[MAX_TEST_CHUNKS];
	Size		lens[MAX_TEST_CHUNKS];
	Size		linked[MAX_TEST_CHUNKS];
} Table;

/* An empty table whose index is sized for capacity records. */
static Table *
make_table(uint64 capacity)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	Table	   *table = palloc0(sizeof(Table));

	if (tess_table_size(1, one_int4, 8, capacity, &table->ref.index_len,
						&status) != TESS_OK)
		return NULL;
	table->ref.index = palloc(table->ref.index_len);
	table->ref.chunks = table->chunks;
	table->ref.chunk_lens = table->lens;
	if (tess_table_create(table->ref.index, table->ref.index_len, 1, one_int4,
						  8, capacity, &status) != TESS_OK)
		return NULL;
	return table;
}

/* Add an empty chunk of len bytes. */
static bool
add_chunk(Table *table, Size len)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	int			chunk = table->ref.nchunks;

	if (chunk == MAX_TEST_CHUNKS)
		return false;
	table->chunks[chunk] = palloc(len);
	table->lens[chunk] = len;
	table->linked[chunk] = TESS_TABLE_CHUNK_HEADER;
	if (tess_table_chunk_init(table->chunks[chunk], len, &status) != TESS_OK)
		return false;
	table->ref.nchunks++;
	return true;
}

static void
free_table(Table *table)
{
	int			chunk;

	for (chunk = 0; chunk < table->ref.nchunks; chunk++)
		pfree(table->chunks[chunk]);
	pfree(table->ref.index);
	pfree(table);
}

/*
 * Append the rows of pending with their row numbers as payload, adding
 * chunks of CHUNK_LEN bytes as they fill, and link them, grouped or not:
 * true when every row went in. *duplicates, unless NULL, receives the
 * linked records whose keys were there already.
 */
static bool
insert_batch(Table *table, Batch *batch, uint64 *pending_words,
			 uint32 *offsets, bool grouped, uint64 *duplicates)
{
	TessRowMask pending = {NROWS, pending_words};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	int			chunk;

	if (duplicates != NULL)
		*duplicates = 0;
	for (;;)
	{
		if (table->ref.nchunks == 0 && !add_chunk(table, CHUNK_LEN))
			return false;
		if (tess_table_append(&table->ref, table->ref.nchunks - 1, 8,
							  batch->hashes, 1, &batch->key,
							  (const uint8 *) batch->payload, &pending,
							  offsets, &status) != TESS_OK)
			return false;
		if (count_bits(pending_words) == 0)
			break;
		if (!add_chunk(table, CHUNK_LEN))
			return false;
	}
	for (chunk = 0; chunk < table->ref.nchunks; chunk++)
	{
		uint64		repeated = 0;

		if (grouped ?
			tess_table_link_grouped(&table->ref, chunk, &table->linked[chunk],
									NULL, &repeated, &status) != TESS_OK :
			tess_table_link(&table->ref, chunk, &table->linked[chunk], NULL,
							&status) != TESS_OK)
			return false;
		if (duplicates != NULL)
			*duplicates += repeated;
	}
	return true;
}

/* Probe the valid rows of a batch. */
static bool
probe_batch(const Table *table, const Batch *batch, uint64 *found_words,
			uint32 *matches)
{
	TessRowMask rows = {NROWS, (uint64 *) batch->valid};
	TessRowMask found = {NROWS, found_words};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	memset(found_words, 0, NWORDS * sizeof(uint64));
	return tess_table_probe(&table->ref, batch->hashes, 1, &batch->key,
							&rows, matches, &found, &status) == TESS_OK;
}

static bool
stats_of(const Table *table, TessTableStats *stats)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	stats->struct_size = sizeof(TessTableStats);
	return tess_table_stats(&table->ref, stats, &status) == TESS_OK;
}

/* The record at an offset holds the key of a row and the payload of one with that key. */
static bool
record_matches(const Table *table, const Batch *batch, int row, uint32 offset)
{
	TessTableRecord record;
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	uint64		stored;

	record.struct_size = sizeof(TessTableRecord);
	if (tess_table_record(&table->ref, offset, &record, &status) != TESS_OK ||
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

/*
 * A participant alone at its barrier, which elects it at every phase, and
 * whose build takes two chunks and appends three records: the actions it
 * steps through.
 */
static bool
build_alone(void)
{
	static const uint32 expected[] = {
		TESS_BUILD_ATTACH, TESS_BUILD_DO_BUILD,
		TESS_BUILD_ARRIVE_AND_WAIT, TESS_BUILD_DO_SIZE,
		TESS_BUILD_ARRIVE_AND_WAIT, TESS_BUILD_DO_LINK,
		TESS_BUILD_ARRIVE_AND_WAIT, TESS_BUILD_DO_PROBE,
		TESS_BUILD_ARRIVE_AND_DETACH, TESS_BUILD_DO_FREE
	};
	TessBuildParticipant participant = {0};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	uint64		counters[TESS_BUILD_COUNTER_WORDS];
	uint64		records;
	uint64		nulls;
	uint64		chunks;
	uint32		phase = TESS_BUILD_BUILD;
	uint32		reply = 0;
	int			step;

	StaticAssertStmt(sizeof(TessBuildParticipant) == 12,
					 "a participant is three words of four bytes");
	if (tess_build_counters_init(counters, &status) != TESS_OK)
		return false;
	for (step = 0; step < lengthof(expected); step++)
	{
		uint32		action;

		if (tess_build_step(&participant, counters, reply, &action,
							&status) != TESS_OK || action != expected[step])
			return false;
		reply = 0;
		if (action == TESS_BUILD_ATTACH)
			reply = phase;
		else if (action == TESS_BUILD_ARRIVE_AND_WAIT)
		{
			phase++;
			reply = 1;
		}
		else if (action == TESS_BUILD_DO_BUILD)
		{
			uint64		first;
			uint64		second;

			if (tess_build_take_chunk(counters, &first, &status) != TESS_OK ||
				tess_build_take_chunk(counters, &second, &status) != TESS_OK ||
				first != 0 || second != 1 ||
				tess_build_report(counters, 3, 0x5, &status) != TESS_OK)
				return false;
		}
		else if (action == TESS_BUILD_ARRIVE_AND_DETACH)
			reply = 1;
	}
	return tess_build_totals(counters, &records, &nulls, &chunks,
							 &status) == TESS_OK &&
		records == 3 && nulls == 0x5 && chunks == 2;
}

Datum
tessera_test_table_layout(PG_FUNCTION_ARGS)
{
	PG_RETURN_BOOL(build_alone() &&
				   tess_table_format_version() == TESS_TABLE_FORMAT_VERSION &&
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
				   tess_table_layout(TESS_TABLE_LAYOUT_REF_SIZE) ==
				   sizeof(TessTableRef) &&
				   tess_table_layout(TESS_TABLE_LAYOUT_REF_NCHUNKS_OFFSET) ==
				   offsetof(TessTableRef, nchunks) &&
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
	uint64		pending[NWORDS] = {0};
	uint64		found[NWORDS] = {0};
	uint32		offsets[NROWS];
	uint32		matches[NROWS];
	uint32		chain[NROWS];
	Datum		gathered[NROWS];
	TessTableStats stats;
	TessRowMask valid_rows = {NROWS, pending};
	TessRowMask found_mask = {NROWS, found};
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	Table	   *table;
	int			row;
	int			twins = 0;

	if (!prepare(batch, TESS_NULL_KEYS_REJECT, 0, false) ||
		count_bits(batch->valid) != 160)
		PG_RETURN_BOOL(false);
	table = make_table(NROWS);
	if (table == NULL ||
		tess_table_attach(&table->ref, &status) != TESS_OK ||
		!stats_of(table, &stats) || stats.records != 0 ||
		stats.region_len != table->ref.index_len)
		PG_RETURN_BOOL(false);

	/* 160 records of 32 bytes take three chunks of 64. */
	memcpy(pending, batch->valid, sizeof(pending));
	if (!insert_batch(table, batch, pending, offsets, false, NULL) ||
		table->ref.nchunks != 3 ||
		!stats_of(table, &stats) || stats.records != 160 ||
		stats.buckets != 1024 ||
		stats.bytes_used != 96 + 1024 * 4)
		PG_RETURN_BOOL(false);

	if (!probe_batch(table, batch, found, matches) ||
		memcmp(found, batch->valid, sizeof(found)) != 0)
		PG_RETURN_BOOL(false);
	for (row = 0; row < NROWS; row++)
	{
		if (!has_bit(batch->valid, row))
			continue;
		if (!record_matches(table, batch, row, offsets[row]) ||
			!record_matches(table, batch, row, matches[row]))
			PG_RETURN_BOOL(false);
	}

	/* Every value is held by several rows: each has a next record. */
	memcpy(chain, matches, sizeof(chain));
	memcpy(pending, batch->valid, sizeof(pending));
	if (tess_table_next_match(&table->ref, chain, &valid_rows, &found_mask,
							  &status) != TESS_OK)
		PG_RETURN_BOOL(false);
	for (row = 0; row < NROWS; row++)
	{
		if (!has_bit(found, row))
			continue;
		twins++;
		if (chain[row] == matches[row] ||
			!record_matches(table, batch, row, chain[row]))
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
	if (tess_table_gather(&table->ref, matches, &valid_rows, 0, gathered,
						  &status) != TESS_OK ||
		tess_table_gather(&table->ref, matches, &valid_rows, 1, gathered,
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
		!probe_batch(table, absent, found, matches) ||
		count_bits(found) != 0)
		PG_RETURN_BOOL(false);
	free_table(table);

	/* Under the group policy the NULL rows are keys of their own. */
	if (!prepare(batch, TESS_NULL_KEYS_GROUP, 0, false) ||
		count_bits(batch->valid) != NROWS)
		PG_RETURN_BOOL(false);
	table = make_table(NROWS);
	memcpy(pending, batch->valid, sizeof(pending));
	if (table == NULL ||
		!insert_batch(table, batch, pending, offsets, false, NULL) ||
		!stats_of(table, &stats) || stats.records != NROWS ||
		!probe_batch(table, batch, found, matches) ||
		count_bits(found) != NROWS)
		PG_RETURN_BOOL(false);
	for (row = 0; row < NROWS; row++)
		if (!record_matches(table, batch, row, matches[row]))
			PG_RETURN_BOOL(false);
	free_table(table);

	/*
	 * Linked grouped, a key's records follow each other: the first row of
	 * each of the ten keys is new, and from it next_in_group steps through
	 * the other nineteen.
	 */
	if (!prepare(batch, TESS_NULL_KEYS_GROUP, 0, true))
		PG_RETURN_BOOL(false);
	table = make_table(NROWS);
	memcpy(pending, batch->all, sizeof(pending));
	{
		uint64		duplicates;
		uint64		first_ten[NWORDS] = {0x3ff};
		TessRowMask rows = {NROWS, first_ten};
		int			step;

		if (table == NULL ||
			!insert_batch(table, batch, pending, offsets, true, &duplicates) ||
			duplicates != NROWS - 10)
			PG_RETURN_BOOL(false);
		memcpy(chain, offsets, sizeof(chain));
		for (step = 0; step < 20; step++)
		{
			if (tess_table_next_in_group(&table->ref, chain, &rows, &found_mask,
										 &status) != TESS_OK ||
				count_bits(found) != (step < 19 ? 10 : 0))
				PG_RETURN_BOOL(false);
			memcpy(first_ten, found, sizeof(first_ten));
		}
	}
	free_table(table);
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
	uint64		pending[NWORDS] = {0};
	uint64		inserted[NWORDS] = {0};
	uint32		offsets[NROWS];
	uint32		walked[64];
	TessRowMask pending_mask = {NROWS, pending};
	TessRowMask inserted_mask = {NROWS, inserted};
	TessTableStats stats;
	TessTableRecord record;
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	uint64		cursor = 0;
	Table	   *table;
	int			count;
	int			row;
	int			key;

	if (!prepare(batch, TESS_NULL_KEYS_GROUP, 0, true))
		PG_RETURN_BOOL(false);
	table = make_table(20);
	memcpy(pending, batch->all, sizeof(pending));
	if (table == NULL || !add_chunk(table, CHUNK_LEN) ||
		tess_table_find_or_insert(&table->ref, 0, batch->hashes, 1, &batch->key,
								  &pending_mask, offsets, &inserted_mask,
								  &status) != TESS_OK ||
		count_bits(pending) != 0 || inserted[0] != 0x3ff ||
		count_bits(inserted) != 10 ||
		!stats_of(table, &stats) || stats.records != 10)
		PG_RETURN_BOOL(false);
	for (row = 0; row < NROWS; row++)
	{
		uint8	   *payload;
		uint64		counter;

		if (offsets[row] != offsets[row % 10] ||
			tess_table_payload(&table->ref, offsets[row], &payload,
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

		if (tess_table_record(&table->ref, offsets[key], &record,
							  &status) != TESS_OK || record.keys[0] != key)
			PG_RETURN_BOOL(false);
		memcpy(&sum, record.payload, sizeof(sum));
		if (sum != 20 * (uint64) key + 1900)
			PG_RETURN_BOOL(false);
	}

	/* The walk: the ten records in insertion order, then nothing. */
	if (tess_table_scan(&table->ref, &cursor, walked, 64, &count,
						&status) != TESS_OK || count != 10 ||
		memcmp(walked, offsets, 10 * sizeof(uint32)) != 0 ||
		tess_table_scan(&table->ref, &cursor, walked, 64, &count,
						&status) != TESS_OK || count != 0)
		PG_RETURN_BOOL(false);

	/* A second pass creates nothing. */
	memcpy(pending, batch->all, sizeof(pending));
	if (tess_table_find_or_insert(&table->ref, 0, batch->hashes, 1, &batch->key,
								  &pending_mask, offsets, &inserted_mask,
								  &status) != TESS_OK ||
		count_bits(pending) != 0 || count_bits(inserted) != 0 ||
		!stats_of(table, &stats) || stats.records != 10)
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

		if (tess_table_accumulate(&table->ref, offsets, &all, TESS_TABLE_COUNT_ROWS,
								  NULL, NULL, 0, 0, 0, &status) != TESS_OK ||
			tess_table_accumulate(&table->ref, offsets, &all, TESS_TABLE_COUNT,
								  &batch->column, NULL, 0, 0, 0, &status) != TESS_OK ||
			tess_table_gather_key(&table->ref, offsets, &groups, 0, keys, nulls,
								  &status) != TESS_OK)
			PG_RETURN_BOOL(false);
		for (key = 0; key < 10; key++)
		{
			uint64		sum;

			if (tess_table_record(&table->ref, offsets[key], &record,
								  &status) != TESS_OK)
				PG_RETURN_BOOL(false);
			memcpy(&sum, record.payload, sizeof(sum));
			if (sum != 20 * (uint64) key + 1940 || DatumGetInt32(keys[key]) != key ||
				nulls[key])
				PG_RETURN_BOOL(false);
		}
		/* A filter of the ten records lets every row of the batch through. */
		{
			Size		nwords;
			uint64	   *filter;
			uint64		passed[NWORDS] = {0};
			TessRowMask passed_mask = {NROWS, passed};

			if (tess_table_bloom_words(10, &nwords, &status) != TESS_OK || nwords != 4)
				PG_RETURN_BOOL(false);
			filter = palloc0(sizeof(uint64) * nwords);
			if (tess_table_bloom(&table->ref, filter, nwords, &status) != TESS_OK ||
				tess_bloom_probe(filter, nwords, batch->hashes, &all, &passed_mask,
								 &status) != TESS_OK ||
				count_bits(passed) != NROWS ||
				tess_bloom_probe(filter, 3, batch->hashes, &all, &passed_mask,
								 &status) != TESS_ERROR_INVALID_ARGUMENT)
				PG_RETURN_BOOL(false);
		}
		/*
		 * A shared filter: not ready before it is built, built once, then
		 * every row passes.
		 */
		{
			Size		nwords;
			uint64	   *filter;
			uint64		passed[NWORDS] = {0};
			TessRowMask passed_mask = {NROWS, passed};
			bool		built;
			bool		ready;

			if (tess_bloom_shared_words(10, &nwords, &status) != TESS_OK || nwords != 5)
				PG_RETURN_BOOL(false);
			filter = palloc(sizeof(uint64) * nwords);
			if (tess_bloom_shared_init(filter, nwords, &status) != TESS_OK ||
				tess_bloom_shared_ready(filter, nwords, &ready, &status) != TESS_OK ||
				ready ||
				tess_bloom_shared_probe(filter, nwords, batch->hashes, &all,
										&passed_mask,
										&status) != TESS_ERROR_INVALID_ARGUMENT ||
				tess_table_try_build_bloom(&table->ref, filter, nwords, &built,
										   &status) != TESS_OK || !built ||
				tess_table_try_build_bloom(&table->ref, filter, nwords, &built,
										   &status) != TESS_OK || built ||
				tess_bloom_shared_ready(filter, nwords, &ready, &status) != TESS_OK ||
				!ready ||
				tess_bloom_shared_probe(filter, nwords, batch->hashes, &all,
										&passed_mask, &status) != TESS_OK ||
				count_bits(passed) != NROWS)
				PG_RETURN_BOOL(false);
		}
		/* A word past the payload is refused. */
		if (tess_table_accumulate(&table->ref, offsets, &all, TESS_TABLE_COUNT_ROWS,
								  NULL, NULL, 8, 0, 0,
								  &status) != TESS_ERROR_INVALID_ARGUMENT)
			PG_RETURN_BOOL(false);
	}
	free_table(table);
	pfree(batch);
	PG_RETURN_BOOL(true);
}

/*
 * A table outgrows its index: find_or_insert stops at half the buckets,
 * tess_table_regrow builds a larger index over the same chunks, and the
 * records keep their references.
 */
Datum
tessera_test_table_regrow(PG_FUNCTION_ARGS)
{
	Batch	   *batch = palloc(sizeof(Batch));
	uint64		pending[NWORDS] = {0};
	uint64		inserted[NWORDS] = {0};
	uint64		found[NWORDS] = {0};
	uint32		offsets[NROWS];
	uint32		matches[NROWS];
	uint32		walked[64];
	TessRowMask pending_mask = {NROWS, pending};
	TessRowMask inserted_mask = {NROWS, inserted};
	TessTableStats stats;
	TessTableRecord record;
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	uint64		cursor = 0;
	Table	   *table;
	void	   *old_index;
	void	   *index;
	Size		len;
	int			count;
	int			total = 0;
	int			row;

	/* Every row a key of its own under the group policy: 200 groups. */
	if (!prepare(batch, TESS_NULL_KEYS_GROUP, 0, false))
		PG_RETURN_BOOL(false);
	for (row = 0; row < NROWS; row++)
	{
		batch->values[row] = Int32GetDatum(row);
		batch->isnull[row] = false;
	}
	{
		TessRowMask rows = {NROWS, batch->all};
		TessRowMask valid = {NROWS, batch->valid};

		if (tess_int4_hash(&batch->column, NULL, &rows, TESS_NULL_KEYS_GROUP,
						   batch->hashes, &valid, &status) != TESS_OK)
			PG_RETURN_BOOL(false);
	}

	/* An index of 1024 buckets takes 512 records; here all 200 fit. */
	table = make_table(16);
	memcpy(pending, batch->all, sizeof(pending));
	if (table == NULL || !add_chunk(table, TESS_TABLE_MAX_CHUNK_LEN) ||
		tess_table_find_or_insert(&table->ref, 0, batch->hashes, 1, &batch->key,
								  &pending_mask, offsets, &inserted_mask,
								  &status) != TESS_OK ||
		count_bits(pending) != 0 || count_bits(inserted) != NROWS ||
		!stats_of(table, &stats) || stats.records != NROWS ||
		stats.buckets != 1024)
		PG_RETURN_BOOL(false);

	/* A larger index, the same records at the same references. */
	if (tess_table_size(1, one_int4, 8, 4096, &len, &status) != TESS_OK)
		PG_RETURN_BOOL(false);
	index = palloc(len);
	if (tess_table_regrow(&table->ref, index, len, 4096, &status) != TESS_OK)
		PG_RETURN_BOOL(false);
	old_index = table->ref.index;
	table->ref.index = index;
	table->ref.index_len = len;
	pfree(old_index);
	if (!stats_of(table, &stats) || stats.records != NROWS ||
		stats.region_len != len || stats.buckets != 8192 ||
		!probe_batch(table, batch, found, matches) ||
		memcmp(found, batch->all, sizeof(found)) != 0 ||
		memcmp(matches, offsets, sizeof(matches)) != 0)
		PG_RETURN_BOOL(false);
	record.struct_size = sizeof(TessTableRecord);
	for (row = 0; row < NROWS; row++)
		if (tess_table_record(&table->ref, offsets[row], &record,
							  &status) != TESS_OK ||
			record.hash != batch->hashes[row] || record.keys[0] != row)
			PG_RETURN_BOOL(false);
	do
	{
		if (tess_table_scan(&table->ref, &cursor, walked, 64, &count,
							&status) != TESS_OK)
			PG_RETURN_BOOL(false);
		total += count;
	} while (count > 0);

	/* A short index and fewer buckets are refused. */
	index = palloc(len);
	if (total != NROWS ||
		tess_table_regrow(&table->ref, index, len - 8, 4096,
						  &status) != TESS_ERROR_INVALID_ARGUMENT ||
		tess_table_regrow(&table->ref, index, len, 16,
						  &status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);
	pfree(index);
	free_table(table);
	pfree(batch);
	PG_RETURN_BOOL(true);
}

/*
 * Garbage, a foreign version, a misaligned or short index, a reference or
 * chunk past the chunks, the wrong key count or kind and undersized
 * structures are statuses, and the table works after a caught panic.
 */
Datum
tessera_test_table_errors(PG_FUNCTION_ARGS)
{
	Batch	   *batch = palloc(sizeof(Batch));
	uint64		pending[NWORDS] = {0};
	uint64		found[NWORDS] = {0};
	uint32		offsets[NROWS];
	uint32		matches[NROWS];
	TessTableKey two[2];
	TessTableKey odd;
	TessRowMask rows = {NROWS, batch->valid};
	TessRowMask found_mask = {NROWS, found};
	TessRowMask pending_mask = {NROWS, pending};
	TessTableStats stats;
	TessTableRecord record;
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	TessTableRef other;
	void	   *zeros = palloc0(4096);
	uint32	   *version;
	uint64		cursor = 4;
	Size		size;
	Table	   *table;
	int			count;
	int			last = NROWS - 1;

	other.index = zeros;
	other.index_len = 4096;
	other.chunks = NULL;
	other.chunk_lens = NULL;
	other.nchunks = 0;
	if (tess_table_attach(&other, &status) != TESS_ERROR_INVALID_ARGUMENT ||
		strstr(status.message, "does not hold a table") == NULL ||
		strcmp(status.sqlstate, "XX000") != 0)
		PG_RETURN_BOOL(false);

	if (!prepare(batch, TESS_NULL_KEYS_REJECT, 0, false))
		PG_RETURN_BOOL(false);
	table = make_table(NROWS);
	memcpy(pending, batch->valid, sizeof(pending));
	if (table == NULL ||
		!insert_batch(table, batch, pending, offsets, false, NULL))
		PG_RETURN_BOOL(false);
	size = table->ref.index_len;

	version = (uint32 *) ((char *) table->ref.index +
						  tess_table_layout(TESS_TABLE_LAYOUT_VERSION_OFFSET));
	*version = TESS_TABLE_FORMAT_VERSION + 1;
	if (tess_table_attach(&table->ref, &status) != TESS_ERROR_INVALID_ARGUMENT ||
		strstr(status.message, "version") == NULL)
		PG_RETURN_BOOL(false);
	*version = TESS_TABLE_FORMAT_VERSION;
	other = table->ref;
	other.index = (char *) table->ref.index + 4;
	other.index_len = size - 4;
	if (tess_table_attach(&table->ref, &status) != TESS_OK ||
		tess_table_attach(&other, &status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);
	other = table->ref;
	other.index_len = size - 8;
	if (tess_table_attach(&other, &status) != TESS_ERROR_INVALID_ARGUMENT ||
		tess_table_attach(NULL, &status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);

	/*
	 * The last valid row lies in the third chunk: a view of two chunks
	 * refuses its reference, and appending to a chunk past them fails.
	 */
	while (!has_bit(batch->valid, last))
		last--;
	other = table->ref;
	other.nchunks = 2;
	record.struct_size = sizeof(TessTableRecord);
	memcpy(pending, batch->valid, sizeof(pending));
	if (tess_table_record(&other, offsets[last], &record,
						  &status) != TESS_ERROR_INVALID_ARGUMENT ||
		tess_table_append(&other, 2, 8, batch->hashes, 1, &batch->key, NULL,
						  &pending_mask, offsets,
						  &status) != TESS_ERROR_INVALID_ARGUMENT ||
		tess_table_chunk_init((char *) zeros + 4, 64,
							  &status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);

	two[0] = batch->key;
	two[1] = batch->key;
	odd = batch->key;
	odd.kind = (TessTableKeyKind) 3;
	if (tess_table_probe(&table->ref, batch->hashes, 2, two, &rows, matches,
						 &found_mask, &status) != TESS_ERROR_INVALID_ARGUMENT ||
		strstr(status.message, "keys") == NULL ||
		tess_table_probe(&table->ref, batch->hashes, 1, &odd, &rows, matches,
						 &found_mask, &status) != TESS_ERROR_INVALID_ARGUMENT ||
		tess_table_size(1, &odd.kind, 8, 1, &size,
						&status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);

	stats.struct_size = 8;
	record.struct_size = 8;
	if (tess_table_stats(&table->ref, &stats,
						 &status) != TESS_ERROR_INVALID_ARGUMENT ||
		tess_table_record(&table->ref, offsets[1], &record,
						  &status) != TESS_ERROR_INVALID_ARGUMENT ||
		tess_table_record(&table->ref, 0, &record,
						  &status) != TESS_ERROR_INVALID_ARGUMENT ||
		tess_table_scan(&table->ref, &cursor, matches, 1, &count,
						&status) != TESS_ERROR_INVALID_ARGUMENT)
		PG_RETURN_BOOL(false);

	/* The library works after a caught panic. */
	if (tess_kernels_test_panic(&status) != TESS_ERROR_PANIC ||
		!probe_batch(table, batch, found, matches) ||
		memcmp(found, batch->valid, sizeof(found)) != 0)
		PG_RETURN_BOOL(false);
	free_table(table);
	pfree(zeros);
	pfree(batch);
	PG_RETURN_BOOL(true);
}

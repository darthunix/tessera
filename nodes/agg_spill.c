#include "postgres.h"

#include "common/hashfn.h"
#include "miscadmin.h"
#include "utils/datum.h"
#include "lib/hyperloglog.h"
#include "utils/memutils.h"

#include "tessera/runtime.h"

#include "internal.h"
#include "agg.h"

/*
 * Spilling (plan item 5.6, docs/spill.md). The groups fit until the table
 * takes more than hash_mem; then they go into partitions by the hashes'
 * low bits under the one index, and while the table takes more, the
 * largest partition goes to disk whole: its records, each a group's
 * states, are written and freed, and the index is made anew over the
 * rest. Its rows then make new records, which are merged with those on
 * disk once the input is done: partition by partition, the records in
 * memory make a table, and the chunks read back merge into it
 * (tess_table_combine). A partition too large to merge is split first by
 * the next bits of the hash into a level of its own.
 *
 * The chunk arrays keep two slots first: AGG_SOURCE, empty or a chunk read
 * back for a merge or a split, and AGG_EMPTY, an empty chunk a partition
 * without one appends to, which sends its rows back for a chunk.
 */
#define AGG_SPILL_MIN_CHUNK (8 * 1024)
#define AGG_SPILL_MIN_PARTITIONS 4
#define AGG_SPILL_MAX_PARTITIONS 1024
#define AGG_SOURCE 0
#define AGG_EMPTY 1

/* A partition: its chunks in memory, the last the one it appends to. */
typedef struct AggPart
{
	void	  **chunks;
	int			nchunks;
	int			slots;
	Size		bytes;
	/* Records in memory and on disk, and the bytes written. */
	uint64		records;
	uint64		disk_records;
	uint64		disk_bytes;
	/*
	 * The partition's groups, estimated from the hashes of every record
	 * made in it (HyperLogLog, as the core's hash aggregate keeps one per
	 * spilled partition): what a merge holds, however many times a group
	 * went to disk.
	 */
	hyperLogLogState groups;
} AggPart;

/* Registers of a partition's estimate, 2^6 bytes: an error of about 13 %. */
#define AGG_GROUPS_WIDTH 6

typedef struct AggSpill
{
	struct AggSpill *parent;
	/* The chunks, in blocks of their own size; the blocks read back. */
	MemoryContext context;
	MemoryContext block_context;
	uint32		level;
	uint32		shift;
	int			npartitions;
	Size		chunk_len;
	AggPart    *parts;
	/*
	 * A split's chunks: the source, the empty one, and each partition's
	 * current chunk at AGG_EMPTY + 1 + partition, or the empty one.
	 */
	void	  **bases;
	Size	   *lens;
	uint32	   *current;
	TessSpill  *file;
	uint32		next_number;
	/* The input is read; the partition being given out, -1 before any. */
	bool		done_input;
	int			partition;
	/*
	 * Giving out: the partitions wholly in memory first, which merge with
	 * nothing and free their memory, then those with records on disk (pass
	 * 1); whether the current partition was given out, to free it next.
	 */
	int			pass;
	bool		given;
	/* The empty chunks of the two first slots. */
	uint64		source_empty[1];
	uint64		empty[1];
} AggSpill;

static bool agg_evict(TessAggState *state, Size extra);
static uint64 agg_records(AggSpill *spill);

static inline uint32
agg_partition(const AggSpill *spill, uint32 hash)
{
	return tess_table_partition(hash, spill->shift, (uint32) spill->npartitions);
}

static inline Size
agg_chunk_used(const void *base)
{
	return (Size) *(const uint64 *) base;
}

static void
part_push(AggSpill *spill, AggPart *part, void *base)
{
	if (part->nchunks == part->slots)
	{
		part->slots = Max(part->slots * 2, 4);
		part->chunks = part->chunks == NULL ?
			MemoryContextAlloc(spill->context, sizeof(void *) * part->slots) :
			repalloc(part->chunks, sizeof(void *) * part->slots);
	}
	part->chunks[part->nchunks++] = base;
	part->bytes += spill->chunk_len;
}

static void *
agg_new_chunk(TessAggState *state, AggSpill *spill)
{
	void	   *base = MemoryContextAlloc(spill->context, spill->chunk_len);

	check(state, state->kernels->table_chunk_init(base, spill->chunk_len,
												  &state->status));
	return base;
}

/* Free a partition's chunks in memory. */
static void
part_release(AggSpill *spill, AggPart *part)
{
	for (int chunk = 0; chunk < part->nchunks; chunk++)
		pfree(part->chunks[chunk]);
	part->nchunks = 0;
	part->bytes = 0;
	part->records = 0;
}

/* Write a chunk of the partition's records, unless it holds none. */
static void
agg_write_chunk(TessAggState *state, AggSpill *spill, int partition, void *base)
{
	Size		used = agg_chunk_used(base);

	if (used <= TESS_TABLE_CHUNK_HEADER)
		return;
	/* disk_bytes of a partition: what it takes read back; the node's, what was stored. */
	state->disk_bytes += tess_spill_write(spill->file, partition, TESS_SPILL_RECORDS,
										  spill->next_number++, base, used, NULL);
	spill->parts[partition].disk_bytes += used;
	spill->parts[partition].disk_records +=
		(used - TESS_TABLE_CHUNK_HEADER) / state->record_size;
	state->spilled++;
}

/*
 * A level of partitions by the hash bits from shift, for expected bytes:
 * the power of two that makes each about half of hash_mem, as long as the
 * bits last and a chunk per partition fits in half of hash_mem.
 */
static AggSpill *
agg_spill_create(TessAggState *state, AggSpill *parent, double expected, uint32 shift)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	Size		limit = get_hash_memory_limit();
	Size		record = state->record_size;
	TessSpillConfig config = TESS_STRUCT_INITIALIZER(TessSpillConfig);
	TessTableRef layout = {0};
	AggSpill   *spill = MemoryContextAllocZero(context, sizeof(AggSpill));
	uint32		npartitions;

	/* Each partition keeps a chunk and its file's buffer of a page. */
	check(state, state->kernels->spill_partitions(expected, limit, AGG_SPILL_MIN_CHUNK + BLCKSZ,
												  shift, AGG_SPILL_MIN_PARTITIONS,
												  AGG_SPILL_MAX_PARTITIONS, 0,
												  &npartitions, &state->status));
	/* A chunk an eighth of hash_mem among them, of four records at least. */
	check(state, state->kernels->spill_chunk_len(limit, npartitions, 8,
												 Max(AGG_SPILL_MIN_CHUNK,
													 TESS_TABLE_CHUNK_HEADER + 4 * record),
												 TESS_TABLE_MAX_CHUNK_LEN,
												 &spill->chunk_len, &state->status));
	spill->parent = parent;
	spill->level = parent == NULL ? 0 : parent->level + 1;
	spill->shift = shift;
	spill->npartitions = npartitions;
	spill->partition = -1;
	spill->context = AllocSetContextCreate(context, "TessAgg spill",
										   TESS_CHUNK_CONTEXT_SIZES);
	spill->block_context = AllocSetContextCreate(spill->context,
												 "TessAgg spilled block",
												 TESS_CHUNK_CONTEXT_SIZES);
	spill->parts = MemoryContextAllocZero(spill->context,
										  sizeof(AggPart) * npartitions);
	{
		MemoryContext old = MemoryContextSwitchTo(spill->context);

		for (int partition = 0; partition < npartitions; partition++)
			initHyperLogLog(&spill->parts[partition].groups, AGG_GROUPS_WIDTH);
		MemoryContextSwitchTo(old);
	}
	spill->bases = MemoryContextAlloc(spill->context,
									  sizeof(void *) * (npartitions + 2));
	spill->lens = MemoryContextAlloc(spill->context, sizeof(Size) * (npartitions + 2));
	spill->current = MemoryContextAlloc(spill->context, sizeof(uint32) * npartitions);
	spill->source_empty[0] = TESS_TABLE_CHUNK_HEADER;
	spill->empty[0] = TESS_TABLE_CHUNK_HEADER;
	spill->bases[AGG_SOURCE] = spill->source_empty;
	spill->lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
	for (int partition = 0; partition < npartitions; partition++)
	{
		spill->bases[AGG_EMPTY + 1 + partition] = spill->empty;
		spill->lens[AGG_EMPTY + 1 + partition] = TESS_TABLE_CHUNK_HEADER;
		spill->current[partition] = AGG_EMPTY + 1 + partition;
	}
	spill->bases[AGG_EMPTY] = spill->empty;
	spill->lens[AGG_EMPTY] = TESS_TABLE_CHUNK_HEADER;
	/* The files' fingerprint: the table's layout, from the index. */
	layout.index = state->layout_index;
	layout.index_len = state->layout_len;
	layout.chunks = spill->bases;
	layout.chunk_lens = spill->lens;
	check(state, state->kernels->table_fingerprint(&layout, &config.fingerprint,
												   &state->status));
	config.parent_context = spill->context;
	config.kernels = state->kernels;
	config.npartitions = npartitions;
	config.level = spill->level;
	config.max_len = (uint64) MaxAllocHugeSize;
	config.buffer_len = TESS_SPILL_BUFFER_LEN(get_hash_memory_limit());
	spill->file = tess_spill_create(&config);
	state->partitions = Max(state->partitions, (uint64) npartitions);
	return spill;
}

/* Delete a level's files and free its memory. */
static void
agg_level_free(AggSpill *spill)
{
	tess_spill_free(spill->file);
	MemoryContextDelete(spill->context);
	pfree(spill);
}

void
agg_spill_free(TessAggState *state)
{
	while (state->spill != NULL)
	{
		AggSpill   *parent = state->spill->parent;

		agg_level_free(state->spill);
		state->spill = parent;
	}
}

/*
 * Split a chunk of records, placed at the source slot, into the level's
 * partitions by the kernel: a partition whose chunk fills keeps it and
 * gets another with keep, or writes it and starts it again otherwise.
 */
static void
agg_split(TessAggState *state, AggSpill *spill, void *base, Size len, bool keep)
{
	TessTableRef ref = {0};
	uint32		offsets[AGG_GROUP_ROWS];
	uint32		hashes[AGG_GROUP_ROWS];
	Size		from = TESS_TABLE_CHUNK_HEADER;

	spill->bases[AGG_SOURCE] = base;
	spill->lens[AGG_SOURCE] = len;
	ref.chunks = spill->bases;
	ref.chunk_lens = spill->lens;
	ref.nchunks = spill->npartitions + 2;
	for (;;)
	{
		int			count;
		int			full;

		check(state, state->kernels->table_split(&ref, state->nkeys, state->kinds,
												 state->payload_size,
												 spill->current, spill->npartitions,
												 spill->shift, AGG_SOURCE, &from,
												 AGG_GROUP_ROWS, offsets, hashes,
												 &count, &full, &state->status));
		for (int index = 0; index < count; index++)
		{
			AggPart    *part = &spill->parts[agg_partition(spill, hashes[index])];

			part->records++;
			addHyperLogLog(&part->groups, murmurhash32(hashes[index]));
		}
		if (full >= 0)
		{
			AggPart    *part = &spill->parts[full];
			int			slot = AGG_EMPTY + 1 + full;

			if (spill->bases[slot] == spill->empty)
			{
				void	   *chunk = agg_new_chunk(state, spill);

				part_push(spill, part, chunk);
				spill->bases[slot] = chunk;
				spill->lens[slot] = spill->chunk_len;
			}
			else if (keep)
			{
				void	   *chunk = agg_new_chunk(state, spill);

				part_push(spill, part, chunk);
				spill->bases[slot] = chunk;
			}
			else
			{
				agg_write_chunk(state, spill, full, spill->bases[slot]);
				part->records = 0;
				check(state, state->kernels->table_chunk_init(spill->bases[slot],
															  spill->chunk_len,
															  &state->status));
			}
		}
		else if (count == 0)
			break;
	}
	spill->bases[AGG_SOURCE] = spill->source_empty;
	spill->lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
}

/*
 * The table over the partitions' chunks in memory: the chunk arrays made
 * anew, each partition appending to its last chunk, and an index for
 * twice their records, into which every record is linked.
 */
static void
agg_table_from_parts(TessAggState *state, AggSpill *spill, uint64 capacity)
{
	Size		payload_size = state->payload_size;
	int			nchunks = 2;
	Size		size;
	void	   *old = state->table.index;

	for (int partition = 0; partition < spill->npartitions; partition++)
		nchunks += spill->parts[partition].nchunks;
	if (nchunks > state->chunk_slots)
	{
		state->chunk_slots = Max(nchunks, state->chunk_slots * 2);
		state->chunk_bases = repalloc(state->chunk_bases,
									  sizeof(void *) * state->chunk_slots);
		state->chunk_lens = repalloc(state->chunk_lens,
									 sizeof(Size) * state->chunk_slots);
	}
	state->chunk_bases[AGG_SOURCE] = spill->source_empty;
	state->chunk_lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
	state->chunk_bases[AGG_EMPTY] = spill->empty;
	state->chunk_lens[AGG_EMPTY] = TESS_TABLE_CHUNK_HEADER;
	nchunks = 2;
	state->table_bytes = 0;
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		AggPart    *part = &spill->parts[partition];

		spill->current[partition] = AGG_EMPTY;
		for (int chunk = 0; chunk < part->nchunks; chunk++)
		{
			state->chunk_bases[nchunks] = part->chunks[chunk];
			state->chunk_lens[nchunks] = spill->chunk_len;
			spill->current[partition] = nchunks++;
			state->table_bytes += spill->chunk_len;
		}
	}
	state->table.chunks = state->chunk_bases;
	state->table.chunk_lens = state->chunk_lens;
	state->table.nchunks = nchunks;
	capacity = first_capacity(capacity);
	state->table.index = new_index(state, capacity, &size);
	state->table.index_len = size;
	check(state, state->kernels->table_create(state->table.index, size, state->nkeys,
											  state->kinds, payload_size,
											  capacity, &state->status));
	for (int chunk = AGG_EMPTY + 1; chunk < nchunks; chunk++)
	{
		Size		from = TESS_TABLE_CHUNK_HEADER;

		check(state, state->kernels->table_link(&state->table, chunk, &from, NULL,
												NULL, &state->status));
	}
	if (old != NULL)
		pfree(old);
	state->table_bytes += size;
	note_memory(state);
}

/*
 * The table outgrew hash_mem: the first level of partitions, for twice the
 * groups so far or the planner's if more, and the records so far split
 * into them.
 */
void
agg_start_spill(TessAggState *state)
{
	TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
	double		bytes = state->table_bytes;
	double		expected;
	AggSpill   *spill;
	int			nold = state->table.nchunks;
	void	  **old = palloc(sizeof(void *) * Max(nold, 1));
	Size	   *old_lens = palloc(sizeof(Size) * Max(nold, 1));

	check(state, state->kernels->table_stats(&state->table, &stats, &state->status));
	expected = bytes * 2;
	if (stats.records > 0)
		expected = Max(expected,
					   bytes / stats.records * (double) state->groups_estimate);
	spill = agg_spill_create(state, NULL, expected, 0);
	state->spill = spill;
	memcpy(old, state->chunk_bases, sizeof(void *) * nold);
	memcpy(old_lens, state->chunk_lens, sizeof(Size) * nold);
	for (int chunk = 0; chunk < nold; chunk++)
	{
		agg_split(state, spill, old[chunk], old_lens[chunk], true);
		pfree(old[chunk]);
	}
	pfree(old);
	pfree(old_lens);
	/* Room for the index first: the old one goes, the new one is made last. */
	pfree(state->table.index);
	state->table.index = NULL;
	(void) agg_evict(state, sizeof(uint64) * first_capacity(agg_records(spill) * 2));
	agg_table_from_parts(state, spill, agg_records(spill) * 2);
}

/*
 * Once the table, with its files' buffers, takes more than seven eighths
 * of hash_mem, the partition with the most bytes in memory goes to disk
 * whole, and the next, until the table takes half of hash_mem; the index
 * is then made anew over the rest. Evicting down to the limit only made
 * the index anew after every partition: 5 M groups of a row each at a
 * work_mem of 4 MB made it 3598 times.
 */
static bool
agg_evict(TessAggState *state, Size extra)
{
	AggSpill   *spill = state->spill;
	/* An eighth of hash_mem is left for a batch's new chunks and index. */
	Size		limit = get_hash_memory_limit() / 8 * 7;
	Size		target = get_hash_memory_limit() / 2;
	bool		evicted = false;

	if (agg_spill_memory(state) + extra <= limit)
		return false;
	while (agg_spill_memory(state) + extra > target)
	{
		int			largest = -1;
		Size		bytes = 0;

		for (int partition = 0; partition < spill->npartitions; partition++)
			if (spill->parts[partition].bytes > bytes)
			{
				largest = partition;
				bytes = spill->parts[partition].bytes;
			}
		if (largest < 0)
			break;
		for (int chunk = 0; chunk < spill->parts[largest].nchunks; chunk++)
			agg_write_chunk(state, spill, largest, spill->parts[largest].chunks[chunk]);
		part_release(spill, &spill->parts[largest]);
		state->evictions++;
		evicted = true;
	}
	return evicted;
}

/* The records of the partitions in memory, for their index. */
static uint64
agg_records(AggSpill *spill)
{
	uint64		records = 0;

	for (int partition = 0; partition < spill->npartitions; partition++)
		records += spill->parts[partition].records;
	return records;
}

void
agg_make_room(TessAggState *state)
{
	TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
	Size		extra = 0;

	/*
	 * An index a batch could fill grows by a new one twice its size next
	 * to it: counted now, so that the partitions go to disk before the
	 * table outgrows hash_mem in the middle of a batch.
	 */
	check(state, state->kernels->table_stats(&state->table, &stats, &state->status));
	if ((stats.records + state->capacity) * 2 >= stats.buckets)
		extra = 2 * state->table.index_len;
	if (agg_evict(state, extra))
		agg_table_from_parts(state, state->spill, agg_records(state->spill) * 2);
}

/*
 * The rows of a batch into the groups of a table that spills: new groups
 * go to their partitions' chunks, a partition without room getting
 * another; a full index grows. Returns with every row resolved.
 */
void
agg_find_partitioned(TessAggState *state, TessRowMask *pending, TessRowMask *inserted)
{
	AggSpill   *spill = state->spill;
	int			nrows = pending->nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	uint64	   *found = palloc0(sizeof(uint64) * nwords);
	bool	   *seen = palloc(sizeof(bool) * spill->npartitions);

	for (;;)
	{
		TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
		TessRowMask created = {nrows, found};
		int			row = -1;
		bool		index_full;

		memset(found, 0, sizeof(uint64) * nwords);
		check(state, state->kernels->table_find_or_insert_partitioned(&state->table,
																	  spill->current,
																	  spill->npartitions,
																	  spill->shift,
																	  state->hashes,
																	  state->nkeys,
																	  state->table_keys,
																	  pending,
																	  state->offsets,
																	  &created,
																	  &state->status));
		while ((row = tess_row_mask_next(&created, row)) >= 0)
		{
			AggPart    *part = &spill->parts[agg_partition(spill, state->hashes[row])];

			part->records++;
			/* The partition's bits are the hash's low ones: mixed first. */
			addHyperLogLog(&part->groups, murmurhash32(state->hashes[row]));
		}
		for (int word = 0; word < nwords; word++)
			inserted->bits[word] |= found[word];
		if (tess_row_mask_count(pending) == 0)
			break;
		check(state, state->kernels->table_stats(&state->table, &stats,
												 &state->status));
		index_full = stats.records * 2 >= stats.buckets;
		if (index_full)
		{
			regrow_table(state, stats.records);
			continue;
		}
		/*
		 * A new chunk for each partition that has rows left: its chunk ran
		 * out of room, or it had none.
		 */
		memset(seen, 0, sizeof(bool) * spill->npartitions);
		row = -1;
		while ((row = tess_row_mask_next(pending, row)) >= 0)
		{
			int			partition = agg_partition(spill, state->hashes[row]);
			AggPart    *part = &spill->parts[partition];
			int			chunk = state->table.nchunks;

			if (seen[partition])
				continue;
			seen[partition] = true;
			if (chunk == state->chunk_slots)
			{
				state->chunk_slots *= 2;
				state->chunk_bases = repalloc(state->chunk_bases,
											  sizeof(void *) * state->chunk_slots);
				state->chunk_lens = repalloc(state->chunk_lens,
											 sizeof(Size) * state->chunk_slots);
				state->table.chunks = state->chunk_bases;
				state->table.chunk_lens = state->chunk_lens;
			}
			if (chunk == TESS_TABLE_MAX_CHUNKS)
				ereport(ERROR,
						(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
						 errmsg("TessAgg group table cannot hold more than %d chunks",
								TESS_TABLE_MAX_CHUNKS)));
			state->chunk_bases[chunk] = agg_new_chunk(state, spill);
			state->chunk_lens[chunk] = spill->chunk_len;
			part_push(spill, part, state->chunk_bases[chunk]);
			spill->current[partition] = chunk;
			state->table.nchunks++;
			state->table_bytes += spill->chunk_len;
		}
		note_memory(state);
	}
	pfree(found);
	pfree(seen);
}

/*
 * Merge a chunk of groups' states, placed at the source slot, into the
 * partition's table by the kernel: a group the table lacks goes to the
 * chunk at *dest, or to a new one of the partition's when that is full.
 */
static void
agg_combine(TessAggState *state, AggSpill *spill, AggPart *part, void *base,
			Size len, int *dest)
{
	Size		from = TESS_TABLE_CHUNK_HEADER;

	state->chunk_bases[AGG_SOURCE] = base;
	state->chunk_lens[AGG_SOURCE] = len;
	for (;;)
	{
		int			merged;
		int			stop;

		/* A partition with nothing in memory takes a chunk for its groups. */
		if (*dest <= AGG_EMPTY)
			stop = TESS_TABLE_COMBINE_CHUNK_FULL;
		else
			check(state, state->kernels->table_combine(&state->table, AGG_SOURCE,
													   &from, *dest, state->nvalues,
													   state->combines, &merged,
													   &stop, &state->status));
		if (stop == TESS_TABLE_COMBINE_DONE)
			break;
		if (stop == TESS_TABLE_COMBINE_INDEX_FULL)
		{
			TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);

			/*
			 * A new index links every chunk's records: the source's, merged
			 * or not, are no groups of the table. Hidden, or a group whose
			 * record the source still holds would be found there and never
			 * merged into its record of the table.
			 */
			state->chunk_bases[AGG_SOURCE] = spill->source_empty;
			state->chunk_lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
			check(state, state->kernels->table_stats(&state->table, &stats,
													 &state->status));
			regrow_table(state, stats.records);
			state->chunk_bases[AGG_SOURCE] = base;
			state->chunk_lens[AGG_SOURCE] = len;
			continue;
		}
		if (state->table.nchunks == state->chunk_slots)
		{
			state->chunk_slots *= 2;
			state->chunk_bases = repalloc(state->chunk_bases,
										  sizeof(void *) * state->chunk_slots);
			state->chunk_lens = repalloc(state->chunk_lens,
										 sizeof(Size) * state->chunk_slots);
			state->table.chunks = state->chunk_bases;
			state->table.chunk_lens = state->chunk_lens;
		}
		*dest = state->table.nchunks++;
		state->chunk_bases[*dest] = agg_new_chunk(state, spill);
		state->chunk_lens[*dest] = spill->chunk_len;
		part_push(spill, part, state->chunk_bases[*dest]);
	}
	state->chunk_bases[AGG_SOURCE] = spill->source_empty;
	state->chunk_lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
}

/*
 * The groups a partition merges into: its estimate with a third more for
 * the estimate's error, at most its records.
 */
static uint64
part_groups(AggPart *part)
{
	double		groups = estimateHyperLogLog(&part->groups) * 4 / 3;

	return (uint64) Min(groups, (double) (part->records + part->disk_records));
}

/*
 * The table of one partition, with an index for its records in memory and
 * on disk: its chunks in memory linked, when they hold each group once, as
 * those the first level found by the index do, and merged by the kernel
 * otherwise, as a level below's split them; then its chunks read back
 * merged in, a group the table lacks copied to its last chunk or a new one.
 */
static void
agg_merge(TessAggState *state, AggSpill *spill, int partition)
{
	AggPart    *part = &spill->parts[partition];
	Size		payload_size = state->payload_size;
	uint64		capacity = first_capacity(part_groups(part));
	bool		unique = spill->parent == NULL;
	void	  **split = NULL;
	int			nsplit = 0;
	TessSpillReader *reader;
	TessSpillHeader header;
	int			nchunks = 2;
	Size		size;
	int			dest;

	/* A level below's chunks are merged as sources, the partition's afresh. */
	if (!unique && part->nchunks > 0)
	{
		nsplit = part->nchunks;
		split = palloc(sizeof(void *) * nsplit);
		memcpy(split, part->chunks, sizeof(void *) * nsplit);
		part->nchunks = 0;
		part->bytes = 0;
	}

	if (state->table.index != NULL)
		pfree(state->table.index);
	if (part->nchunks + 3 > state->chunk_slots)
	{
		state->chunk_slots = Max(part->nchunks + 3, state->chunk_slots * 2);
		state->chunk_bases = repalloc(state->chunk_bases,
									  sizeof(void *) * state->chunk_slots);
		state->chunk_lens = repalloc(state->chunk_lens,
									 sizeof(Size) * state->chunk_slots);
	}
	state->chunk_bases[AGG_SOURCE] = spill->source_empty;
	state->chunk_lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
	state->chunk_bases[AGG_EMPTY] = spill->empty;
	state->chunk_lens[AGG_EMPTY] = TESS_TABLE_CHUNK_HEADER;
	for (int chunk = 0; chunk < part->nchunks; chunk++)
	{
		state->chunk_bases[nchunks] = part->chunks[chunk];
		state->chunk_lens[nchunks++] = spill->chunk_len;
	}
	state->table.chunks = state->chunk_bases;
	state->table.chunk_lens = state->chunk_lens;
	state->table.nchunks = nchunks;
	state->table.index = new_index(state, capacity, &size);
	state->table.index_len = size;
	check(state, state->kernels->table_create(state->table.index, size, state->nkeys,
											  state->kinds, payload_size,
											  capacity, &state->status));
	for (int chunk = AGG_EMPTY + 1; chunk < nchunks; chunk++)
	{
		Size		from = TESS_TABLE_CHUNK_HEADER;

		check(state, state->kernels->table_link(&state->table, chunk, &from, NULL,
												NULL, &state->status));
	}
	dest = nchunks - 1;
	for (int chunk = 0; chunk < nsplit; chunk++)
	{
		CHECK_FOR_INTERRUPTS();
		agg_combine(state, spill, part, split[chunk], spill->chunk_len, &dest);
		pfree(split[chunk]);
	}
	if (split != NULL)
		pfree(split);
	reader = tess_spill_open(spill->file, 0, partition);
	while (reader != NULL && tess_spill_read_header(reader, &header))
	{
		void	   *body;

		CHECK_FOR_INTERRUPTS();
		body = MemoryContextAllocExtended(spill->block_context, Max(header.len, 8),
										  MCXT_ALLOC_HUGE);
		tess_spill_read_body(reader, body, header.len);
		agg_combine(state, spill, part, body, header.len, &dest);
		MemoryContextReset(spill->block_context);
	}
	if (reader != NULL)
		tess_spill_close(reader);
	tess_spill_drop(spill->file, partition);
	state->table_bytes = size + part->bytes;
	note_memory(state);
}

/*
 * Before a level's partitions are given out: a partition with records on
 * disk writes its chunks in memory too, since it merges from disk anyway;
 * kept, they would narrow the room every other partition merges in, and
 * a partition that does not fit splits, writing all its records again.
 * Partitions wholly in memory stay.
 */
static void
agg_flush_spilled(TessAggState *state, AggSpill *spill)
{
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		AggPart    *part = &spill->parts[partition];

		if (part->disk_records == 0 || part->nchunks == 0)
			continue;
		for (int chunk = 0; chunk < part->nchunks; chunk++)
			agg_write_chunk(state, spill, partition, part->chunks[chunk]);
		part_release(spill, part);
	}
}

/*
 * A partition too large to merge splits by the next bits of the hash
 * into a level of its own: its chunks read back, then those in memory,
 * each split into the new level's partitions, which keep a chunk each in
 * memory and write the others. The new level is given out next.
 */
static void
agg_split_level(TessAggState *state, AggSpill *spill, int partition)
{
	AggPart    *part = &spill->parts[partition];
	AggSpill   *level = agg_spill_create(state, spill,
										 (double) part->disk_bytes + part->bytes,
										 spill->shift + pg_leftmost_one_pos32(spill->npartitions));
	TessSpillReader *reader = tess_spill_open(spill->file, 0, partition);
	TessSpillHeader header;

	state->splits++;
	while (reader != NULL && tess_spill_read_header(reader, &header))
	{
		void	   *body;

		CHECK_FOR_INTERRUPTS();
		body = MemoryContextAllocExtended(level->block_context, Max(header.len, 8),
										  MCXT_ALLOC_HUGE);
		tess_spill_read_body(reader, body, header.len);
		agg_split(state, level, body, header.len, false);
		MemoryContextReset(level->block_context);
	}
	if (reader != NULL)
		tess_spill_close(reader);
	for (int chunk = 0; chunk < part->nchunks; chunk++)
	{
		CHECK_FOR_INTERRUPTS();
		agg_split(state, level, part->chunks[chunk], spill->chunk_len, false);
	}
	part_release(spill, part);
	tess_spill_drop(spill->file, partition);
	level->done_input = true;
	agg_flush_spilled(state, level);
	tess_spill_finish(level->file);
	state->spill = level;
	note_memory(state);
}

/*
 * The next partition to give out, merged into a table: after the input,
 * the partitions of the first level in turn, those wholly in memory
 * first, so that a partition read back from disk merges with the most
 * room; a level below given out whole where one split, and then the level
 * above again. False once every group is out.
 */
bool
agg_advance(TessAggState *state)
{
	Size		limit = get_hash_memory_limit();

	for (;;)
	{
		AggSpill   *spill = state->spill;
		AggPart    *part;
		Size		others = 0;
		Size		size;

		CHECK_FOR_INTERRUPTS();

		if (spill->given)
			part_release(spill, &spill->parts[spill->partition]);
		spill->given = false;
		if (++spill->partition >= spill->npartitions)
		{
			if (spill->pass == 0)
			{
				spill->pass = 1;
				spill->partition = -1;
				continue;
			}
			if (spill->parent == NULL)
				return false;
			state->spill = spill->parent;
			agg_level_free(spill);
			continue;
		}
		part = &spill->parts[spill->partition];
		if (part->records == 0 && part->disk_records == 0)
			continue;
		if (spill->pass == 0 && part->disk_records > 0)
			continue;
		/*
		 * What the partition takes merged: a record per group and its
		 * index, and a block read back, next to the chunks every level
		 * keeps. A group written many times merges into one record, so the
		 * groups decide, not the file.
		 */
		size = part_groups(part) * (state->record_size + 2 * sizeof(uint64)) +
			spill->chunk_len;
		for (AggSpill *level = spill; level != NULL; level = level->parent)
			for (int partition = 0; partition < level->npartitions; partition++)
				if (level != spill || partition != spill->partition)
					others += level->parts[partition].bytes;
		if (part->disk_bytes > 0 && others + size > limit &&
			spill->shift + pg_leftmost_one_pos32(spill->npartitions) + 2 <= 32)
		{
			agg_split_level(state, spill, spill->partition);
			continue;
		}
		agg_merge(state, spill, spill->partition);
		spill->given = true;
		return true;
	}
}

/* The input is done: the partitions are given out one by one. */
void
agg_finish_input(TessAggState *state)
{
	AggSpill   *spill = state->spill;

	spill->done_input = true;
	spill->partition = -1;
	agg_flush_spilled(state, spill);
	tess_spill_finish(spill->file);
	if (state->table.index != NULL)
		pfree(state->table.index);
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->table_bytes = 0;
	state->cursor = 0;
	if (!agg_advance(state))
		state->table.nchunks = 0;
}

Size
agg_spill_memory(TessAggState *state)
{
	Size		memory = state->table.index != NULL ? state->table.index_len : 0;

	for (AggSpill *spill = state->spill; spill != NULL; spill = spill->parent)
		memory += MemoryContextMemAllocated(spill->context, true);
	return memory;
}

/* ------------------------------------------------------ rows past hash_mem */

/* Rows of a block, and the first bytes of its values. */
#define ROWS_BLOCK_ROWS 256
#define ROWS_BLOCK_VALUES 8192

/* The block a partition fills: its columns and its by-reference values. */
typedef struct RowWriter
{
	void	   *chunk;
	Size		chunk_len;
	uint32		capacity;
	uint32		rows;
	char	   *values;
	Size		values_len;
	Size		values_used;
	uint64		written;
} RowWriter;

/* A level of partitions: one set of files, a partition per ROWS_PART_BITS bits. */
typedef struct RowSpill
{
	TessSpill  *file;
	int			level;
	RowWriter	writers[ROWS_PARTS];
	/* Reading: the next partition to read. */
	int			next;
} RowSpill;

/* A partition being read back: the block in hand and its next row. */
typedef struct RowReader
{
	TessSpillReader *file;
	void	   *chunk;
	Size		chunk_len;
	char	   *values;
	Size		values_len;
	uint32		rows;
	uint32		next;
	/* The batch given to group_batch: a window of the block. */
	TessBatch	batch;
	uint64		bits;
	Datum	  **column_values;
	bool	  **column_isnull;
} RowReader;

/* Words of a block: one per computed column. */
static int
rows_words(TessAggState *state)
{
	return state->ncomputed;
}

static void
rows_writer_reset(TessAggState *state, RowWriter *writer)
{
	Size		capacity;

	check(state, state->kernels->spill_columns_init(writer->chunk, writer->chunk_len,
													rows_words(state), &capacity,
													&state->status));
	writer->capacity = (uint32) capacity;
	writer->rows = 0;
	writer->values_used = 0;
}

/* A level of partitions, their files not made until written. */
RowSpill *
rows_spill_create(TessAggState *state, int level)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	TessSpillConfig config = TESS_STRUCT_INITIALIZER(TessSpillConfig);
	RowSpill   *spill = MemoryContextAllocZero(context, sizeof(RowSpill));
	int			null_lanes = tess_spill_columns_null_lanes(rows_words(state));

	config.parent_context = context;
	config.kernels = state->kernels;
	config.npartitions = ROWS_PARTS;
	config.level = (uint32) level;
	config.fingerprint = (uint64) rows_words(state);
	config.max_len = MaxAllocHugeSize;
	config.buffer_len = TESS_SPILL_BUFFER_LEN(get_hash_memory_limit());
	spill->file = tess_spill_create(&config);
	spill->level = level;
	for (int part = 0; part < ROWS_PARTS; part++)
	{
		RowWriter  *writer = &spill->writers[part];

		writer->chunk_len = TESS_SPILL_COLUMNS_HEADER +
			sizeof(uint64) * ROWS_BLOCK_ROWS * (null_lanes + rows_words(state));
		writer->chunk = MemoryContextAlloc(context, writer->chunk_len);
		writer->values_len = ROWS_BLOCK_VALUES;
		writer->values = MemoryContextAlloc(context, writer->values_len);
		rows_writer_reset(state, writer);
	}
	state->partitions += ROWS_PARTS;
	return spill;
}

/* Write a partition's block: its values, then its columns. */
static void
rows_flush(TessAggState *state, RowSpill *spill, int part)
{
	RowWriter  *writer = &spill->writers[part];

	if (writer->rows == 0)
		return;
	tess_spill_columns_set_rows(writer->chunk, writer->rows);
	state->disk_bytes += tess_spill_write(spill->file, part, TESS_SPILL_VALUES, 0,
										  writer->values, writer->values_used, NULL);
	state->disk_bytes += tess_spill_write(spill->file, part, TESS_SPILL_COLUMNS, 0,
										  writer->chunk, writer->chunk_len, NULL);
	state->spilled++;
	writer->written += writer->rows;
	rows_writer_reset(state, writer);
}

/*
 * The rows of rows to their partitions by the bits of their hash of the
 * spill's level: the values of every computed column, a by-reference one
 * copied into the block's values.
 */
void
rows_write(TessAggState *state, RowSpill *spill, const TessRowMask *rows)
{
	int			null_lanes = tess_spill_columns_null_lanes(rows_words(state));
	int			shift = 32 - ROWS_PART_BITS * (spill->level + 1);
	int			row = -1;

	/* By the values' hashes when a key has a dictionary: the numbers are one table's. */
	const uint32 *hashes = state->has_dicts ? state->value_hashes : state->hashes;

	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		int			part = (int) tess_table_partition(hashes[row], shift, ROWS_PARTS);
		RowWriter  *writer = &spill->writers[part];
		Size		need = 0;
		uint64	   *nulls;
		uint64	   *words;

		for (int column = 0; column < state->ncomputed; column++)
			if (!state->computed_byvals[column] && !state->computed_columns[column].isnull[row])
				need += MAXALIGN(datumGetSize(state->computed_columns[column].values[row], false,
											  state->computed_lens[column]));
		if (writer->rows == writer->capacity ||
			(writer->rows > 0 && writer->values_used + need > writer->values_len))
			rows_flush(state, spill, part);
		if (writer->values_used + need > writer->values_len)
		{
			writer->values_len = Max(writer->values_len * 2, writer->values_used + need);
			writer->values = repalloc_huge(writer->values, writer->values_len);
		}
		nulls = tess_spill_columns_lane(writer->chunk, 0) + writer->rows;
		words = nulls + (Size) writer->capacity * null_lanes;
		for (int lane = 0; lane < null_lanes; lane++)
			nulls[(Size) writer->capacity * lane] = 0;
		for (int column = 0; column < state->ncomputed; column++)
		{
			const TessDatumColumn *from = &state->computed_columns[column];
			uint64	   *lane = words + (Size) writer->capacity * column;

			if (from->isnull[row])
			{
				nulls[(Size) writer->capacity * tess_spill_columns_null_lane(column)] |= tess_spill_columns_null_bit(column);
				lane[0] = 0;
			}
			else if (state->computed_byvals[column])
				lane[0] = (uint64) from->values[row];
			else
			{
				Size		size = datumGetSize(from->values[row], false,
												state->computed_lens[column]);

				memcpy(writer->values + writer->values_used,
					   DatumGetPointer(from->values[row]), size);
				lane[0] = writer->values_used;
				writer->values_used += MAXALIGN(size);
			}
		}
		writer->rows++;
		state->spilled_rows++;
	}
}

/* End a level's writes and put it on the stack of partitions to read. */
void
rows_spill_close(TessAggState *state)
{
	RowSpill   *spill = state->rows_spill;

	if (spill == NULL)
		return;
	for (int part = 0; part < ROWS_PARTS; part++)
		rows_flush(state, spill, part);
	tess_spill_finish(spill->file);
	spill->next = 0;
	state->rows_pending = lcons(spill, state->rows_pending);
	state->rows_spill = NULL;
}

static void
rows_spill_free_one(RowSpill *spill)
{
	for (int part = 0; part < ROWS_PARTS; part++)
	{
		pfree(spill->writers[part].chunk);
		pfree(spill->writers[part].values);
	}
	tess_spill_free(spill->file);
	pfree(spill);
}

/* Forget every level of partitions and the partition being read. */
void
rows_spill_free(TessAggState *state)
{
	if (state->reader != NULL)
	{
		if (state->reader->file != NULL)
			tess_spill_close(state->reader->file);
		state->reader->file = NULL;
	}
	if (state->rows_spill != NULL)
		rows_spill_free_one(state->rows_spill);
	state->rows_spill = NULL;
	foreach_ptr(RowSpill, spill, state->rows_pending)
		rows_spill_free_one(spill);
	list_free(state->rows_pending);
	state->rows_pending = NIL;
	state->frozen = false;
	state->replaying = false;
}

/* A column of the window of rows read back: the computed column it holds. */
static void
reader_get_column(TessBatch *batch, int column, const TessRowMask *rows,
				  TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessAggState *state = (TessAggState *) batch->private_data;
	int			computed = column - state->child_layout.ncolumns;

	if (computed < 0 || computed >= state->ncomputed)
		elog(ERROR, "TessAgg read back no column %d", column);
	result->values = state->reader->column_values[computed];
	result->isnull = state->reader->column_isnull[computed];
	result->nrows = batch->rows.nrows;
}

static const TessBatchOps reader_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = reader_get_column,
};

/*
 * The next window of up to 64 rows of the partition being read, as a
 * batch whose computed columns are the values written; NULL at the end.
 */
TessBatch *
reader_next(TessAggState *state)
{
	RowReader  *reader = state->reader;
	int			null_lanes = tess_spill_columns_null_lanes(rows_words(state));
	uint32		take;
	Size		capacity;

	while (reader->next >= reader->rows)
	{
		TessSpillHeader header;

		CHECK_FOR_INTERRUPTS();
		if (reader->file == NULL || !tess_spill_read_header(reader->file, &header))
			return NULL;
		if (header.kind != TESS_SPILL_VALUES)
			ereport(ERROR,
					errcode(ERRCODE_DATA_CORRUPTED),
					errmsg("TessAgg read a damaged partition of rows"));
		if (header.len > reader->values_len)
		{
			reader->values_len = Max(header.len, reader->values_len * 2);
			reader->values = repalloc_huge(reader->values, reader->values_len);
		}
		tess_spill_read_body(reader->file, reader->values, header.len);
		if (!tess_spill_read_header(reader->file, &header) ||
			header.kind != TESS_SPILL_COLUMNS || header.len > reader->chunk_len)
			ereport(ERROR,
					errcode(ERRCODE_DATA_CORRUPTED),
					errmsg("TessAgg read a damaged partition of rows"));
		tess_spill_read_body(reader->file, reader->chunk, header.len);
		reader->rows = tess_spill_columns_rows(reader->chunk);
		reader->next = 0;
	}
	capacity = tess_spill_columns_capacity(reader->chunk);
	take = Min(reader->rows - reader->next, 64);
	for (int column = 0; column < state->ncomputed; column++)
	{
		const uint64 *nulls = tess_spill_columns_lane(reader->chunk, 0) +
			capacity * tess_spill_columns_null_lane(column) + reader->next;
		const uint64 *lane = tess_spill_columns_lane(reader->chunk, 0) +
			capacity * (null_lanes + column) + reader->next;

		for (uint32 row = 0; row < take; row++)
		{
			bool		isnull = tess_spill_columns_is_null(nulls[row], column);

			reader->column_isnull[column][row] = isnull;
			reader->column_values[column][row] = isnull ? (Datum) 0 :
				state->computed_byvals[column] ? (Datum) lane[row] :
				PointerGetDatum(reader->values + lane[row]);
		}
	}
	reader->next += take;
	reader->bits = take == 64 ? UINT64_MAX : (UINT64CONST(1) << take) - 1;
	reader->batch.rows.nrows = (int) take;
	reader->batch.rows.bits = &reader->bits;
	return &reader->batch;
}

/*
 * Open the next partition to read, depth first: the last level written
 * first; false when none is left. Its groups start in a table of their
 * own, and a partition too large again spills into a level below.
 */
bool
rows_next_partition(TessAggState *state)
{
	RowReader  *reader = state->reader;

	if (reader->file != NULL)
		tess_spill_close(reader->file);
	reader->file = NULL;
	while (state->rows_pending != NIL)
	{
		RowSpill   *spill = linitial(state->rows_pending);

		while (spill->next < ROWS_PARTS)
		{
			int			part = spill->next++;

			if (spill->writers[part].written == 0)
				continue;
			reader->file = tess_spill_open(spill->file, 0, part);
			reader->rows = 0;
			reader->next = 0;
			/* The level below takes the next bits, the last level none. */
			state->rows_level = spill->level + 1;
			return true;
		}
		state->rows_pending = list_delete_first(state->rows_pending);
		rows_spill_free_one(spill);
	}
	return false;
}

/* The reader's buffers and batch, once. */
void
rows_reader_init(TessAggState *state)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	RowReader  *reader = MemoryContextAllocZero(context, sizeof(RowReader));
	int			null_lanes = tess_spill_columns_null_lanes(rows_words(state));

	reader->chunk_len = TESS_SPILL_COLUMNS_HEADER +
		sizeof(uint64) * ROWS_BLOCK_ROWS * (null_lanes + rows_words(state));
	reader->chunk = MemoryContextAlloc(context, reader->chunk_len);
	reader->values_len = ROWS_BLOCK_VALUES;
	reader->values = MemoryContextAlloc(context, reader->values_len);
	reader->column_values = MemoryContextAlloc(context, sizeof(Datum *) * state->ncomputed);
	reader->column_isnull = MemoryContextAlloc(context, sizeof(bool *) * state->ncomputed);
	for (int column = 0; column < state->ncomputed; column++)
	{
		reader->column_values[column] = MemoryContextAllocZero(context, sizeof(Datum) * 64);
		reader->column_isnull[column] = MemoryContextAllocZero(context, sizeof(bool) * 64);
	}
	reader->batch.abi_version = TESS_BATCH_ABI_VERSION;
	reader->batch.struct_size = sizeof(TessBatch);
	reader->batch.ops = &reader_batch_ops;
	reader->batch.private_data = state;
	reader->batch.table_oid = InvalidOid;
	state->reader = reader;
}


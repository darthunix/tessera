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
 * the next bits of the hash into a level of its own. Each level's machine
 * (relspill's group) decides all of that; the node does the work.
 *
 * The chunk arrays keep two slots first, AGG_SOURCE and AGG_EMPTY (agg.h).
 */
#define AGG_SPILL_MIN_CHUNK (8 * 1024)
#define AGG_SPILL_MIN_PARTITIONS 4
#define AGG_SPILL_MAX_PARTITIONS 1024

/* A partition: its chunks in memory, the last the one it appends to. */
typedef struct AggPart
{
	void	  **chunks;
	int			nchunks;
	int			slots;
	Size		bytes;
	/* Records on disk. */
	uint64		disk_records;
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
	/*
	 * The partitions' bytes in memory, counted in words of the level's own
	 * for the rule that sends them to disk (docs/spill.md, "The weights"),
	 * outside the memory the level counts, as before the words; each
	 * partition's records in memory, and the bytes of its blocks written.
	 */
	uint64	   *words;
	Size		nwords;
	uint64	   *records;
	uint64	   *disk;
	const TessKernelOps *kernels;
	/*
	 * The level's machine: the first level's is the node's, a level below
	 * keeps its own.
	 */
	TessGroupMachine *machine;
	TessGroupMachine own;
	/*
	 * The partition being given out, -1 before any, and whether it was, to
	 * free it next.
	 */
	int			partition;
	bool		given;
	/* The empty chunks of the two first slots. */
	uint64		source_empty[1];
	uint64		empty[1];
	/* The level plan's first bit of a level below; the level's own is shift. */
	uint32		below;
} AggSpill;

static bool agg_evict(TessAggState *state, Size extra);
static uint64 agg_records(AggSpill *spill);
static void agg_split_level(TessAggState *state, AggSpill *spill, int partition,
							const TessGroupBelow *below);

static inline uint32
agg_partition(const AggSpill *spill, uint32 hash)
{
	return tess_table_partition(hash, spill->shift, (uint32) spill->npartitions);
}

/* Count bytes of a partition's chunks in memory in the level's words. */
static void
part_count(AggSpill *spill, AggPart *part, int64 delta)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	bool		over;

	if (delta != 0 &&
		spill->kernels->table_spill_add_bytes(spill->words, spill->nwords, false, delta,
											  (int32) (part - spill->parts), &over,
											  &status) != TESS_OK)
		tess_status_report(&status);
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
	part_count(spill, part, (int64) spill->chunk_len);
}

static void *
agg_new_chunk(TessAggState *state, AggSpill *spill)
{
	return table_chunk_new(state->kernels, spill->context, spill->chunk_len,
						   &state->status);
}

/* Free a partition's chunks in memory. */
static void
part_release(AggSpill *spill, AggPart *part)
{
	for (int chunk = 0; chunk < part->nchunks; chunk++)
		pfree(part->chunks[chunk]);
	part->nchunks = 0;
	part_count(spill, part, -(int64) part->bytes);
	part->bytes = 0;
	spill->records[part - spill->parts] = 0;
}

/* Write a chunk of the partition's records, unless it holds none. */
static void
agg_write_chunk(TessAggState *state, AggSpill *spill, int partition, void *base)
{
	Size		used = 0;

	check(state, state->kernels->table_chunk_used(base, spill->chunk_len, state->record_size,
												  &used, &state->status));
	if (used <= TESS_TABLE_CHUNK_HEADER)
		return;
	/* disk_bytes of a partition: what it takes read back; the node's, what was stored. */
	state->disk_bytes += tess_spill_write(spill->file, partition, TESS_SPILL_RECORDS,
										  spill->next_number++, base, used, NULL);
	spill->disk[partition] += used;
	spill->parts[partition].disk_records +=
		(used - TESS_TABLE_CHUNK_HEADER) / state->record_size;
	state->spilled++;
}

/* The shape of the grouping's levels. */
static TessSpillShape
agg_spill_shape(TessAggState *state)
{
	return (TessSpillShape) {
		.reserve = AGG_SPILL_MIN_CHUNK + BLCKSZ,
		.share = 8,
		.min_chunk = AGG_SPILL_MIN_CHUNK,
		.max_chunk = TESS_TABLE_MAX_CHUNK_LEN,
		.record = state->record_size,
		.min_partitions = AGG_SPILL_MIN_PARTITIONS,
		.max_partitions = AGG_SPILL_MAX_PARTITIONS,
	};
}

/*
 * A build starts: the machine of the grouping's spill, whose table fits
 * so far, with what every level shares: hash_mem, every level's; the
 * shape of the levels: each partition keeps a chunk and a page more at
 * the least, and the chunks take an eighth of hash_mem among the
 * partitions; a group merged, its record and its share of an index; the
 * share of hash_mem past which the table spills; and the weights of the
 * settings.
 */
void
agg_spill_begin(TessAggState *state)
{
	state->spill_params = (TessGroupParams) {
		.limit = tess_hash_memory_limit(),
		.shape = agg_spill_shape(state),
		.merged = state->record_size + 2 * sizeof(uint64),
		.fill = AGG_SPILL_FILL,
		.start = tess_agg_spill_start,
		.target = tess_agg_spill_target,
		.spilled = tess_agg_spill_spilled_weight,
		.split_room = tess_agg_spill_split_room,
	};
	check(state, state->kernels->group_init(&state->machine, &state->spill_params,
											&state->status));
}

/* Step the machine of a level, or the first one's before it is made (spill NULL), with event. */
static TessGroupAction
group_step(TessAggState *state, AggSpill *spill, TessGroupEvent event)
{
	TessGroupHost host = {0};
	TessGroupAction action;
	TessGroupMachine *machine = &state->machine;

	if (spill != NULL)
	{
		machine = spill->machine;
		host.words = spill->words;
		host.nwords = spill->nwords;
		host.records = spill->records;
		host.nrecords = spill->npartitions;
		host.disk = spill->disk;
		host.ndisk = spill->npartitions;
	}
	check(state, state->kernels->group_step(machine, &host, &event, &action, &state->status));
	return action;
}

/*
 * A level of partitions by the plan its machine gave, below parent or the
 * first; its machine is told it is made.
 */
static AggSpill *
agg_spill_create(TessAggState *state, AggSpill *parent, const TessSpillPlan *plan,
				 TessGroupMachine *machine)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	TessSpillConfig config = TESS_STRUCT_INITIALIZER(TessSpillConfig);
	TessTableRef layout = {0};
	AggSpill   *spill = MemoryContextAllocZero(context, sizeof(AggSpill));
	uint32		npartitions = plan->partitions;

	spill->chunk_len = plan->chunk_len;
	spill->parent = parent;
	spill->level = parent == NULL ? 0 : parent->level + 1;
	spill->shift = plan->shift;
	spill->below = plan->below;
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
	spill->records = MemoryContextAllocZero(spill->context, sizeof(uint64) * npartitions);
	spill->disk = MemoryContextAllocZero(spill->context, sizeof(uint64) * npartitions);
	spill->bases = MemoryContextAlloc(spill->context,
									  sizeof(void *) * (npartitions + 2));
	spill->lens = MemoryContextAlloc(spill->context, sizeof(Size) * (npartitions + 2));
	spill->current = MemoryContextAlloc(spill->context, sizeof(uint32) * npartitions);
	spill->kernels = state->kernels;
	spill->nwords = plan->words;
	spill->words = MemoryContextAllocZero(context, sizeof(uint64) * spill->nwords);
	check(state, state->kernels->table_chunk_init(spill->source_empty,
												  TESS_TABLE_CHUNK_HEADER,
												  &state->status));
	check(state, state->kernels->table_chunk_init(spill->empty,
												  TESS_TABLE_CHUNK_HEADER,
												  &state->status));
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
	config.max_chunk = spill->chunk_len;
	config.buffer_len = TESS_SPILL_BUFFER_LEN(tess_hash_memory_limit());
	spill->file = tess_spill_create(&config);
	state->partitions = Max(state->partitions, (uint64) npartitions);
	/* The machine clears the words for the level's partitions. */
	if (parent != NULL)
	{
		spill->own = *machine;
		machine = &spill->own;
	}
	spill->machine = machine;
	(void) group_step(state, spill, (TessGroupEvent) {.kind = TESS_GROUP_MADE});
	return spill;
}

/* Delete a level's files and free its memory. */
static void
agg_level_free(AggSpill *spill)
{
	tess_spill_free(spill->file);
	MemoryContextDelete(spill->context);
	pfree(spill->words);
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
 * partitions by the kernel: a partition whose chunk fills gets another,
 * or writes it and starts it again, as the machine says. With read_back,
 * a chunk the kernel refuses was read back damaged.
 */
static void
agg_split(TessAggState *state, AggSpill *spill, void *base, Size len, bool read_back)
{
	TessTableRef ref = {0};
	uint32		offsets[AGG_GROUP_ROWS];
	uint32		hashes[AGG_GROUP_ROWS];
	Size		from = TESS_TABLE_CHUNK_HEADER;
	/* In a register: the calls of the loop might change spill->records. */
	uint64	   *records = spill->records;

	spill->bases[AGG_SOURCE] = base;
	spill->lens[AGG_SOURCE] = len;
	ref.chunks = spill->bases;
	ref.chunk_lens = spill->lens;
	ref.nchunks = spill->npartitions + 2;
	for (;;)
	{
		int			count;
		int			full;

		check_table(state, read_back,
					state->kernels->table_split(&ref, state->nkeys,
												state->payload_size,
												spill->current, spill->npartitions,
												spill->shift, AGG_SOURCE, &from,
												AGG_GROUP_ROWS, offsets, hashes,
												&count, &full, &state->status));
		for (int index = 0; index < count; index++)
		{
			uint32		partition = agg_partition(spill, hashes[index]);

			records[partition]++;
			addHyperLogLog(&spill->parts[partition].groups, murmurhash32(hashes[index]));
		}
		if (full >= 0)
		{
			AggPart    *part = &spill->parts[full];
			int			slot = AGG_EMPTY + 1 + full;
			bool		tail = spill->bases[slot] != spill->empty;
			TessGroupAction action;

			action = group_step(state, spill, (TessGroupEvent) {
				.kind = TESS_GROUP_FULL,
				.partition = (uint32) full,
				.flag = tail,
			});
			if (action.kind == TESS_GROUP_ADD_CHUNK)
			{
				void	   *chunk = agg_new_chunk(state, spill);

				part_push(spill, part, chunk);
				spill->bases[slot] = chunk;
				spill->lens[slot] = spill->chunk_len;
			}
			else
			{
				Assert(action.kind == TESS_GROUP_FLUSH);
				agg_write_chunk(state, spill, full, spill->bases[slot]);
				spill->records[full] = 0;
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
	Size		old_size = old != NULL ? state->table.index_len : 0;

	for (int partition = 0; partition < spill->npartitions; partition++)
		nchunks += spill->parts[partition].nchunks;
	agg_reserve_chunks(state, nchunks);
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
	check(state, state->kernels->table_create(state->table.index, size, state->nkeys, payload_size,
											  capacity, &state->status));
	for (int chunk = AGG_EMPTY + 1; chunk < nchunks; chunk++)
	{
		Size		from = TESS_TABLE_CHUNK_HEADER;

		tess_loop_turn("group link");
		check(state, state->kernels->table_link(&state->table, chunk, &from, NULL,
												NULL, &state->status));
	}
	state->table_bytes += size;
	/* The old index is freed last: the highest moment, both alive. */
	note_memory_alive(state, old_size);
	if (old != NULL)
		pfree(old);
}

/*
 * After a batch: past seven eighths of hash_mem, as the machine decides,
 * or at once with force, the first level of partitions, for twice the
 * groups so far or the planner's if more, and the records so far split
 * into them.
 */
void
agg_spill_check(TessAggState *state, bool force)
{
	TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
	TessGroupAction start;
	AggSpill   *spill;
	int			nold;
	void	  **old;
	Size	   *old_lens;

	check(state, state->kernels->table_stats(&state->table, &stats, &state->status));
	start = group_step(state, NULL, (TessGroupEvent) {
		.kind = TESS_GROUP_INNER,
		.flag = force,
		.bytes = state->table_bytes,
		.records = stats.records,
		.estimate = (double) state->groups_estimate,
	});
	if (start.kind != TESS_GROUP_START)
		return;
	spill = agg_spill_create(state, NULL, &start.plan, &state->machine);
	state->spill = spill;
	nold = state->table.nchunks;
	old = palloc(sizeof(void *) * Max(nold, 1));
	old_lens = palloc(sizeof(Size) * Max(nold, 1));
	memcpy(old, state->chunk_bases, sizeof(void *) * nold);
	memcpy(old_lens, state->chunk_lens, sizeof(Size) * nold);
	for (int chunk = 0; chunk < nold; chunk++)
	{
		tess_loop_turn("group split");
		agg_split(state, spill, old[chunk], old_lens[chunk], false);
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
 * By the rule of the weights (docs/spill.md, "The weights"), as the
 * machine says: once the table, with its files' buffers, takes more than
 * seven eighths of hash_mem, the partition with the most bytes in memory
 * goes to disk whole, one that went before among them, and the next,
 * until the table takes half of hash_mem; the index is then made anew
 * over the rest. An eighth of hash_mem is left for a batch's new chunks
 * and index. Evicting down to the limit only made the index anew after
 * every partition: 5 M groups of a row each at a work_mem of 4 MB made it
 * 3598 times.
 */
static bool
agg_evict(TessAggState *state, Size extra)
{
	AggSpill   *spill = state->spill;
	TessGroupEvent event = {
		.kind = TESS_GROUP_GROWN,
		.memory = agg_spill_memory(state) + extra,
	};
	bool		evicted = false;

	for (;;)
	{
		TessGroupAction action = group_step(state, spill, event);
		int			largest = (int) action.partition;

		if (action.kind == TESS_GROUP_DONE)
			break;
		Assert(action.kind == TESS_GROUP_WRITE);
		for (int chunk = 0; chunk < spill->parts[largest].nchunks; chunk++)
		{
			tess_loop_turn("evict");
			agg_write_chunk(state, spill, largest, spill->parts[largest].chunks[chunk]);
		}
		part_release(spill, &spill->parts[largest]);
		state->evictions++;
		evicted = true;
		event = (TessGroupEvent) {
			.kind = TESS_GROUP_SENT,
			.memory = agg_spill_memory(state) + extra,
		};
	}
	return evicted;
}

/* The records of the partitions in memory, for their index. */
static uint64
agg_records(AggSpill *spill)
{
	uint64		records = 0;

	for (int partition = 0; partition < spill->npartitions; partition++)
		records += spill->records[partition];
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
		/* In a register: the calls of the loop might change spill->records. */
		uint64	   *records = spill->records;

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
			uint32		partition = agg_partition(spill, state->hashes[row]);

			records[partition]++;
			/* The partition's bits are the hash's low ones: mixed first. */
			addHyperLogLog(&spill->parts[partition].groups, murmurhash32(state->hashes[row]));
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
			/* The first level keeps its partitions' chunks in memory. */
			if (group_step(state, spill, (TessGroupEvent) {
				.kind = TESS_GROUP_FULL,
				.partition = (uint32) partition,
				.flag = spill->current[partition] != AGG_EMPTY,
			}).kind != TESS_GROUP_ADD_CHUNK)
				elog(ERROR, "TessAgg's spill wrote a chunk of the groups it reads");
			agg_reserve_chunks(state, chunk + 1);
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
 * With read_back, a chunk the kernel refuses was read back damaged.
 */
static void
agg_combine(TessAggState *state, AggSpill *spill, AggPart *part, void *base,
			Size len, int *dest, bool read_back)
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
			check_table(state, read_back,
						state->kernels->table_combine(&state->table, AGG_SOURCE,
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
		agg_reserve_chunks(state, state->table.nchunks + 1);
		*dest = state->table.nchunks++;
		state->chunk_bases[*dest] = agg_new_chunk(state, spill);
		state->chunk_lens[*dest] = spill->chunk_len;
		part_push(spill, part, state->chunk_bases[*dest]);
		state->table_bytes += spill->chunk_len;
		note_memory(state);
	}
	state->chunk_bases[AGG_SOURCE] = spill->source_empty;
	state->chunk_lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
}

/*
 * The groups a partition merges into: its estimate with a third more for
 * the estimate's error, at most its records.
 */
static uint64
part_groups(AggSpill *spill, int partition)
{
	AggPart    *part = &spill->parts[partition];
	double		groups = estimateHyperLogLog(&part->groups) * 4 / 3;

	return (uint64) Min(groups, (double) (spill->records[partition] + part->disk_records));
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
	uint64		capacity = first_capacity(part_groups(spill, partition));
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
		part_count(spill, part, -(int64) part->bytes);
		part->bytes = 0;
	}

	if (state->table.index != NULL)
		pfree(state->table.index);
	agg_reserve_chunks(state, part->nchunks + 2);
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
	/* The table is the partition's chunks and this index; combines add. */
	state->table_bytes = (Size) (nchunks - AGG_EMPTY - 1) * spill->chunk_len + size;
	check(state, state->kernels->table_create(state->table.index, size, state->nkeys, payload_size,
											  capacity, &state->status));
	for (int chunk = AGG_EMPTY + 1; chunk < nchunks; chunk++)
	{
		Size		from = TESS_TABLE_CHUNK_HEADER;

		tess_loop_turn("merge link");
		check(state, state->kernels->table_link(&state->table, chunk, &from, NULL,
												NULL, &state->status));
	}
	dest = nchunks - 1;
	for (int chunk = 0; chunk < nsplit; chunk++)
	{
		CHECK_FOR_INTERRUPTS();
		agg_combine(state, spill, part, split[chunk], spill->chunk_len, &dest, false);
		pfree(split[chunk]);
	}
	if (split != NULL)
		pfree(split);
	reader = tess_spill_open(spill->file, 0, partition);
	while (reader != NULL && tess_spill_read_header(reader, &header))
	{
		void	   *body;

		CHECK_FOR_INTERRUPTS();
		tess_spill_expect_kind(&header, TESS_SPILL_RECORDS);
		body = MemoryContextAllocExtended(spill->block_context, Max(header.len, 8),
										  MCXT_ALLOC_HUGE);
		tess_spill_read_body(reader, body, header.len);
		agg_combine(state, spill, part, body, header.len, &dest, true);
		MemoryContextReset(spill->block_context);
	}
	if (reader != NULL)
		tess_spill_close(reader);
	tess_spill_drop(spill->file, partition);
	note_memory(state);
}

/*
 * A level's input is read: before its partitions are given out, a
 * partition with records on disk writes its chunks in memory too, as the
 * machine says, since it merges from disk anyway; kept, they would narrow
 * the room every other partition merges in, and a partition that does
 * not fit splits, writing all its records again. Partitions wholly in
 * memory stay.
 */
static void
agg_input_end(TessAggState *state, AggSpill *spill)
{
	TessGroupAction action;

	for (action = group_step(state, spill, (TessGroupEvent) {.kind = TESS_GROUP_INPUT_END});
		 action.kind == TESS_GROUP_WRITE;
		 action = group_step(state, spill, (TessGroupEvent) {.kind = TESS_GROUP_NEXT}))
	{
		AggPart    *part = &spill->parts[action.partition];

		for (int chunk = 0; chunk < part->nchunks; chunk++)
		{
			tess_loop_turn("flush");
			agg_write_chunk(state, spill, (int) action.partition, part->chunks[chunk]);
		}
		part_release(spill, part);
	}
	tess_spill_finish(spill->file);
}

/*
 * A partition too large to merge splits by the next bits of the hash
 * into the level below its machine asked for: its chunks read back, then
 * those in memory, each split into the new level's partitions, which keep
 * a chunk each in memory and write the others. The new level is given
 * out next.
 */
static void
agg_split_level(TessAggState *state, AggSpill *spill, int partition,
				const TessGroupBelow *below)
{
	AggPart    *part = &spill->parts[partition];
	TessGroupMachine machine;
	TessGroupAction start;
	AggSpill   *level;
	TessSpillReader *reader;
	TessSpillHeader header;

	check(state, state->kernels->group_below(&machine, &state->spill_params, below, &start,
											 &state->status));
	level = agg_spill_create(state, spill, &start.plan, &machine);
	reader = tess_spill_open(spill->file, 0, partition);
	state->splits++;
	while (reader != NULL && tess_spill_read_header(reader, &header))
	{
		void	   *body;

		CHECK_FOR_INTERRUPTS();
		tess_spill_expect_kind(&header, TESS_SPILL_RECORDS);
		body = MemoryContextAllocExtended(level->block_context, Max(header.len, 8),
										  MCXT_ALLOC_HUGE);
		tess_spill_read_body(reader, body, header.len);
		agg_split(state, level, body, header.len, true);
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
	agg_input_end(state, level);
	state->spill = level;
	note_memory(state);
}

/*
 * The next partition to give out, merged into a table, as the machine
 * says: after the input, the partitions of the first level in turn, those
 * wholly in memory first, so that a partition read back from disk merges
 * with the most room; a level below given out whole where one split, and
 * then the level above again. False once every group is out.
 */
bool
agg_advance(TessAggState *state)
{
	for (;;)
	{
		AggSpill   *spill = state->spill;
		TessGroupAction action;
		int			partition;

		CHECK_FOR_INTERRUPTS();

		if (spill->given)
			part_release(spill, &spill->parts[spill->partition]);
		spill->given = false;
		action = group_step(state, spill, (TessGroupEvent) {.kind = TESS_GROUP_NEXT});
		if (action.kind == TESS_GROUP_LEVEL_DONE)
		{
			if (spill->parent == NULL)
				return false;
			state->spill = spill->parent;
			agg_level_free(spill);
			continue;
		}
		partition = (int) action.partition;
		spill->partition = partition;
		if (action.kind == TESS_GROUP_WEIGH)
		{
			/*
			 * What the partition takes merged: a record per group and its
			 * index, and a block read back, next to the chunks every level
			 * keeps. A group written many times merges into one record, so
			 * the groups decide, not the file.
			 */
			uint64		others = 0;

			for (AggSpill *level = spill; level != NULL; level = level->parent)
				for (int other = 0; other < level->npartitions; other++)
					if (level != spill || other != partition)
						others += level->parts[other].bytes;
			action = group_step(state, spill, (TessGroupEvent) {
				.kind = TESS_GROUP_WEIGHED,
				.groups = part_groups(spill, partition),
				.others = others,
			});
			if (action.kind == TESS_GROUP_SPLIT)
			{
				agg_split_level(state, spill, partition, &action.below);
				continue;
			}
		}
		Assert(action.kind == TESS_GROUP_MERGE);
		agg_merge(state, spill, partition);
		spill->given = true;
		return true;
	}
}

/* The input is done: the partitions are given out one by one. */
void
agg_finish_input(TessAggState *state)
{
	AggSpill   *spill = state->spill;

	spill->partition = -1;
	agg_input_end(state, spill);
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
	/* The pairs written: the next one's number. */
	uint32		pairs;
} RowWriter;

/* A level of partitions: one set of files, its partitions by its plan's bits. */
typedef struct RowSpill
{
	TessSpill  *file;
	int			level;
	RowWriter	writers[ROWS_PARTS];
	/* Reading: the next partition to read. */
	int			next;
	/* The level plan's bits: its own from shift, a level below's from below. */
	uint32		shift;
	uint32		below;
	bool		deeper;
} RowSpill;

/* A partition being read back: the chunk in hand and its next row. */
typedef struct RowReader
{
	TessSpillReader *file;
	TessColumnsReader columns;
	uint32		next;
	/* The pairs loaded: the next one's number. */
	uint32		pairs;
	/* The batch given to group_batch: a window of the chunk. */
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

/*
 * A level of partitions by the hash bits from shift, their files not made
 * until written: by the level plan, ROWS_PARTS partitions, each keeping a
 * chunk of ROWS_BLOCK_ROWS rows and a chunk of their values.
 */
RowSpill *
rows_spill_create(TessAggState *state, int level, uint32 shift)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	TessSpillConfig config = TESS_STRUCT_INITIALIZER(TessSpillConfig);
	RowSpill   *spill = MemoryContextAllocZero(context, sizeof(RowSpill));
	int			null_lanes = tess_spill_columns_null_lanes(rows_words(state));
	Size		rows_len = TESS_SPILL_COLUMNS_HEADER +
		sizeof(uint64) * ROWS_BLOCK_ROWS * (null_lanes + rows_words(state));
	TessSpillShape shape = {
		.reserve = rows_len + ROWS_BLOCK_VALUES,
		.share = 1,
		.min_chunk = rows_len,
		.max_chunk = rows_len,
		.min_partitions = ROWS_PARTS,
		.max_partitions = ROWS_PARTS,
	};
	TessSpillLevel at = {
		.limit = tess_hash_memory_limit(),
		.shift = shift,
	};
	TessSpillPlan plan;
	Size		chunk_len;

	check(state, state->kernels->spill_plan(&shape, &at, &plan, &state->status));
	chunk_len = plan.chunk_len;

	config.parent_context = context;
	config.kernels = state->kernels;
	config.npartitions = plan.partitions;
	config.level = (uint32) level;
	config.fingerprint = (uint64) rows_words(state);
	config.max_chunk = chunk_len;
	config.buffer_len = TESS_SPILL_BUFFER_LEN(tess_hash_memory_limit());
	spill->file = tess_spill_create(&config);
	spill->level = level;
	spill->shift = plan.shift;
	spill->below = plan.below;
	spill->deeper = plan.deeper;
	for (int part = 0; part < ROWS_PARTS; part++)
	{
		RowWriter  *writer = &spill->writers[part];

		writer->chunk_len = chunk_len;
		writer->chunk = MemoryContextAlloc(context, writer->chunk_len);
		writer->values_len = ROWS_BLOCK_VALUES;
		writer->values = MemoryContextAlloc(context, writer->values_len);
		rows_writer_reset(state, writer);
	}
	state->partitions += ROWS_PARTS;
	return spill;
}

/*
 * Write a partition's chunk as its pair, numbered as the pairs before it:
 * its values, unless it has none, then its columns.
 */
static void
rows_flush(TessAggState *state, RowSpill *spill, int part)
{
	RowWriter  *writer = &spill->writers[part];

	if (writer->rows == 0)
		return;
	tess_spill_columns_set_rows(writer->chunk, writer->rows);
	if (writer->values_used > 0)
		state->disk_bytes += tess_spill_write(spill->file, part, TESS_SPILL_VALUES,
											  writer->pairs, writer->values,
											  writer->values_used, NULL);
	state->disk_bytes += tess_spill_write(spill->file, part, TESS_SPILL_COLUMNS, writer->pairs,
										  writer->chunk, writer->chunk_len, NULL);
	writer->pairs++;
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
	int			row = -1;

	/*
	 * By the values' hashes, mixed, when a key has a dictionary: the numbers
	 * are one table's. The partitions take the low bits, the bits above for
	 * each level, which the buckets of the table of groups, taken from the
	 * high bits, leave free.
	 */
	const uint32 *hashes = state->has_dicts ? state->value_hashes : state->hashes;
	uint32		shift = spill->shift;

	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		int			part = (int) tess_table_partition(hashes[row], shift, ROWS_PARTS);
		RowWriter  *writer = &spill->writers[part];
		Size		need = 0;
		uint64	   *nulls;
		uint64	   *words;
		ExpandedObjectHeader *expanded;

		for (int column = 0; column < state->ncomputed; column++)
			if (!state->computed_byvals[column] && !state->computed_columns[column].isnull[row])
				need += MAXALIGN(tess_value_size(state->computed_columns[column].values[row],
												 state->computed_lens[column], &expanded));
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
				Size		size = tess_value_size(from->values[row],
												   state->computed_lens[column], &expanded);

				tess_value_copy(from->values[row], expanded,
								writer->values + writer->values_used, size);
				lane[0] = TESS_SPILL_VALUE_REF(0, writer->values_used);
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
	uint32		take;

	while (reader->next >= reader->columns.rows)
	{
		CHECK_FOR_INTERRUPTS();
		if (reader->file == NULL ||
			!tess_columns_reader_load(&reader->columns, reader->file, reader->pairs, -1))
			return NULL;
		reader->pairs++;
		reader->next = 0;
	}
	take = Min(reader->columns.rows - reader->next, 64);
	tess_columns_reader_window(&reader->columns, reader->next, take, reader->column_values,
							   reader->column_isnull);
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
			reader->columns.rows = 0;
			reader->next = 0;
			reader->pairs = 0;
			/* The level below takes the next bits, the last level none. */
			state->rows_level = spill->level + 1;
			state->rows_shift = spill->below;
			state->rows_last = !spill->deeper;
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

	tess_columns_reader_init(&reader->columns, state->kernels, context, rows_words(state),
							 state->ncomputed, state->computed_byvals, state->computed_lens);
	reader->column_values = MemoryContextAlloc(context, sizeof(Datum *) * state->ncomputed);
	reader->column_isnull = MemoryContextAlloc(context, sizeof(bool *) * state->ncomputed);
	for (int column = 0; column < state->ncomputed; column++)
	{
		reader->column_values[column] = MemoryContextAllocZero(context, sizeof(Datum) * 64);
		reader->column_isnull[column] = MemoryContextAllocZero(context, sizeof(bool) * 64);
	}
	reader->batch.abi_version = TESS_BATCH_ABI_VERSION;
	reader->batch.struct_size = sizeof(TessBatch);
	reader->batch.values_stored = true;
	reader->batch.ops = &reader_batch_ops;
	reader->batch.private_data = state;
	reader->batch.table_oid = InvalidOid;
	state->reader = reader;
}


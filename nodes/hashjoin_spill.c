#include "postgres.h"

#include "miscadmin.h"
#include "utils/datum.h"
#include "utils/expandeddatum.h"
#include "utils/memutils.h"
#include "storage/barrier.h"
#include "utils/dsa.h"

#include "tessera/runtime.h"

#include "internal.h"
#include "hashjoin.h"

static void spill_get_column(TessBatch *batch, int column, const TessRowMask *rows,
							 TessColumnPurpose purpose, TessDatumColumn *result);

static const TessBatchOps spill_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = spill_get_column,
};

/*
 * Spilling (docs/spill.md). The table fits until the bytes it takes pass
 * hash_mem; then its records go into partitions and the build goes on
 * partitioned. Each side keeps its partitions' records in chunks of the
 * table's format and their by-reference values in value chunks; a chunk
 * written to disk goes after the value chunks opened since the last one,
 * so that a partition's file, read in order, gives a chunk's values
 * before its records.
 */

static void split_table(TessHashJoinState *state);
static Size spill_room(TessHashJoinState *state);

void
join_part_open(PartReader *reader, TessSpill *file, int partition, int writers)
{
	reader->file = file;
	reader->partition = partition;
	reader->writers = writers;
	reader->next = 0;
	reader->reader = NULL;
	reader->open = true;
}

/* The next block's header, from the next writer's file once one ends. */
static bool
part_header(PartReader *reader, TessSpillHeader *header)
{
	if (!reader->open)
		return false;
	for (;;)
	{
		/* Every block read back from disk passes here: a block at a time. */
		CHECK_FOR_INTERRUPTS();
		if (reader->reader == NULL)
		{
			if (reader->next >= reader->writers)
			{
				reader->open = false;
				return false;
			}
			reader->reader = tess_spill_open(reader->file, reader->next++,
											 reader->partition);
			continue;
		}
		if (tess_spill_read_header(reader->reader, header))
			return true;
		tess_spill_close(reader->reader);
		reader->reader = NULL;
	}
}

static void
part_body(PartReader *reader, void *body, Size len)
{
	tess_spill_read_body(reader->reader, body, len);
}

static void
part_close(PartReader *reader)
{
	if (reader->reader != NULL)
		tess_spill_close(reader->reader);
	reader->reader = NULL;
	reader->open = false;
}

static void
grow_ints(MemoryContext context, int **array, int *slots, int needed)
{
	int			grown = Max(*slots, 8);

	if (needed <= *slots)
		return;
	while (grown < needed)
		grown *= 2;
	*array = *array == NULL ?
		MemoryContextAlloc(context, sizeof(int) * grown) :
		repalloc(*array, sizeof(int) * grown);
	*slots = grown;
}

/* The partition of a hash on a side, which may have fewer partitions than its level. */
static inline uint32
side_partition(const SpillSide *side, const JoinSpill *spill, uint32 hash)
{
	return tess_table_partition(hash, spill->shift, (uint32) side->npartitions);
}

static inline uint64
chunk_used(const void *base)
{
	return *(const uint64 *) base;
}

/* Whether a chunk of the side holds a row. */
static inline bool
side_chunk_rows(const SpillSide *side, const void *base)
{
	return side->columnar ? tess_spill_columns_rows(base) > 0 :
		chunk_used(base) > TESS_TABLE_CHUNK_HEADER;
}

/* Make the len bytes at base an empty chunk of the side: of records, or of columns. */
static void
side_chunk_init(TessHashJoinState *state, SpillSide *side, void *base, Size len)
{
	Size		capacity;

	if (side->columnar)
		check(state, state->kernels->spill_columns_init(base, len, side->nwords,
														&capacity, &state->status));
	else
		check(state, state->kernels->table_chunk_init(base, len, &state->status));
}

static void
side_sync(SpillSide *side)
{
	side->ref.chunks = side->bases;
	side->ref.chunk_lens = side->lens;
	side->ref.nchunks = side->nchunks;
}

/*
 * A side of nkeys keys of the kinds and a payload of a word of NULL bits
 * and nwords words of the types, with no chunk yet but the empty one and
 * an index for the layout only.
 */
static void
side_init(TessHashJoinState *state, SpillSide *side, int nkeys, const TessTableKeyKind *kinds, int nwords,
		  const int16 *typlens, const bool *byvals, Size chunk_len,
		  bool resident, int npartitions)
{
	JoinSpill  *spill = state->spill;
	TessSpillConfig config = TESS_STRUCT_INITIALIZER(TessSpillConfig);
	void	   *empty;
	Size		empty_len;

	/*
	 * Small blocks: a chunk past a few kB gets a block of its own, of its
	 * size, not a share of a block twice as large, and memory is what the
	 * chunks take.
	 */
	if (resident)
		side->context = AllocSetContextCreate(spill->context,
											  "TessHashJoin inner partitions",
											  ALLOCSET_SMALL_SIZES);
	else
		side->context = AllocSetContextCreate(spill->context,
											  "TessHashJoin outer partitions",
											  ALLOCSET_SMALL_SIZES);
	side->columnar = !resident;
	side->nkeys = nkeys;
	memcpy(side->kinds, kinds, sizeof(TessTableKeyKind) * nkeys);
	side->nwords = nwords;
	side->typlens = MemoryContextAlloc(side->context, sizeof(int16) * Max(nwords, 1));
	side->byvals = MemoryContextAlloc(side->context, sizeof(bool) * Max(nwords, 1));
	if (nwords > 0)
	{
		memcpy(side->typlens, typlens, sizeof(int16) * nwords);
		memcpy(side->byvals, byvals, sizeof(bool) * nwords);
	}
	side->payload_size = sizeof(uint64) * (1 + nwords);
	check(state, state->kernels->table_record_size(nkeys, side->payload_size,
												   &side->record_size, &state->status));
	side->chunk_len = chunk_len;
	check(state, state->kernels->table_size(nkeys, kinds, side->payload_size,
											JOIN_INITIAL_ROWS,
											&side->ref.index_len, &state->status));
	side->ref.index = MemoryContextAllocZero(side->context, side->ref.index_len);
	check(state, state->kernels->table_create(side->ref.index, side->ref.index_len,
											  nkeys, kinds, side->payload_size,
											  JOIN_INITIAL_ROWS, &state->status));
	side->slots = 16;
	side->bases = MemoryContextAlloc(side->context, sizeof(void *) * side->slots);
	side->lens = MemoryContextAlloc(side->context, sizeof(Size) * side->slots);
	side->pointers = MemoryContextAllocZero(side->context, sizeof(dsa_pointer) * side->slots);
	empty_len = side->columnar ? TESS_SPILL_COLUMNS_HEADER : TESS_TABLE_CHUNK_HEADER;
	empty = MemoryContextAlloc(side->context, empty_len);
	side_chunk_init(state, side, empty, empty_len);
	side->bases[0] = empty;
	side->lens[0] = empty_len;
	side->nchunks = 1;
	side_sync(side);
	side->npartitions = npartitions;
	side->current = MemoryContextAllocZero(side->context,
										   sizeof(uint32) * npartitions);
	side->parts = MemoryContextAllocZero(side->context,
										 sizeof(SpillPart) * npartitions);
	side->rows = MemoryContextAllocZero(side->context, sizeof(uint64) * npartitions);
	for (int partition = 0; partition < npartitions; partition++)
	{
		side->parts[partition].value_current = -1;
		side->parts[partition].resident = resident;
	}
	side->queue = MemoryContextAlloc(side->context, sizeof(int) * npartitions);
	check(state, state->kernels->table_fingerprint(&side->ref, &side->fingerprint,
												   &state->status));
	config.parent_context = side->context;
	config.kernels = state->kernels;
	config.npartitions = npartitions;
	config.level = spill->level;
	config.fingerprint = side->fingerprint;
	config.max_len = (uint64) MaxAllocHugeSize;
	config.buffer_len = TESS_SPILL_BUFFER_LEN(get_hash_memory_limit());
	side->file = tess_spill_create(&config);
}

/*
 * Count bytes of a shared side's chunks in the words of the table's
 * spilling, of a partition: whether they pass the budget is kept.
 */
void
join_side_count(SpillSide *side, int partition, int64 delta)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	if (side->area == NULL)
		return;
	if (side->kernels->table_spill_add_bytes(side->spill_words, side->spill_nwords,
											 delta, partition, &side->over,
											 &status) != TESS_OK)
		tess_status_report(&status);
}

/* The bytes a chunk of len bytes takes: with its header when shared. */
static inline int64
side_block(SpillSide *side, Size len)
{
	return (int64) (side->area != NULL ? JOIN_CHUNK_HEADER + len : len);
}

/* What a chunk of records of len bytes costs a shared side's budget, its index and filter included. */
static inline int64
side_chunk_cost(SpillSide *side, Size len)
{
	return record_chunk_cost(len, side->record_size);
}

/* A block of len bytes: in the query's shared memory after a header, or the side's own. */
static void *
side_alloc(SpillSide *side, Size len, dsa_pointer *pointer)
{
	JoinChunk  *header;

	if (side->area == NULL)
	{
		*pointer = InvalidDsaPointer;
		return MemoryContextAllocExtended(side->context, len, MCXT_ALLOC_HUGE);
	}
	*pointer = dsa_allocate_extended(side->area, JOIN_CHUNK_HEADER + len, DSA_ALLOC_HUGE);
	header = dsa_get_address(side->area, *pointer);
	header->next = InvalidDsaPointer;
	header->number = 0;
	header->len = len;
	header->owner = side->owner;
	return (char *) header + JOIN_CHUNK_HEADER;
}

static void
side_free(SpillSide *side, void *base, dsa_pointer pointer)
{
	if (DsaPointerIsValid(pointer))
		dsa_free(side->area, pointer);
	else
		pfree(base);
}

/* Free chunk `index` of a partition, and count it. */
static void
side_free_chunk(SpillSide *side, int partition, int index)
{
	side_free(side, side->bases[index], side->pointers[index]);
	side->bases[index] = NULL;
	side->pointers[index] = InvalidDsaPointer;
	side->bytes -= side->lens[index];
	side->parts[partition].bytes -= side->lens[index];
	join_side_count(side, partition, -side_chunk_cost(side, side->lens[index]));
}

/* Free value chunk `number` of a partition, and count it. */
static void
side_free_values(SpillSide *side, int partition, int number)
{
	side_free(side, side->value_bases[number], side->value_pointers[number]);
	side->value_bases[number] = NULL;
	side->value_pointers[number] = InvalidDsaPointer;
	side->bytes -= side->value_allocated[number];
	side->parts[partition].bytes -= side->value_allocated[number];
	join_side_count(side, partition, -side_block(side, side->value_allocated[number]));
}

/* Room for value chunk `number`, a number of the side or of every participant. */
static void
side_value_slot(SpillSide *side, int number)
{
	int			slots = Max(side->value_slots, 16);

	if (number < side->value_slots)
		return;
	while (slots <= number)
		slots *= 2;
	side->value_bases = side->value_bases == NULL ?
		MemoryContextAllocZero(side->context, sizeof(char *) * slots) :
		repalloc0(side->value_bases, sizeof(char *) * side->value_slots,
				  sizeof(char *) * slots);
	side->value_pointers = side->value_pointers == NULL ?
		MemoryContextAllocZero(side->context, sizeof(dsa_pointer) * slots) :
		repalloc0(side->value_pointers, sizeof(dsa_pointer) * side->value_slots,
				  sizeof(dsa_pointer) * slots);
	side->value_lens = side->value_lens == NULL ?
		MemoryContextAllocZero(side->context, sizeof(Size) * slots) :
		repalloc0(side->value_lens, sizeof(Size) * side->value_slots,
				  sizeof(Size) * slots);
	side->value_allocated = side->value_allocated == NULL ?
		MemoryContextAllocZero(side->context, sizeof(Size) * slots) :
		repalloc0(side->value_allocated, sizeof(Size) * side->value_slots,
				  sizeof(Size) * slots);
	side->value_slots = slots;
}

/* A new chunk that the partition appends to from now on; its index. */
static int
side_add_chunk(TessHashJoinState *state, SpillSide *side, int partition)
{
	SpillPart  *part = &side->parts[partition];
	int			index = side->nchunks;
	void	   *base;

	if (index == TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin hash table cannot hold more than %d chunks",
						TESS_TABLE_MAX_CHUNKS)));
	if (index == side->slots)
	{
		side->pointers = repalloc0(side->pointers, sizeof(dsa_pointer) * side->slots,
								   sizeof(dsa_pointer) * side->slots * 2);
		side->slots *= 2;
		side->bases = repalloc(side->bases, sizeof(void *) * side->slots);
		side->lens = repalloc(side->lens, sizeof(Size) * side->slots);
	}
	base = side_alloc(side, side->chunk_len, &side->pointers[index]);
	side_chunk_init(state, side, base, side->chunk_len);
	side->bases[index] = base;
	side->lens[index] = side->chunk_len;
	side->nchunks++;
	side_sync(side);
	grow_ints(side->context, &part->chunks, &part->chunk_slots, part->nchunks + 1);
	part->chunks[part->nchunks++] = index;
	side->current[partition] = index;
	part->bytes += side->chunk_len;
	side->bytes += side->chunk_len;
	join_side_count(side, partition, side_chunk_cost(side, side->chunk_len));
	if (side == &state->spill->build)
		state->counters[JOIN_CHUNKS]++;
	return index;
}

/* Drop the chunks whose base is gone and number the others anew. */
void
join_side_compact(SpillSide *side)
{
	int		   *map = palloc(sizeof(int) * side->nchunks);
	int			kept = 1;

	map[0] = 0;
	for (int index = 1; index < side->nchunks; index++)
	{
		if (side->bases[index] == NULL)
		{
			map[index] = 0;
			continue;
		}
		map[index] = kept;
		side->bases[kept] = side->bases[index];
		side->lens[kept] = side->lens[index];
		side->pointers[kept] = side->pointers[index];
		kept++;
	}
	for (int partition = 0; partition < side->npartitions; partition++)
	{
		SpillPart  *part = &side->parts[partition];
		int			count = 0;

		side->current[partition] = map[side->current[partition]];
		for (int chunk = 0; chunk < part->nchunks; chunk++)
			if (map[part->chunks[chunk]] != 0)
				part->chunks[count++] = map[part->chunks[chunk]];
		part->nchunks = count;
	}
	side->nchunks = kept;
	side_sync(side);
	pfree(map);
}

static void
write_block(TessHashJoinState *state, SpillSide *side, int partition,
			TessSpillKind kind, uint32 number, const void *body, Size len)
{
	/* disk_bytes: what the block takes read back; the disk counts what was stored. */
	state->counters[JOIN_DISK] += tess_spill_write(side->file, partition, kind, number,
												   body, len, NULL);
	side->parts[partition].written = true;
	side->parts[partition].disk_bytes += len;
	if (kind != TESS_SPILL_VALUES)
		side->parts[partition].blocks++;
	state->counters[JOIN_SPILLED]++;
}

/* Write the partition's value chunks in memory and free them. */
static void
side_write_values(TessHashJoinState *state, SpillSide *side, int partition)
{
	SpillPart  *part = &side->parts[partition];

	for (int index = 0; index < part->nvalues; index++)
	{
		int			number = part->values[index];

		write_block(state, side, partition, TESS_SPILL_VALUES, number,
					side->value_bases[number], side->value_lens[number]);
		side_free_values(side, partition, number);
	}
	part->nvalues = 0;
	part->value_current = -1;
	part->value_bytes = 0;
}

/* Write chunk `index` of the partition, unless it holds no row. */
static void
side_write_records(TessHashJoinState *state, SpillSide *side, int partition,
				   int index)
{
	if (!side_chunk_rows(side, side->bases[index]))
		return;
	if (side->columnar)
		write_block(state, side, partition, TESS_SPILL_COLUMNS,
					side->next_number++, side->bases[index], side->lens[index]);
	else
		write_block(state, side, partition, TESS_SPILL_RECORDS,
					side->next_number++, side->bases[index],
					chunk_used(side->bases[index]));
}

/*
 * A partition on disk writes its values and its tail, which then takes
 * the next records.
 */
void
join_side_flush(TessHashJoinState *state, SpillSide *side, int partition)
{
	SpillPart  *part = &side->parts[partition];
	int			index = side->current[partition];

	Assert(!part->resident);
	side_write_values(state, side, partition);
	part->queued = false;
	if (index == 0)
		return;
	side_write_records(state, side, partition, index);
	side_chunk_init(state, side, side->bases[index], side->lens[index]);
}

/*
 * A resident partition goes to disk: its values, then every chunk; the
 * last chunk stays as its tail, emptied.
 */
void
join_side_demote(TessHashJoinState *state, SpillSide *side, int partition)
{
	SpillPart  *part = &side->parts[partition];
	int			tail = side->current[partition];

	part->resident = false;
	side_write_values(state, side, partition);
	for (int chunk = 0; chunk < part->nchunks; chunk++)
	{
		int			index = part->chunks[chunk];

		side_write_records(state, side, partition, index);
		if (index == tail)
			continue;
		side_free_chunk(side, partition, index);
	}
	if (tail != 0)
		side_chunk_init(state, side, side->bases[tail], side->lens[tail]);
	join_side_compact(side);
}

/*
 * A partition on disk writes its values and its tail and frees them, as
 * its level starts joining: the tails of every partition kept in memory
 * would leave a partition, or a level below, little of hash_mem, and a
 * small hash_mem split partitions thousands of times.
 */
static void
side_evict(TessHashJoinState *state, SpillSide *side, int partition)
{
	int			index = side->current[partition];

	if (side->parts[partition].resident)
		return;
	side_write_values(state, side, partition);
	side->parts[partition].queued = false;
	if (index == 0)
		return;
	side_write_records(state, side, partition, index);
	side_free_chunk(side, partition, index);
	side->current[partition] = 0;
}

/* A value chunk of len bytes for the partition; its number. */
static int
side_value_chunk(SpillSide *side, int partition, Size len)
{
	SpillPart  *part = &side->parts[partition];
	int			number = side->nvalues;

	/* A shared table's value chunks take numbers every participant shares. */
	if (side->area != NULL)
	{
		SpinLockAcquire(&side->shared->lock);
		number = (int) side->shared->next_value_chunk++;
		SpinLockRelease(&side->shared->lock);
	}
	if (number >= INT_MAX - 1)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin cannot hold more chunks of values")));
	side_value_slot(side, number);
	side->value_bases[number] = side_alloc(side, len, &side->value_pointers[number]);
	if (side->area != NULL)
		((JoinChunk *) (side->value_bases[number] - JOIN_CHUNK_HEADER))->number = number;
	side->value_lens[number] = 0;
	side->value_allocated[number] = len;
	side->nvalues = Max(side->nvalues, number + 1);
	join_side_count(side, partition, side_block(side, len));
	grow_ints(side->context, &part->values, &part->value_slots, part->nvalues + 1);
	part->values[part->nvalues++] = number;
	part->value_bytes += len;
	part->bytes += len;
	side->bytes += len;
	return number;
}

/*
 * Copy a by-reference value into the partition's value chunks, as
 * join_store_value does into the table's, and return its reference. A
 * partition on disk whose values outgrow a chunk is queued to be written.
 */
static uint64
side_store(SpillSide *side, int partition, Datum value, int16 typlen)
{
	SpillPart  *part = &side->parts[partition];
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
	if (aligned > side->chunk_len / 4)
	{
		number = side_value_chunk(side, partition, aligned);
		byte = 0;
		side->value_lens[number] = aligned;
	}
	else
	{
		if (part->value_current < 0 ||
			part->value_used + aligned > part->value_len)
		{
			part->value_current = side_value_chunk(side, partition, side->chunk_len);
			part->value_len = side->chunk_len;
			part->value_used = 0;
		}
		number = part->value_current;
		byte = part->value_used;
		part->value_used += aligned;
		side->value_lens[number] = part->value_used;
	}
	if (expanded != NULL)
		EOH_flatten_into(expanded, side->value_bases[number] + byte, size);
	else
		memcpy(side->value_bases[number] + byte, DatumGetPointer(value), size);
	if (!part->resident && !part->queued && part->value_bytes > side->chunk_len)
	{
		part->queued = true;
		side->queue[side->nqueue++] = partition;
	}
	return JOIN_VALUE_REF(number, byte);
}

/* Write the partitions whose values outgrew a chunk. */
static void
side_flush_queue(TessHashJoinState *state, SpillSide *side)
{
	for (int index = 0; index < side->nqueue; index++)
		if (side->parts[side->queue[index]].queued)
			join_side_flush(state, side, side->queue[index]);
	side->nqueue = 0;
}

/*
 * The bytes spilling takes in memory, with 8 bytes of index per resident
 * row; the resident rows into *resident unless NULL.
 */
Size
join_spill_memory(JoinSpill *spill, uint64 *resident)
{
	uint64		rows = 0;
	Size		bytes = 0;

	for (int partition = 0; partition < spill->npartitions; partition++)
		if (spill->build.parts[partition].resident)
			rows += spill->build.rows[partition];
	if (resident != NULL)
		*resident = rows;
	/*
	 * The levels above keep the tails of their partitions to come; every
	 * set keeps its write buffer while it writes, and its readers a block.
	 */
	for (JoinSpill *level = spill; level != NULL; level = level->parent)
	{
		bytes += level->build.bytes + level->probe.bytes +
			sizeof(uint64) * level->bloom_words +
			MemoryContextMemAllocated(level->part_context, true) +
			MemoryContextMemAllocated(level->block_context, true) +
			tess_spill_memory(level->build.file) + tess_spill_memory(level->probe.file);
	}
	return bytes + (spill->indexed ? 0 : rows * sizeof(uint64));
}

/*
 * Keep spilling within hash_mem while building: while it takes more, the
 * largest resident partition goes to disk. Room stays for the outer
 * side's tails of the partitions on disk, which come once the probing
 * starts: a chunk of records, one of values when the outer side keeps a
 * by-reference column, and a file's buffer each; a resident partition
 * writes no outer row. The tails stay: the chunk size bounds them, and
 * writing them sooner would write chunks of a few rows.
 */
static void
make_room(TessHashJoinState *state, bool building)
{
	JoinSpill  *spill = state->spill;
	Size		limit = get_hash_memory_limit();
	Size		tail = spill->probe.chunk_len + BLCKSZ;
	int			on_disk = 0;

	for (int word = 0; word < spill->probe.nwords; word++)
		if (!spill->probe.byvals[word])
		{
			tail += spill->probe.chunk_len;
			break;
		}
	for (int partition = 0; partition < spill->npartitions; partition++)
		if (!spill->build.parts[partition].resident)
			on_disk++;
	/* What the node reports, so that what it keeps is what it says. */
	while (building && join_memory(state) + on_disk * tail > limit)
	{
		int			largest = -1;
		Size		bytes = 0;

		for (int partition = 0; partition < spill->npartitions; partition++)
		{
			SpillPart  *part = &spill->build.parts[partition];

			if (part->resident && part->bytes > bytes)
			{
				largest = partition;
				bytes = part->bytes;
			}
		}
		if (largest < 0)
			break;
		join_side_demote(state, &spill->build, largest);
		on_disk++;
	}
	/*
	 * Too little left resident to be worth probing, as join_finish_spill_build
	 * decides: all of it goes now, not after the build.
	 */
	if (building && on_disk > 0)
	{
		uint64		resident = 0;

		for (int partition = 0; partition < spill->npartitions; partition++)
			if (spill->build.parts[partition].resident)
				resident += spill->build.rows[partition];
		if (resident > 0 && resident * 4 < spill->total_rows)
			for (int partition = 0; partition < spill->npartitions; partition++)
				if (spill->build.parts[partition].resident)
					join_side_demote(state, &spill->build, partition);
	}
	join_note_memory(state);
}

/*
 * A level of partitions by the hash bits from shift, for an inner side
 * of expected bytes, as the current one: the power of two of them that
 * makes each about half of hash_mem, as long as the bits last and each
 * partition's tails on both sides fit in half of hash_mem.
 */
JoinSpill *
join_spill_create(TessHashJoinState *state, JoinSpill *parent, double expected,
			 uint32 shift, int forced)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	Size		limit = get_hash_memory_limit();
	Size		record = state->record_size;
	Size		chunk_len;
	uint32		count;
	int			npartitions;
	JoinSpill  *spill;
	int16	   *typlens;
	bool	   *byvals;
	int			nchild = 0;
	int			nstored = 0;

	/* A level below has what the levels above leave, a quarter at least. */
	if (parent != NULL)
	{
		Size		used = join_spill_memory(parent, NULL);

		limit = used < limit / 4 * 3 ? limit - used : limit / 4;
	}

	/* Each partition's reserve on both sides: half of hash_mem bounds them all. */
	check(state, state->kernels->spill_partitions(expected, limit, JOIN_SPILL_RESERVE, shift,
												  JOIN_SPILL_MIN_PARTITIONS,
												  JOIN_SPILL_MAX_PARTITIONS, 0, &count,
												  &state->status));
	/* A shared table's partitions, which every participant took. */
	npartitions = forced > 0 ? forced : (int) count;
	/* A chunk a sixteenth of hash_mem among them, of four records at least. */
	check(state, state->kernels->spill_chunk_len(limit, (uint32) npartitions, 16,
												 Max(JOIN_SPILL_MIN_CHUNK,
													 TESS_TABLE_CHUNK_HEADER + 4 * record),
												 JOIN_CHUNK_LEN, &chunk_len, &state->status));

	spill = MemoryContextAllocZero(context, sizeof(JoinSpill));
	state->spill = spill;
	spill->parent = parent;
	spill->level = parent == NULL ? 0 : parent->level + 1;
	spill->context = AllocSetContextCreate(context, "TessHashJoin spill",
										   ALLOCSET_DEFAULT_SIZES);
	spill->part_context = AllocSetContextCreate(spill->context,
												"TessHashJoin partition",
												ALLOCSET_SMALL_SIZES);
	spill->block_context = AllocSetContextCreate(spill->context,
												 "TessHashJoin outer block",
												 ALLOCSET_SMALL_SIZES);
	spill->npartitions = npartitions;
	spill->shift = shift;
	spill->partition = -1;
	/* A level's own files; a shared table's first level reads every participant's. */
	spill->writers = 1;
	state->counters[JOIN_BATCHES] = Max(state->counters[JOIN_BATCHES],
										(uint64) npartitions);

	/* The inner side: the table's own payload. */
	typlens = palloc(sizeof(int16) * Max(state->npayload, 1));
	byvals = palloc(sizeof(bool) * Max(state->npayload, 1));
	for (int word = 0; word < state->npayload; word++)
	{
		typlens[word] = state->typlens[state->payload_columns[word]];
		byvals[word] = state->typbyvals[state->payload_columns[word]];
	}
	side_init(state, &spill->build, state->keys.nkeys, state->keys.inner_kinds, state->npayload, typlens,
			  byvals, chunk_len, true, npartitions);

	/*
	 * The outer side: the columns of the outer child the node reads, the
	 * keys and the columns asked for, each once.
	 */
	for (int key = 0; key < state->keys.nkeys; key++)
		nchild = Max(nchild, state->keys.outer_keys[key] + 1);
	for (int index = 0; index < state->compact.nouter; index++)
		nchild = Max(nchild, state->child_columns[state->compact.outer_columns[index]] + 1);
	spill->nchild = nchild;
	spill->word_of = MemoryContextAllocZero(spill->context, sizeof(int) * Max(nchild, 1));
	spill->stored = MemoryContextAlloc(spill->context,
									   sizeof(int) * (state->keys.nkeys + state->compact.nouter));
	typlens = repalloc(typlens, sizeof(int16) * (state->keys.nkeys + state->compact.nouter));
	byvals = repalloc(byvals, sizeof(bool) * (state->keys.nkeys + state->compact.nouter));
	for (int index = 0; index < state->compact.nouter; index++)
	{
		int			scan = state->compact.outer_columns[index];
		int			child = state->child_columns[scan];

		if (spill->word_of[child] != 0)
			continue;
		spill->stored[nstored] = child;
		typlens[nstored] = state->typlens[scan];
		byvals[nstored] = state->typbyvals[scan];
		spill->word_of[child] = ++nstored;
	}
	for (int key = 0; key < state->keys.nkeys; key++)
	{
		int			child = state->keys.outer_keys[key];

		if (spill->word_of[child] != 0)
			continue;
		spill->stored[nstored] = child;
		typlens[nstored] = sizeof(Datum);
		byvals[nstored] = true;
		spill->word_of[child] = ++nstored;
	}
	if (nstored > 64)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin cannot spill more than 64 outer columns")));
	side_init(state, &spill->probe, state->keys.nkeys, state->keys.outer_kinds, nstored, typlens, byvals,
			  chunk_len, false, npartitions);
	/* The outer rows a shared table answers: one partition of their own. */
	if (state->parallel.shared != NULL && parent == NULL)
		side_init(state, &spill->resident, state->keys.nkeys, state->keys.outer_kinds, nstored,
				  typlens, byvals, chunk_len, false, 1);
	spill->rows = &spill->probe;
	pfree(typlens);
	pfree(byvals);
	spill->build_children = MemoryContextAlloc(spill->context,
											   sizeof(int) * Max(state->npayload, 1));
	for (int word = 0; word < state->npayload; word++)
		spill->build_children[word] = state->child_columns[state->payload_columns[word]];
	spill->columns = MemoryContextAlloc(spill->context,
										sizeof(TessDatumColumn) *
										Max(Max(state->npayload, nstored), 1));
	spill->values = MemoryContextAlloc(spill->context, sizeof(Datum *) * Max(nstored, 1));
	spill->window = MemoryContextAllocZero(spill->context, sizeof(Datum *) * Max(nstored, 1));
	spill->isnull = MemoryContextAlloc(spill->context, sizeof(bool *) * Max(nstored, 1));
	for (int word = 0; word < nstored; word++)
	{
		spill->values[word] = MemoryContextAllocZero(spill->context,
													 sizeof(Datum) * JOIN_COMPACT_ROWS);
		spill->isnull[word] = MemoryContextAllocZero(spill->context,
													 sizeof(bool) * JOIN_COMPACT_ROWS);
	}
	spill->batch = (TessBatch) {
		TESS_ABI_INITIALIZER(TESS_BATCH_ABI_VERSION, TessBatch),
	};
	spill->batch.table_oid = InvalidOid;
	spill->batch.ops = &spill_batch_ops;
	spill->batch.private_data = spill;
	spill->batch.rows.bits = spill->bits;
	return spill;
}

/*
 * The table outgrew hash_mem: the first level of partitions, for twice
 * what was read, or what the planner expects if more; a Bloom filter of
 * every inner row, and the table built so far moved into the partitions.
 */
void
join_start_spill(TessHashJoinState *state)
{
	double		bytes = (double) state->table_bytes + state->values.bytes;
	double		expected = bytes * 2;
	JoinSpill  *spill;
	uint64		expected_rows;

	if (state->build_rows > 0)
		expected = Max(expected, bytes / state->build_rows * state->inner_rows);
	spill = join_spill_create(state, NULL, expected, 0, 0);

	/* A filter of every inner row, sized for the rows expected. */
	state->counters[JOIN_BLOOM_FILTERS]++;
	expected_rows = Max((uint64) state->inner_rows, state->build_rows * 2);
	check(state, state->kernels->table_bloom_words_within(Max(expected_rows, 1),
														  get_hash_memory_limit(),
														  &spill->bloom_words,
														  &state->status));
	spill->bloom = MemoryContextAllocExtended(spill->context,
											  mul_size(sizeof(uint64), spill->bloom_words),
											  MCXT_ALLOC_HUGE | MCXT_ALLOC_ZERO);
	split_table(state);
}

/* Room for the appends of a batch of the node's capacity, of either side. */
static void
spill_reserve(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	if (spill->payload_rows >= state->probe.capacity)
		return;
	if (spill->before_bits != NULL)
	{
		pfree(spill->before_bits);
		pfree(spill->payloads);
	}
	spill->before_bits = MemoryContextAlloc(spill->context,
											sizeof(uint64) *
											tess_row_mask_word_count(state->probe.capacity));
	spill->payloads = MemoryContextAlloc(spill->context,
										 sizeof(uint8 *) * state->probe.capacity);
	spill->payload_rows = state->probe.capacity;
}

/*
 * The table built before the first spill, into the partitions: its
 * chunks one by one, each split by the kernel into the partitions'
 * chunks, the by-reference values of its records copied into the value
 * chunks of their partitions, and then freed.
 */
/*
 * Split the len bytes at base, a chunk of records of the inner side's
 * layout, into the current level's partitions: each record copied by the
 * kernel to its partition's chunk, its by-reference values, found through
 * values (the bases of the value chunks it refers to), copied into its
 * partition's value chunks, and its hash into the level's Bloom filter.
 */
void
join_split_chunk(TessHashJoinState *state, void *base, Size len, char *const *values)
{
	JoinSpill  *spill = state->spill;
	SpillSide  *side = &spill->build;
	uint32		offsets[JOIN_COMPACT_ROWS];
	uint32		hashes[JOIN_COMPACT_ROWS];
	uint8	   *payloads[JOIN_COMPACT_ROWS];
	bool		byref = false;
	Size		from = TESS_TABLE_CHUNK_HEADER;
	int			source;

	for (int word = 0; word < side->nwords; word++)
		byref |= !side->byvals[word];
	/* The chunk joins the side's for the call, as a chunk of no partition. */
	if (side->nchunks == side->slots)
	{
		side->pointers = repalloc0(side->pointers, sizeof(dsa_pointer) * side->slots,
								   sizeof(dsa_pointer) * side->slots * 2);
		side->slots *= 2;
		side->bases = repalloc(side->bases, sizeof(void *) * side->slots);
		side->lens = repalloc(side->lens, sizeof(Size) * side->slots);
	}
	source = side->nchunks++;
	side->bases[source] = base;
	side->lens[source] = len;
	side->pointers[source] = InvalidDsaPointer;
	side_sync(side);
	for (;;)
	{
		int			count;
		int			full;
		uint64		bits;

		check(state, state->kernels->table_split(&side->ref, side->nkeys,
												 side->kinds, side->payload_size,
												 side->current, spill->npartitions,
												 spill->shift, source, &from,
												 JOIN_COMPACT_ROWS, offsets, hashes,
												 &count, &full, &state->status));
		bits = count == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << count) - 1;
		if (count > 0 && spill->bloom != NULL)
			check(state, (spill->shared ? state->kernels->bloom_shared_add :
						  state->kernels->bloom_add) (spill->bloom, spill->bloom_words,
													  hashes,
													  &(TessRowMask) {count, &bits},
													  &state->status));
		/* The payloads of the records just split, in one call. */
		if (byref && count > 0)
			check(state, state->kernels->table_payloads(&side->ref, offsets,
														&(TessRowMask) {count, &bits},
														payloads, &state->status));
		for (int index = 0; index < count; index++)
		{
			int			partition = spill_partition(spill, hashes[index]);

			side->rows[partition]++;
			if (!byref)
				continue;
			for (int word = 0; word < side->nwords; word++)
			{
				uint64	   *slot = (uint64 *) payloads[index] + 1 + word;
				char	   *value;

				if (side->byvals[word] || *slot == 0)
					continue;
				if (values[(*slot >> 32) - 1] == NULL)
					elog(ERROR, "TessHashJoin split a record whose values it does not have");
				value = values[(*slot >> 32) - 1] + (*slot & 0xFFFFFFFF);
				*slot = side_store(side, partition, PointerGetDatum(value),
								   side->typlens[word]);
			}
		}
		if (full >= 0)
		{
			if (side->parts[full].resident || side->current[full] == 0)
				side_add_chunk(state, side, full);
			else
				join_side_flush(state, side, full);
		}
		else if (count == 0)
			break;
	}
	/* The chunk leaves the side; its memory is the caller's. */
	side->bases[source] = NULL;
	join_side_compact(side);
}

/*
 * Move the table built so far into the partitions of the first level,
 * its by-reference values into their value chunks, and free it.
 */
static void
split_table(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	for (int chunk = 0; chunk < state->table.nchunks; chunk++)
	{
		join_split_chunk(state, state->chunk_bases[chunk], state->chunk_lens[chunk],
					state->values.bases);
		pfree(state->chunk_bases[chunk]);
	}
	spill->total_rows = state->build_rows;
	MemoryContextReset(state->table_context);
	/* The by-reference values moved into the partitions' value chunks. */
	MemoryContextReset(state->values_context);
	join_reset_values(state);
	if (state->values.bases != NULL)
		pfree(state->values.bases);
	state->values.bases = NULL;
	state->values.slots = 0;
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->table_bytes = 0;
	make_room(state, true);
}

/*
 * Make room for the rows still pending: a partition whose chunk is full,
 * or that has none, gets a new one while resident or without a tail, and
 * writes its tail otherwise.
 */
static void
make_chunks(TessHashJoinState *state, SpillSide *side, const TessRowMask *pending,
			const uint32 *hashes)
{
	JoinSpill  *spill = state->spill;
	int			row = -1;

	spill->stamp++;
	while ((row = tess_row_mask_next(pending, row)) >= 0)
	{
		int			partition = side_partition(side, spill, hashes[row]);
		SpillPart  *part = &side->parts[partition];

		if (part->stamp == spill->stamp)
			continue;
		part->stamp = spill->stamp;
		if (part->resident || side->current[partition] == 0)
			side_add_chunk(state, side, partition);
		else
			join_side_flush(state, side, partition);
	}
}

/*
 * join_side_append of the outer side: the rows of pending into the chunks of
 * columns of their partitions, the words of columns (by-reference ones
 * as pointers first); a by-reference value is then copied into its
 * partition's value chunks, as a record's is, and its word refers to the
 * copy. Rows go on as the partitions' chunks fill; pending ends empty.
 */
static void
column_append(TessHashJoinState *state, SpillSide *side, TessRowMask *pending,
			  const TessDatumColumn *columns, bool byref)
{
	JoinSpill  *spill = state->spill;
	int			nwords = tess_row_mask_word_count(pending->nrows);
	TessRowMask before = {pending->nrows, spill->before_bits};

	for (;;)
	{
		if (byref)
			memcpy(spill->before_bits, pending->bits, sizeof(uint64) * nwords);
		check(state, state->kernels->spill_columns_append_partitioned((void *const *) side->bases,
																	  side->lens, side->nchunks,
																	  side->current,
																	  side->npartitions,
																	  spill->shift,
																	  state->probe.hashes,
																	  side->nwords, columns,
																	  pending, state->probe.offsets,
																	  side->rows,
																	  &state->status));
		if (byref)
		{
			int			row = -1;

			for (int word = 0; word < nwords; word++)
				spill->before_bits[word] &= ~pending->bits[word];
			while ((row = tess_row_mask_next(&before, row)) >= 0)
			{
				uint32		place = state->probe.offsets[row];
				void	   *base = side->bases[place >> TESS_SPILL_COLUMNS_PLACE_BITS];
				int			partition = side_partition(side, spill, state->probe.hashes[row]);

				place &= (1u << TESS_SPILL_COLUMNS_PLACE_BITS) - 1;
				for (int word = 0; word < side->nwords; word++)
				{
					if (side->byvals[word] || columns[word].isnull[row])
						continue;
					tess_spill_columns_lane(base, 1 + word)[place] =
						side_store(side, partition, columns[word].values[row],
								   side->typlens[word]);
				}
			}
		}
		if (tess_row_mask_count(pending) == 0)
			break;
		make_chunks(state, side, pending, state->probe.hashes);
	}
	side_flush_queue(state, side);
}

/*
 * Append the rows of pending, hashed and keyed by join_batch_keys, as records
 * of the side's partitions: the payload is the NULL bits and a word per
 * column of the batch at children, which the kernel takes as columns,
 * counting the rows of each partition; a by-reference value is copied
 * into its partition's value chunks right after its row went in, its
 * word then referring to the copy, so that the values of a chunk's rows
 * are written with it or before it, never with the chunk before. Rows go
 * on as the partitions' chunks fill; pending ends empty. With nulls, the
 * NULL bits of the columns are kept there.
 */
void
join_side_append(TessHashJoinState *state, SpillSide *side, TessBatch *batch,
			TessRowMask *pending, const int *children, uint64 *nulls)
{
	JoinSpill  *spill = state->spill;
	int			nwords = tess_row_mask_word_count(pending->nrows);
	TessRowMask before = {pending->nrows, NULL};
	TessDatumColumn *columns = spill->columns;
	uint64		seen = 0;
	bool		byref = false;

	for (int word = 0; word < side->nwords; word++)
		byref |= !side->byvals[word];
	spill_reserve(state);
	before.bits = spill->before_bits;
	for (int word = 0; word < side->nwords; word++)
		join_child_column(batch, children[word], pending, TESS_COLUMN_FOR_PROJECTION,
					 &columns[word]);
	if (side->columnar)
	{
		column_append(state, side, pending, columns, byref);
		return;
	}
	for (;;)
	{
		if (byref)
			memcpy(spill->before_bits, pending->bits, sizeof(uint64) * nwords);
		check(state, state->kernels->table_append_partitioned_columns(&side->ref, side->current,
																	  side->npartitions,
																	  spill->shift,
																	  state->probe.hashes, state->keys.nkeys,
																	  state->keys.table_keys,
																	  side->nwords, columns,
																	  pending, state->probe.offsets,
																	  side->rows, &seen,
																	  &state->status));
		/* The by-reference values of the rows just appended, found through their payloads in one call. */
		if (byref)
		{
			int			row = -1;

			for (int word = 0; word < nwords; word++)
				spill->before_bits[word] &= ~pending->bits[word];
			if (tess_row_mask_count(&before) > 0)
				check(state, state->kernels->table_payloads(&side->ref, state->probe.offsets, &before,
															spill->payloads, &state->status));
			while ((row = tess_row_mask_next(&before, row)) >= 0)
			{
				int			partition = side_partition(side, spill, state->probe.hashes[row]);

				for (int word = 0; word < side->nwords; word++)
				{
					if (side->byvals[word] || columns[word].isnull[row])
						continue;
					((uint64 *) spill->payloads[row])[1 + word] =
						side_store(side, partition, columns[word].values[row],
								   side->typlens[word]);
				}
			}
		}
		if (tess_row_mask_count(pending) == 0)
			break;
		make_chunks(state, side, pending, state->probe.hashes);
	}
	if (nulls != NULL)
		*nulls |= seen;
	side_flush_queue(state, side);
}

/* Append the rows of an inner batch to the partitions. */
void
join_insert_spill(TessHashJoinState *state, TessBatch *batch)
{
	JoinSpill  *spill = state->spill;
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask valid;
	TessRowMask pending;
	int			count;

	join_reserve_rows(state, nrows);
	memset(state->probe.valid_bits, 0, sizeof(uint64) * nwords);
	valid = (TessRowMask) {nrows, state->probe.valid_bits};
	pending = (TessRowMask) {nrows, state->probe.pending_bits};
	join_batch_keys(state, batch, state->keys.inner_keys, state->keys.inner_kinds, &valid);
	count = tess_row_mask_count(&valid);
	if (count == 0)
		return;
	check(state, (spill->shared ? state->kernels->bloom_shared_add :
				  state->kernels->bloom_add) (spill->bloom, spill->bloom_words,
											  state->probe.hashes, &valid, &state->status));
	memcpy(state->probe.pending_bits, state->probe.valid_bits, sizeof(uint64) * nwords);
	join_side_append(state, &spill->build, batch, &pending, spill->build_children,
				&state->null_columns);
	state->build_rows += count;
	spill->total_rows += count;
	state->counters[JOIN_BUILD_ROWS] += count;
	/* A shared table's partitions go to disk as every participant decides. */
	if (spill->shared)
		state->parallel.appended += count;
	else
		make_room(state, true);
}

/*
 * The build is over: the resident partitions' records make the table the
 * outer batches probe now, the others wait on disk and in their tails.
 */
void
join_finish_spill_build(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;
	SpillSide  *side = &spill->build;
	int			nchunks = 0;
	uint64		rows = 0;
	uint64		resident = 0;

	/*
	 * Resident partitions holding less than a quarter of the inner rows go
	 * to disk too: probing them would cost every outer batch the whole
	 * probe for the few rows of theirs, more than writing them saves.
	 */
	for (int partition = 0; partition < spill->npartitions; partition++)
		if (side->parts[partition].resident)
			resident += side->rows[partition];
	if (resident > 0 && resident * 4 < spill->total_rows)
		for (int partition = 0; partition < spill->npartitions; partition++)
			if (side->parts[partition].resident)
				join_side_demote(state, side, partition);

	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		SpillPart  *part = &side->parts[partition];

		if (!part->resident)
			continue;
		state->counters[JOIN_RESIDENT]++;
		rows += side->rows[partition];
		join_reserve_chunks(state, nchunks + part->nchunks);
		for (int chunk = 0; chunk < part->nchunks; chunk++)
		{
			state->chunk_bases[nchunks] = side->bases[part->chunks[chunk]];
			state->chunk_lens[nchunks] = side->lens[part->chunks[chunk]];
			nchunks++;
		}
	}
	join_reserve_chunks(state, Max(nchunks, 1));
	if (spill->parent == NULL)
		spill->input_rows = spill->total_rows;
	state->table.index = NULL;
	state->table.nchunks = nchunks;
	/* A level's own resident table, with its tail still to come. */
	join_forget_marks(state);
	/* The chunks count with the side's memory; the index with the table's. */
	state->table_bytes = 0;
	state->build_rows = rows;
	state->values.bases = side->value_bases;
	state->values.nchunks = side->nvalues;
	spill->indexed = true;
	if (spill->parent == NULL)
		join_index_table(state);
	else
	{
		uint64		buckets = state->counters[JOIN_BUCKETS];

		/* The buckets count for the build, not for a level below. */
		join_index_table(state);
		state->counters[JOIN_BUCKETS] = buckets;
	}
}

/* Free a partition's chunks and value chunks in memory of one side. */
void
join_side_forget(SpillSide *side, int partition)
{
	SpillPart  *part = &side->parts[partition];

	for (int chunk = 0; chunk < part->nchunks; chunk++)
	{
		int			index = part->chunks[chunk];

		if (side->bases[index] != NULL && index != 0)
			side_free_chunk(side, partition, index);
	}
	part->nchunks = 0;
	side->current[partition] = 0;
	for (int index = 0; index < part->nvalues; index++)
	{
		int			number = part->values[index];

		if (side->value_bases[number] != NULL)
			side_free_values(side, partition, number);
	}
	part->nvalues = 0;
	part->value_current = -1;
	part->bytes = 0;
}

/* Free a partition's tail and value chunks of one side, and its files. */
static void
side_release(SpillSide *side, int partition)
{
	join_side_forget(side, partition);
	if (!side->shared_files)
		tess_spill_drop(side->file, partition);
}

/*
 * The outer child is done, and so are the resident partitions: their
 * memory goes, and the files become readable.
 */
static void
start_joining(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	spill->joining = true;
	spill->partition = -1;
	/* Every outer row is written or answered: the filter's work is done. */
	if (spill->bloom != NULL)
		pfree(spill->bloom);
	spill->bloom = NULL;
	spill->bloom_words = 0;
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		if (spill->build.parts[partition].resident)
			side_release(&spill->build, partition);
		else
			side_evict(state, &spill->build, partition);
		side_evict(state, &spill->probe, partition);
	}
	join_side_compact(&spill->build);
	join_side_compact(&spill->probe);
	MemoryContextReset(state->table_context);
	state->table.index = NULL;
	state->table.nchunks = 0;
	join_forget_marks(state);
	state->bloom.bits = NULL;
	state->bloom.nwords = 0;
	tess_spill_finish(spill->build.file);
	tess_spill_finish(spill->probe.file);
}

/* Forget the joined partition: its files, tails and what was read back. */
static void
end_partition(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;
	int			partition = spill->partition;

	if (partition < 0 || partition >= spill->npartitions)
		return;
	if (state->parallel.round_partition >= 0)
		join_round_leave(state);
	part_close(&spill->reader);
	part_close(&spill->build_reader);
	if (spill->carried != NULL)
	{
		pfree(spill->carried);
		spill->carried = NULL;
	}
	if (spill->matched_rows != NULL)
	{
		pfree(spill->matched_rows);
		spill->matched_rows = NULL;
	}
	spill->multipass = false;
	spill->final_pass = false;
	for (int index = 0; index < spill->nloaded; index++)
		spill->build.value_bases[spill->loaded_values[index]] = NULL;
	spill->nloaded = 0;
	for (int index = 0; index < spill->nblock_values; index++)
		spill->probe.value_bases[spill->block_values[index]] = NULL;
	spill->nblock_values = 0;
	side_release(&spill->build, partition);
	side_release(&spill->probe, partition);
	MemoryContextReset(spill->part_context);
	MemoryContextReset(spill->block_context);
	spill->block = NULL;
	MemoryContextReset(state->table_context);
	state->table.index = NULL;
	state->table.nchunks = 0;
	join_forget_marks(state);
}

/* A chunk of the joined partition's table, read back or its tail. */
static void
add_loaded_chunk(TessHashJoinState *state, void *base, Size len)
{
	int			chunk = state->table.nchunks;

	if (chunk == TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin hash table cannot hold more than %d chunks",
						TESS_TABLE_MAX_CHUNKS)));
	join_reserve_chunks(state, chunk + 1);
	state->chunk_bases[chunk] = base;
	state->chunk_lens[chunk] = len;
	state->table.nchunks++;
}

/*
 * The bytes a piece of a partition's table may take: what hash_mem leaves
 * besides the rest of spilling and a chunk of outer rows with its values,
 * two thirds of it, the rest for the index.
 */
static Size
spill_room(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;
	Size		limit = get_hash_memory_limit();
	Size		used = join_spill_memory(spill, NULL) + 2 * spill->probe.chunk_len;

	return used >= limit ? 0 : (limit - used) / 3 * 2;
}

/* Forget the table of a piece, and the values read for it. */
static void
drop_piece(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	for (int index = 0; index < spill->nloaded; index++)
		spill->build.value_bases[spill->loaded_values[index]] = NULL;
	spill->nloaded = 0;
	MemoryContextReset(spill->part_context);
	MemoryContextReset(state->table_context);
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->table_bytes = 0;
	state->build_rows = 0;
	state->duplicates = 0;
	join_forget_marks(state);
}

/* A value chunk read back into the piece's table. */
static void
add_loaded_values(JoinSpill *spill, uint32 number, void *body)
{
	/* Another participant's number may be past this one's. */
	if (number >= INT_MAX - 1)
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("TessHashJoin read back a value chunk it never wrote"));
	side_value_slot(&spill->build, (int) number);
	spill->build.nvalues = Max(spill->build.nvalues, (int) number + 1);
	spill->build.value_bases[number] = body;
	grow_ints(spill->context, &spill->loaded_values, &spill->loaded_slots,
			  spill->nloaded + 1);
	spill->loaded_values[spill->nloaded++] = number;
}

/*
 * The joined partition's next piece: its blocks read back, as many whole
 * groups as room allows (all of them when it does), and once the file is
 * done its tail, indexed as a table of its own.
 */

static void
load_piece(TessHashJoinState *state, int partition, bool whole)
{
	JoinSpill  *spill = state->spill;
	SpillSide  *side = &spill->build;
	SpillPart  *part = &side->parts[partition];
	TessSpillHeader header;
	uint64		buckets = state->counters[JOIN_BUCKETS];
	Size		record = side->record_size;
	Size		loaded = 0;
	bool		records = false;
	uint64		rows = 0;
	Size		room;
	bool		byref = false;

	for (int word = 0; word < side->nwords; word++)
		byref |= !side->byvals[word];
	drop_piece(state);
	room = whole ? SIZE_MAX : spill_room(state);
	if (spill->carried != NULL)
	{
		void	   *body = MemoryContextAllocExtended(spill->part_context,
													  Max(spill->carried_header.len, 8),
													  MCXT_ALLOC_HUGE);

		memcpy(body, spill->carried, spill->carried_header.len);
		pfree(spill->carried);
		spill->carried = NULL;
		loaded += spill->carried_header.len;
		if (spill->carried_header.kind == TESS_SPILL_VALUES)
			add_loaded_values(spill, spill->carried_header.number, body);
		else
		{
			add_loaded_chunk(state, body, spill->carried_header.len);
			rows += (spill->carried_header.len - TESS_TABLE_CHUNK_HEADER) / record;
			records = true;
		}
	}
	while (part_header(&spill->build_reader, &header))
	{
		void	   *body;

		/*
		 * A group ends where values follow records, or, without values, at
		 * every chunk: the piece may end there.
		 */
		if (records && loaded >= room &&
			(header.kind == TESS_SPILL_VALUES || !byref))
		{
			spill->carried = MemoryContextAllocExtended(spill->context,
														Max(header.len, 8),
														MCXT_ALLOC_HUGE);
			spill->carried_header = header;
			part_body(&spill->build_reader, spill->carried, header.len);
			break;
		}
		body = MemoryContextAllocExtended(spill->part_context, Max(header.len, 8),
										  MCXT_ALLOC_HUGE);
		part_body(&spill->build_reader, body, header.len);
		loaded += header.len;
		if (header.kind == TESS_SPILL_VALUES)
		{
			add_loaded_values(spill, header.number, body);
			records = false;
			continue;
		}
		add_loaded_chunk(state, body, header.len);
		rows += (header.len - TESS_TABLE_CHUNK_HEADER) / record;
		records = true;
	}
	if (spill->carried == NULL)
	{
		/* The file is done: the tail goes with the last piece. */
		part_close(&spill->build_reader);
		spill->pieces_done = true;
		for (int chunk = 0; chunk < part->nchunks; chunk++)
		{
			int			index = part->chunks[chunk];
			Size		used = chunk_used(side->bases[index]);

			if (used > TESS_TABLE_CHUNK_HEADER)
			{
				add_loaded_chunk(state, side->bases[index], side->lens[index]);
				rows += (used - TESS_TABLE_CHUNK_HEADER) / record;
				state->counters[JOIN_TAILS]++;
			}
		}
	}
	state->build_rows = rows;
	state->duplicates = 0;
	state->bloom.bits = NULL;
	state->bloom.nwords = 0;
	/* The rows written passed the Bloom filter: no other one pays. */
	state->bloom.decided = true;
	state->values.bases = side->value_bases;
	state->values.nchunks = side->nvalues;
	join_index_table(state);
	/* The buckets count for the build, not for each partition. */
	state->counters[JOIN_BUCKETS] = buckets;
	/* Duplicates the resident table did not have: the pairs go compact. */
	if (!state->compact.on && state->duplicates > 0)
		join_decide_compact(state);
}

/* Read the joined partition's outer rows from their first. */
static void
open_outer_rows(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	join_part_open(&spill->reader, spill->probe.file, spill->partition, spill->writers);
	spill->tail_read = false;
	spill->block = NULL;
	spill->ordinal = 0;
}

/*
 * The next pass over the joined partition's outer rows: with its next
 * piece, then, for a left or anti join, the last one without a table.
 * False when the partition is done.
 */
bool
join_next_pass(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	/* A round's outer rows: the next file this participant takes. */
	if (state->parallel.round_partition >= 0)
		return join_round_next_outer(state);
	if (!spill->multipass)
		return false;
	if (!spill->pieces_done)
		load_piece(state, spill->partition, false);
	else if ((state->jointype == JOIN_LEFT || state->jointype == JOIN_ANTI) &&
			 !spill->final_pass)
	{
		drop_piece(state);
		spill->final_pass = true;
	}
	else
		return false;
	state->counters[JOIN_PASSES]++;
	open_outer_rows(state);
	join_note_memory(state);
	return true;
}

/*
 * Split partition `partition` of the current level into a level below:
 * its inner rows read back group by group (value chunks, then the chunks
 * of records that refer to them), and then its tail, each chunk split by
 * the hash bits above the level's into the new level's partitions, which
 * start resident and go to disk as memory runs short, as the first
 * level's did. The new level then probes with the partition's outer
 * rows.
 */
static void
start_level(TessHashJoinState *state, int partition)
{
	JoinSpill  *parent = state->spill;
	SpillSide  *from = &parent->build;
	SpillPart  *part = &from->parts[partition];
	TessSpillHeader header;
	JoinSpill  *spill;
	bool		records = false;

	spill = join_spill_create(state, parent, (double) part->disk_bytes + part->bytes,
						 parent->shift + pg_leftmost_one_pos32(parent->npartitions), 0);
	spill->input_rows = from->rows[partition];
	spill->total_rows = from->rows[partition];
	state->counters[JOIN_SPLITS]++;
	state->counters[JOIN_BATCHES] = Max(state->counters[JOIN_BATCHES],
										(uint64) spill->npartitions);
	while (part_header(&parent->build_reader, &header))
	{
		void	   *body;

		if (header.kind == TESS_SPILL_VALUES)
		{
			/* A new group: the values of the one before are done. */
			if (records)
			{
				for (int index = 0; index < parent->nloaded; index++)
					from->value_bases[parent->loaded_values[index]] = NULL;
				parent->nloaded = 0;
				MemoryContextReset(spill->part_context);
				records = false;
			}
			body = MemoryContextAllocExtended(spill->part_context, Max(header.len, 8),
											  MCXT_ALLOC_HUGE);
			part_body(&parent->build_reader, body, header.len);
			add_loaded_values(parent, header.number, body);
			continue;
		}
		body = MemoryContextAllocExtended(spill->block_context, Max(header.len, 8),
										  MCXT_ALLOC_HUGE);
		part_body(&parent->build_reader, body, header.len);
		join_split_chunk(state, body, header.len, from->value_bases);
		MemoryContextReset(spill->block_context);
		records = true;
		make_room(state, true);
	}
	part_close(&parent->build_reader);
	for (int chunk = 0; chunk < part->nchunks; chunk++)
	{
		int			index = part->chunks[chunk];

		if (chunk_used(from->bases[index]) > TESS_TABLE_CHUNK_HEADER)
			join_split_chunk(state, from->bases[index], from->lens[index], from->value_bases);
	}
	for (int index = 0; index < parent->nloaded; index++)
		from->value_bases[parent->loaded_values[index]] = NULL;
	parent->nloaded = 0;
	MemoryContextReset(spill->part_context);
	side_release(from, partition);
	make_room(state, true);
	join_finish_spill_build(state);
}

/* Delete a level's files and free its memory. */
static void
level_free(JoinSpill *spill)
{
	SpillSide  *build = &spill->build;

	part_close(&spill->reader);
	part_close(&spill->build_reader);
	/* A shared side's chunks still its own go back to the shared memory. */
	if (build->area != NULL)
	{
		for (int index = 0; index < build->nchunks; index++)
			if (DsaPointerIsValid(build->pointers[index]))
				dsa_free(build->area, build->pointers[index]);
		for (int number = 0; number < build->value_slots; number++)
			if (DsaPointerIsValid(build->value_pointers[number]))
				dsa_free(build->area, build->value_pointers[number]);
	}
	tess_spill_release(spill->build.file);
	tess_spill_release(spill->probe.file);
	if (spill->resident.file != NULL)
		tess_spill_release(spill->resident.file);
	MemoryContextDelete(spill->context);
	pfree(spill);
}

/* A level is done: the level whose partition it split goes on. */
static void
pop_level(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	state->spill = spill->parent;
	level_free(spill);
	MemoryContextReset(state->table_context);
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->build_rows = 0;
	state->duplicates = 0;
}

/*
 * Load a partition on disk to join: split into a level below when too
 * large and no single key, in pieces when still too large, whole
 * otherwise; its outer rows read from their first.
 */
bool
join_open_partition(TessHashJoinState *state, int partition)
{
	JoinSpill  *spill = state->spill;

	spill->partition = partition;
	join_part_open(&spill->build_reader, spill->build.file, partition, spill->writers);
	spill->pieces_done = false;
	if (spill->build.parts[partition].disk_bytes > spill_room(state) &&
		spill->shift + pg_leftmost_one_pos32(spill->npartitions) + 2 <= 32 &&
		spill->build.rows[partition] < spill->input_rows / 10 * 9)
	{
		/*
		 * Too large, and much smaller than what this level split, so not
		 * one key: it splits by the next bits into a level below, whose
		 * outer rows are the partition's.
		 */
		open_outer_rows(state);
		start_level(state, partition);
		join_note_memory(state);
		return true;
	}
	/* The file larger than the room: pieces, and passes over the outer rows. */
	spill->multipass = spill->build.parts[partition].disk_bytes > spill_room(state);
	if (spill->multipass && state->jointype != JOIN_INNER)
	{
		spill->matched_words = Max((spill->probe.rows[partition] + 63) / 64, 1);
		spill->matched_rows =
			MemoryContextAllocExtended(spill->context,
									   sizeof(uint64) * spill->matched_words,
									   MCXT_ALLOC_HUGE | MCXT_ALLOC_ZERO);
	}
	load_piece(state, partition, !spill->multipass);
	open_outer_rows(state);
	join_note_memory(state);
	return true;
}

/*
 * The next partition to join, with its table loaded: one with outer rows
 * written; the others are forgotten. False when none is left.
 */
static bool
next_partition(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	end_partition(state);
	if (spill->shared)
		return join_shared_next_partition(state);
	for (int partition = spill->partition + 1; partition < spill->npartitions; partition++)
	{
		if (spill->build.parts[partition].resident)
			continue;
		/* RIGHT and FULL return a partition's inner rows without outer ones too. */
		if (spill->probe.rows[partition] == 0 &&
			(!state->preserve_inner || spill->build.rows[partition] == 0))
		{
			side_release(&spill->build, partition);
			side_release(&spill->probe, partition);
			continue;
		}
		return join_open_partition(state, partition);
	}
	spill->partition = spill->npartitions;
	return false;
}

/*
 * The next batch of the joined partition's outer rows (or of a shared
 * table's outer rows it answers itself, spill->rows): up to
 * JOIN_COMPACT_ROWS records of the chunk being read, each stored column
 * gathered from their payload; the chunks come from the file, each after
 * its values, and then the tail. False when none is left.
 */
bool
join_next_spilled(TessHashJoinState *state, JoinSpill *spill)
{
	SpillSide  *side = spill->rows;

	for (;;)
	{
		TessSpillHeader header;

		if (spill->block != NULL)
		{
			uint64		total = tess_spill_columns_rows(spill->block);

			if (spill->cursor < total)
			{
				int			count = (int) Min(total - spill->cursor, JOIN_COMPACT_ROWS);
				const uint64 *nulls = tess_spill_columns_lane(spill->block, 0) + spill->cursor;
				uint64		any = 0;

				for (int row = 0; row < count; row++)
					any |= nulls[row];
				spill->bits[0] = count == 64 ? ~UINT64CONST(0) :
					(UINT64CONST(1) << count) - 1;
				spill->batch.rows.nrows = count;
				spill->batch_ordinal = spill->ordinal;
				spill->ordinal += count;
				/* A by-value word is read where it lies; a reference becomes a pointer. */
				for (int word = 0; word < side->nwords; word++)
				{
					uint64	   *lane = tess_spill_columns_lane(spill->block, 1 + word) +
						spill->cursor;
					bool	   *isnull = spill->isnull[word];

					if ((any >> word) & 1)
						for (int row = 0; row < count; row++)
							isnull[row] = (nulls[row] >> word) & 1;
					else
						memset(isnull, 0, sizeof(bool) * count);
					if (side->byvals[word])
					{
						spill->window[word] = (Datum *) lane;
						continue;
					}
					spill->window[word] = spill->values[word];
					for (int row = 0; row < count; row++)
					{
						uint64		ref = lane[row];

						if (isnull[row])
						{
							spill->values[word][row] = (Datum) 0;
							continue;
						}
						if ((ref >> 32) - 1 >= (uint64) side->nvalues ||
							side->value_bases[(ref >> 32) - 1] == NULL)
							ereport(ERROR,
									errcode(ERRCODE_DATA_CORRUPTED),
									errmsg("TessHashJoin read back a value it did not keep"));
						spill->values[word][row] =
							PointerGetDatum(side->value_bases[(ref >> 32) - 1] +
											(ref & 0xFFFFFFFF));
					}
				}
				spill->cursor += count;
				return true;
			}
			spill->block = NULL;
		}
		/* The next chunk: the previous one and its values go. */
		for (int index = 0; index < spill->nblock_values; index++)
			side->value_bases[spill->block_values[index]] = NULL;
		spill->nblock_values = 0;
		MemoryContextReset(spill->block_context);
		if (spill->reader.open)
		{
			while (part_header(&spill->reader, &header))
			{
				void	   *body = MemoryContextAllocExtended(spill->block_context,
															  Max(header.len, 8),
															  MCXT_ALLOC_HUGE);

				part_body(&spill->reader, body, header.len);
				if (header.kind == TESS_SPILL_VALUES)
				{
					/* Another writer's number may be past this one's. */
					if (header.number >= INT_MAX - 1)
						ereport(ERROR,
								errcode(ERRCODE_DATA_CORRUPTED),
								errmsg("TessHashJoin read back a value chunk it never wrote"));
					side_value_slot(side, (int) header.number);
					side->nvalues = Max(side->nvalues, (int) header.number + 1);
					side->value_bases[header.number] = body;
					grow_ints(spill->context, &spill->block_values,
							  &spill->block_slots, spill->nblock_values + 1);
					spill->block_values[spill->nblock_values++] = header.number;
					continue;
				}
				spill->block = body;
				break;
			}
			if (spill->block == NULL)
				part_close(&spill->reader);
		}
		if (spill->block == NULL && !spill->tail_read)
		{
			int			index = side->current[spill->partition];

			spill->tail_read = true;
			if (index != 0 && side_chunk_rows(side, side->bases[index]))
			{
				spill->block = side->bases[index];
				state->counters[JOIN_TAILS]++;
			}
		}
		if (spill->block == NULL)
			return false;
		if (!side->columnar)
			elog(ERROR, "TessHashJoin reads back outer rows in chunks of columns only");
		spill->cursor = 0;
		join_note_memory(state);
	}
}

/* A column of a batch of outer rows read back: a stored column's values. */
static void
spill_get_column(TessBatch *batch, int column, const TessRowMask *rows,
				 TessColumnPurpose purpose, TessDatumColumn *result)
{
	JoinSpill  *spill = (JoinSpill *) batch->private_data;
	int			word;

	if (column < 0 || column >= spill->nchild || spill->word_of[column] == 0)
		elog(ERROR, "TessHashJoin did not keep outer column %d", column);
	word = spill->word_of[column] - 1;
	result->values = spill->window[word];
	result->isnull = spill->isnull[word];
	result->nrows = batch->rows.nrows;
}

/*
 * An outer batch, while the table spills: the valid rows of the
 * partitions on disk leave the batch's probe. Those of a partition with
 * no inner row, and those the filter of every inner row rejects, have no
 * pair and stay to be answered now; the others are written as records of
 * their partitions, leave the rows to answer and come back when their
 * partition is joined.
 */
void
join_spill_outer(TessHashJoinState *state, TessBatch *batch, TessRowMask *valid)
{
	JoinSpill  *spill = state->spill;
	SpillSide  *side = &spill->probe;
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask candidates = {nrows, state->probe.pending_bits};
	TessRowMask pending = {nrows, state->probe.next_bits};
	uint64		any = 0;

	for (int word = 0; word < nwords; word++)
	{
		uint64		bits = valid->bits[word];
		uint64		out = 0;
		uint64		written = 0;

		while (bits != 0)
		{
			int			bit = pg_rightmost_one_pos64(bits);
			int			partition = spill_partition(spill, state->probe.hashes[word * 64 + bit]);

			bits &= bits - 1;
			if (spill->build.parts[partition].resident)
				continue;
			out |= UINT64CONST(1) << bit;
			if (spill->build.rows[partition] > 0)
				written |= UINT64CONST(1) << bit;
		}
		valid->bits[word] &= ~out;
		state->probe.pending_bits[word] = written;
		any |= written;
	}
	if (any == 0)
		return;
	/*
	 * The filter of every inner row: a row it rejects has no pair anywhere.
	 * The kernel fills the mask whole, but checks it is a mask of nrows.
	 */
	memset(state->probe.next_bits, 0, sizeof(uint64) * nwords);
	if (spill->bloom != NULL)
		check(state, state->kernels->bloom_probe(spill->bloom, spill->bloom_words,
												 state->probe.hashes, &candidates, &pending,
												 &state->status));
	else
		memcpy(state->probe.next_bits, state->probe.pending_bits, sizeof(uint64) * nwords);
	state->counters[JOIN_BLOOM_REMOVED] +=
		tess_row_mask_count(&candidates) - tess_row_mask_count(&pending);
	if (tess_row_mask_count(&pending) == 0)
		return;
	for (int word = 0; word < nwords; word++)
		state->active_bits[word] &= ~state->probe.next_bits[word];

	join_side_append(state, side, batch, &pending, spill->stored, NULL);
	make_room(state, false);
}

/* The bits of count outer rows of the joined partition from an ordinal. */
uint64
join_matched_word(const JoinSpill *spill, int count)
{
	uint64		word = 0;

	for (int row = 0; row < count; row++)
	{
		uint64		at = spill->batch_ordinal + row;

		if (at / 64 < spill->matched_words)
			word |= ((spill->matched_rows[at / 64] >> (at % 64)) & 1) << row;
	}
	return word;
}

/*
 * RIGHT and FULL: whether this participant returns the tail of the table
 * in memory now: a table of its own's, once; a shared table's, the last
 * participant to leave it, which the others leave here, their tail done.
 */
bool
join_tail_turn(TessHashJoinState *state)
{
	if (!state->preserve_inner || state->tail.table_done)
		return false;
	if (state->marks_shared &&
		!(state->parallel.round_partition >= 0 ? join_round_depart(state) : join_leave_shared(state, true)))
	{
		state->tail.table_done = true;
		return false;
	}
	return true;
}

/*
 * RIGHT and FULL over a table that spills: before the table in memory goes
 * (the resident partitions', a piece's, a partition's), its records
 * without a pair go out; true asks the caller for that tail first.
 */
static bool
tail_first(TessHashJoinState *state)
{
	if (!join_tail_turn(state))
		return false;
	state->tail.request = true;
	return true;
}

/*
 * The next outer batch: the outer child's, while it has one, then those
 * of the partitions on disk, each joined in turn. The rows to answer are
 * the batch's selected ones, until the table sends some to disk.
 */
TessBatch *
join_outer_next(TessHashJoinState *state)
{
	for (;;)
	{
		JoinSpill  *spill = state->spill;
		TessBatch  *batch = NULL;

		/* Outer rows read back from disk come through no child that checks. */
		CHECK_FOR_INTERRUPTS();
		/* A shared table answers its rows first, then goes: not while a compact batch holds its pairs. */
		if (spill != NULL && spill->shared && !spill->resident_done)
		{
			batch = join_shared_resident_next(state);
			if (batch != NULL)
				return batch;
			/* RIGHT and FULL: the last one to leave the table returns its tail first. */
			if (state->holding || tail_first(state))
				return NULL;
			join_shared_resident_end(state);
			continue;
		}
		if (spill == NULL || !spill->joining)
		{
			/* The outer rows: the child's, or those of the partition a level splits. */
			if (spill == NULL || spill->parent == NULL)
			{
				if (spill == NULL || !spill->child_done)
					batch = tess_input_next(state->outer_input);
				if (batch != NULL)
				{
					join_reserve_rows(state, batch->rows.nrows);
					memcpy(state->active_bits, batch->rows.bits,
						   sizeof(uint64) * tess_row_mask_word_count(batch->rows.nrows));
					state->counters[JOIN_PROBE_ROWS] += tess_row_mask_count(&batch->rows);
					return batch;
				}
				if (spill == NULL)
					return NULL;
			}
			else if (!spill->child_done && join_next_spilled(state, spill->parent))
			{
				batch = &spill->parent->batch;
				join_reserve_rows(state, batch->rows.nrows);
				state->active_bits[0] = spill->parent->bits[0];
				return batch;
			}
			/* The resident table goes: not while a compact batch holds its pairs. */
			spill->child_done = true;
			if (state->holding || tail_first(state))
				return NULL;
			start_joining(state);
		}
		if (spill->partition >= 0 && spill->partition < spill->npartitions &&
			join_next_spilled(state, spill))
		{
			uint64		active = spill->bits[0];

			/*
			 * Passes over the outer rows: left and anti joins answer the rows
			 * without a pair in the last one only.
			 */
			if (spill->multipass &&
				(state->jointype == JOIN_LEFT || state->jointype == JOIN_ANTI))
				active = spill->final_pass ?
					active & ~join_matched_word(spill, spill->batch.rows.nrows) : 0;
			join_reserve_rows(state, spill->batch.rows.nrows);
			state->active_bits[0] = active;
			return &spill->batch;
		}
		/* A round's next outer file, probing the same table. */
		if (state->parallel.round_partition >= 0 && !state->round_departed &&
			join_round_next_outer(state))
			continue;
		/* RIGHT and FULL: the table's records without a pair before it goes. */
		if (state->holding || tail_first(state))
			return NULL;
		if (spill->partition >= 0 && spill->partition < spill->npartitions &&
			join_next_pass(state))
			continue;
		/* The next partition: loaded, or split into a level below. */
		if (spill->partition < spill->npartitions && next_partition(state))
			continue;
		/* The level is done: back to the level whose partition it split. */
		if (spill->parent == NULL)
			return NULL;
		pop_level(state);
	}
}

/*
 * Finish an outer batch: the child's goes back to it; one read back notes
 * the rows that found a pair, when the partition is joined in passes.
 */
void
join_outer_finish(TessHashJoinState *state, TessBatch *batch)
{
	JoinSpill  *spill = state->spill;

	while (spill != NULL && batch != &spill->batch)
		spill = spill->parent;
	if (spill == NULL)
	{
		tess_input_finish(state->outer_input);
		return;
	}
	if (spill->matched_rows != NULL && !spill->final_pass)
	{
		uint64		bits = state->matched_bits[0];

		while (bits != 0)
		{
			uint64		at = spill->batch_ordinal + pg_rightmost_one_pos64(bits);

			bits &= bits - 1;
			/* A shared table's partition has outer rows of every participant, not counted. */
			if (at / 64 >= spill->matched_words)
			{
				Size		words = Max(spill->matched_words * 2, at / 64 + 1);

				spill->matched_rows = repalloc_huge(spill->matched_rows, sizeof(uint64) * words);
				memset(spill->matched_rows + spill->matched_words, 0,
					   sizeof(uint64) * (words - spill->matched_words));
				spill->matched_words = words;
			}
			spill->matched_rows[at / 64] |= UINT64CONST(1) << (at % 64);
		}
	}
}

/* Delete the files and free the memory of spilling. */
void
join_spill_free(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	if (spill == NULL)
		return;
	while (spill != NULL)
	{
		JoinSpill  *parent = spill->parent;

		level_free(spill);
		spill = parent;
	}
	state->spill = NULL;
	state->holding = false;
	state->values.bases = NULL;
	state->values.slots = 0;
	state->values.nchunks = 0;
	MemoryContextReset(state->table_context);
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->table_bytes = 0;
	state->bloom.bits = NULL;
	state->bloom.nwords = 0;
}


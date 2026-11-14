#include "postgres.h"

#include "miscadmin.h"
#include "nodes/nodeFuncs.h"
#include "utils/lsyscache.h"
#include "utils/datum.h"
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
static void side_full(TessHashJoinState *state, SpillSide *side, int partition);

void
join_part_open(PartReader *reader, TessSpill *file, int partition, int writers)
{
	reader->file = file;
	reader->partition = partition;
	reader->writers = writers;
	reader->next = 0;
	reader->reader = NULL;
	reader->pairs = 0;
	reader->open = true;
}

/* The file being read, the next writer's once one ends; NULL after the last. */
static TessSpillReader *
part_file(PartReader *reader)
{
	/* Every block read back from disk passes here: a block at a time. */
	CHECK_FOR_INTERRUPTS();
	while (reader->open && reader->reader == NULL)
	{
		if (reader->next >= reader->writers)
		{
			reader->open = false;
			break;
		}
		reader->reader = tess_spill_open(reader->file, reader->next++, reader->partition);
		reader->pairs = 0;
	}
	return reader->reader;
}

/* The file read to its end: the next writer's comes next. */
static void
part_file_done(PartReader *reader)
{
	tess_spill_close(reader->reader);
	reader->reader = NULL;
}

/* The next block's header, from the next writer's file once one ends. */
static bool
part_header(PartReader *reader, TessSpillHeader *header)
{
	TessSpillReader *file;

	while ((file = part_file(reader)) != NULL)
	{
		if (tess_spill_read_header(file, header))
			return true;
		part_file_done(reader);
	}
	return false;
}

/*
 * The next pair of a chunk of columns into columns, read and checked,
 * from the next writer's file once one ends; a writer numbers the pairs
 * of each partition from 0.
 */
static bool
part_load(PartReader *reader, TessColumnsReader *columns)
{
	TessSpillReader *file;

	while ((file = part_file(reader)) != NULL)
	{
		if (tess_columns_reader_load(columns, file, reader->pairs, -1))
		{
			reader->pairs++;
			return true;
		}
		part_file_done(reader);
	}
	return false;
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

/* The bytes chunk index of a side of records holds, its used mark checked. */
static Size
side_chunk_used(TessHashJoinState *state, const SpillSide *side, int index)
{
	Size		used = 0;

	check(state, state->kernels->table_chunk_used(side->bases[index], side->lens[index],
												  side->record_size, &used, &state->status));
	return used;
}

/* Whether chunk index of the side holds a row. */
static bool
side_chunk_rows(TessHashJoinState *state, const SpillSide *side, int index)
{
	return side->columnar ? tess_spill_columns_rows(side->bases[index]) > 0 :
		side_chunk_used(state, side, index) > TESS_TABLE_CHUNK_HEADER;
}

/* A chunk of len bytes holds no row of nwords words. */
pg_noreturn static pg_noinline void
chunk_too_short(Size len, int nwords)
{
	elog(ERROR, "TessHashJoin made a chunk of %zu bytes for rows of %d words", len, nwords);
}

/* Make the len bytes at base an empty chunk of the side: of records, or of columns. */
static void
side_chunk_init(TessHashJoinState *state, SpillSide *side, void *base, Size len)
{
	Size		capacity;

	if (side->columnar)
	{
		check(state, state->kernels->spill_columns_init(base, len, side->nwords,
														&capacity, &state->status));
		/* A chunk for rows that holds none would be retried without end. */
		if (unlikely(capacity == 0 && len > TESS_SPILL_COLUMNS_HEADER))
			chunk_too_short(len, side->nwords);
	}
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
 * A side of nkeys keys of the kinds and a payload of NULL bits (a word,
 * or for chunks of columns a lane for every 64 words) and nwords words of
 * the types, with no chunk yet but the empty one and an index for the
 * layout only.
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

	if (resident)
		side->context = AllocSetContextCreate(spill->context,
											  "TessHashJoin inner partitions",
											  TESS_CHUNK_CONTEXT_SIZES);
	else
		side->context = AllocSetContextCreate(spill->context,
											  "TessHashJoin outer partitions",
											  TESS_CHUNK_CONTEXT_SIZES);
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
	side->payload_size = sizeof(uint64) *
		((side->columnar ? tess_spill_columns_null_lanes(nwords) : 1) + nwords);
	check(state, state->kernels->table_record_size(nkeys, side->payload_size,
												   &side->record_size, &state->status));
	side->chunk_len = chunk_len;
	check(state, state->kernels->table_size(nkeys, side->payload_size,
											JOIN_INITIAL_ROWS,
											&side->ref.index_len, &state->status));
	side->ref.index = MemoryContextAllocZero(side->context, side->ref.index_len);
	check(state, state->kernels->table_create(side->ref.index, side->ref.index_len,
											  nkeys, side->payload_size,
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
	side->disk_bytes = MemoryContextAllocZero(side->context, sizeof(uint64) * npartitions);
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
	config.max_chunk = chunk_len;
	config.buffer_len = TESS_SPILL_BUFFER_LEN(tess_hash_memory_limit());
	side->file = tess_spill_create(&config);
}

/*
 * Count bytes of the inner side's chunks in the words of its spilling, of
 * a partition: a shared table's or the side's own; the rule that sends
 * partitions to disk weighs them. The outer sides keep no words.
 */
void
join_side_count(SpillSide *side, int partition, int64 delta)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	bool		over;

	if (side->spill_words == NULL)
		return;
	if (side->kernels->table_spill_add_bytes(side->spill_words, side->spill_nwords,
											 side->area != NULL, delta, partition,
											 &over, &status) != TESS_OK)
		tess_status_report(&status);
}

/* The bytes a chunk of len bytes takes: with its header when shared. */
static inline int64
side_block(SpillSide *side, Size len)
{
	return (int64) (side->area != NULL ? JOIN_CHUNK_HEADER + len : len);
}

/*
 * What a chunk of records of len bytes counts: in a shared side's budget,
 * its index and filter included; in a side's own, its bytes.
 */
static inline int64
side_chunk_cost(SpillSide *side, Size len)
{
	return side->area != NULL ? record_chunk_cost(len, side->record_size) : (int64) len;
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

/* Room for chunk `index` in the side's directory, within the most chunks. */
static void
side_reserve(SpillSide *side, int index)
{
	table_chunks_limit(index + 1, "TessHashJoin hash table");
	if (index < side->slots)
		return;
	side->pointers = repalloc0(side->pointers, sizeof(dsa_pointer) * side->slots,
							   sizeof(dsa_pointer) * side->slots * 2);
	side->slots *= 2;
	side->bases = repalloc(side->bases, sizeof(void *) * side->slots);
	side->lens = repalloc(side->lens, sizeof(Size) * side->slots);
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
	Size		slots = Max(side->value_slots, 16);

	if (number < side->value_slots)
		return;
	while (slots <= (Size) number)
		slots *= 2;
	if (slots > INT_MAX)
		slots = INT_MAX;
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

	side_reserve(side, index);
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
	side->disk_bytes[partition] += len;
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

		tess_loop_turn("values");
		write_block(state, side, partition, TESS_SPILL_VALUES, number,
					side->value_bases[number], side->value_lens[number]);
		side_free_values(side, partition, number);
	}
	part->nvalues = 0;
	part->value_current = -1;
	part->value_bytes = 0;
}

/* Write chunk `index` of records of the partition, unless it holds no row. */
static void
side_write_records(TessHashJoinState *state, SpillSide *side, int partition,
				   int index)
{
	Assert(!side->columnar);
	if (!side_chunk_rows(state, side, index))
		return;
	write_block(state, side, partition, TESS_SPILL_RECORDS,
				side->next_number++, side->bases[index],
				side_chunk_used(state, side, index));
}

/* Free the values of the chunk of columns a partition fills. */
static void
side_free_pair_values(SpillSide *side, int partition)
{
	SpillPart  *part = &side->parts[partition];

	if (part->pair_values == NULL)
		return;
	pfree(part->pair_values);
	part->pair_values = NULL;
	side->bytes -= part->value_len;
	part->bytes -= part->value_len;
	part->value_len = 0;
	part->value_used = 0;
}

/*
 * Write the chunk of columns a partition fills, unless it holds no row,
 * as a pair: its values, unless it has none, then the chunk, numbered as
 * the pairs of the partition written before it; its values go.
 */
static void
side_write_pair(TessHashJoinState *state, SpillSide *side, int partition)
{
	SpillPart  *part = &side->parts[partition];
	int			index = side->current[partition];
	uint32		number = (uint32) part->blocks;

	if (index != 0 && side_chunk_rows(state, side, index))
	{
		if (part->value_used > 0)
			write_block(state, side, partition, TESS_SPILL_VALUES, number, part->pair_values,
						part->value_used);
		write_block(state, side, partition, TESS_SPILL_COLUMNS, number, side->bases[index],
					side->lens[index]);
	}
	side_free_pair_values(side, partition);
}

/* Write what a partition on disk holds in memory: its values and its tail. */
static void
side_write_tail(TessHashJoinState *state, SpillSide *side, int partition)
{
	int			index = side->current[partition];

	if (side->columnar)
	{
		side_write_pair(state, side, partition);
		return;
	}
	side_write_values(state, side, partition);
	if (index != 0)
		side_write_records(state, side, partition, index);
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
	side_write_tail(state, side, partition);
	part->queued = false;
	if (index != 0)
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

		tess_loop_turn("demote");
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
	side_write_tail(state, side, partition);
	side->parts[partition].queued = false;
	if (index == 0)
		return;
	side_free_chunk(side, partition, index);
	side->current[partition] = 0;
}

/*
 * A partition the rule chose (tess_table_spill_evict): one in memory goes
 * to disk; one on disk already, chosen again for the bytes of its tail,
 * writes and frees what this side holds of it. False when that freed
 * nothing, which ends the check: what is left of the partition is
 * another participant's, or no chunk at all.
 */
bool
join_side_send(TessHashJoinState *state, SpillSide *side, int partition)
{
	Size		held = side->parts[partition].bytes;

	if (side->parts[partition].resident)
	{
		join_side_demote(state, side, partition);
		return true;
	}
	side_evict(state, side, partition);
	join_side_compact(side);
	return side->parts[partition].bytes < held;
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
 * Queue a partition whose values outgrew a chunk to be written, unless it
 * is: once in the queue until the queue is emptied.
 */
static void
side_queue(SpillSide *side, int partition)
{
	SpillPart  *part = &side->parts[partition];

	if (part->queued)
		return;
	part->queued = true;
	if (!part->in_queue)
	{
		Assert(side->nqueue < side->npartitions);
		part->in_queue = true;
		side->queue[side->nqueue++] = partition;
	}
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
	ExpandedObjectHeader *expanded;
	Size		size = tess_value_size(value, typlen, &expanded);
	Size		aligned;
	int			number;
	Size		byte;

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
	tess_value_copy(value, expanded, side->value_bases[number] + byte, size);
	if (!part->resident && part->value_bytes > side->chunk_len)
		side_queue(side, partition);
	return JOIN_VALUE_REF(number, byte);
}

/*
 * Copy a by-reference value into the values of the chunk of columns its
 * partition fills, which grow as they need, and return its reference into
 * them. A partition whose values outgrow a chunk is queued to be written.
 */
static uint64
side_store_pair(SpillSide *side, int partition, Datum value, int16 typlen)
{
	SpillPart  *part = &side->parts[partition];
	ExpandedObjectHeader *expanded;
	Size		size = tess_value_size(value, typlen, &expanded);
	Size		byte = part->value_used;

	/* A reference names a byte of the first 4 GiB of a pair's values. */
	if (MAXALIGN(size) > ((Size) 1 << TESS_SPILL_VALUE_BYTE_BITS) - byte)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin keeps no more than 4 GB of values in a chunk of outer rows")));
	if (byte + MAXALIGN(size) > part->value_len)
	{
		Size		len = Max(Max(side->chunk_len, part->value_len * 2), byte + MAXALIGN(size));

		part->pair_values = part->pair_values == NULL ?
			MemoryContextAllocExtended(side->context, len, MCXT_ALLOC_HUGE) :
			repalloc_huge(part->pair_values, len);
		side->bytes += len - part->value_len;
		part->bytes += len - part->value_len;
		part->value_len = len;
	}
	tess_value_copy(value, expanded, part->pair_values + byte, size);
	part->value_used = byte + MAXALIGN(size);
	if (part->value_used > side->chunk_len)
		side_queue(side, partition);
	return TESS_SPILL_VALUE_REF(0, byte);
}

/* Write the partitions whose values outgrew a chunk, and empty the queue. */
static void
side_flush_queue(TessHashJoinState *state, SpillSide *side)
{
	for (int index = 0; index < side->nqueue; index++)
	{
		SpillPart  *part = &side->parts[side->queue[index]];

		part->in_queue = false;
		/* Its values went in after a row of its chunk: it holds one. */
		Assert(!part->queued || side->current[side->queue[index]] != 0);
		if (part->queued)
			side_full(state, side, side->queue[index]);
	}
	side->nqueue = 0;
}

/*
 * The bytes spilling takes in memory, with the index the resident rows
 * will need once the build is read, as join_index_table makes it; the
 * resident rows into *resident unless NULL.
 */
Size
join_spill_memory(TessHashJoinState *state, JoinSpill *spill, uint64 *resident)
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
	return bytes + (spill->indexed || rows == 0 ? 0 : join_index_bytes(state, rows));
}

/*
 * The weights a shared table gives the rule that sends its partitions to
 * disk (docs/spill.md, "Weights"): it counts its chunks, not a
 * participant's tails, and its records only once built; its participants
 * send one partition a check, since the others write theirs of it at
 * their next batch.
 */
TessSpillWeights
join_spill_weights(TessHashJoinState *state)
{
	TessSpillWeights weights = {
		.start = tess_join_spill_start,
		.target = tess_join_spill_target,
		.spilled = tess_join_spill_spilled_weight,
		.per_check = (uint32) tess_join_shared_spill_evictions,
	};

	Assert(state->spill->shared);
	return weights;
}

/*
 * Step the machine of a level, or the first one's before its level is
 * made (spill NULL), with event: its action.
 */
static TessJoinAction
spill_step(TessHashJoinState *state, JoinSpill *spill, TessJoinEvent event)
{
	TessJoinHost host = {0};
	TessJoinAction action;
	TessJoinMachine *machine = &state->machine;

	if (spill != NULL)
	{
		Assert(spill->machine != NULL);
		machine = spill->machine;
		/* A shared table's words are every participant's: its machine reads none. */
		if (!spill->shared)
		{
			host.words = spill->build.spill_words;
			host.nwords = spill->build.spill_nwords;
			host.routes = spill->routes;
			host.nroutes = spill->npartitions;
		}
		host.rows = spill->build.rows;
		host.nrows = spill->npartitions;
		host.total = spill->total_rows;
		host.outer = spill->probe.rows;
		host.nouter = spill->npartitions;
		host.disk = spill->build.disk_bytes;
		host.ndisk = spill->npartitions;
	}
	check(state, state->kernels->join_step(machine, &host, &event, &action, &state->status));
	return action;
}

/*
 * A partition of a side whose chunk is full, or whose values outgrew a
 * chunk: as the level's machine says, it takes a new chunk or writes what
 * it holds. A shared table's level keeps the rule of its participants'
 * build: one in memory, or without a chunk, takes a new chunk.
 */
static void
side_full(TessHashJoinState *state, SpillSide *side, int partition)
{
	JoinSpill  *spill = state->spill;
	bool		tail = side->current[partition] != 0;
	bool		add;

	if (spill->shared)
		add = side->parts[partition].resident || !tail;
	else
		add = spill_step(state, spill, (TessJoinEvent) {
			.kind = TESS_JOIN_FULL,
			.side = side == &spill->build ? 0 : 1,
			.partition = (uint32) partition,
			.flag = tail,
		}).kind == TESS_JOIN_ADD_CHUNK;
	if (add)
		side_add_chunk(state, side, partition);
	else
		join_side_flush(state, side, partition);
}

/*
 * Send the inner side's partitions to disk as the level's machine says (a
 * shared table's participants decide in shared_sync): while building,
 * against the memory the node reports, so that what it keeps is what it
 * says; at the end of the build, by the share of the rows in memory alone,
 * after which the machine sets where the outer rows of each partition go.
 */
static void
evict_partitions(TessHashJoinState *state, bool building)
{
	JoinSpill  *spill = state->spill;
	SpillSide  *side = &spill->build;
	TessJoinEvent event = {
		.kind = building ? TESS_JOIN_GROWN : TESS_JOIN_BUILD_END,
		.memory = building ? join_memory(state) : 0,
	};

	Assert(!spill->shared);
	for (;;)
	{
		TessJoinAction action;
		bool		freed = true;

		CHECK_FOR_INTERRUPTS();
		action = spill_step(state, spill, event);
		if (action.kind == TESS_JOIN_DONE)
			break;
		if (action.kind == TESS_JOIN_DEMOTE)
			join_side_demote(state, side, (int) action.partition);
		else
		{
			Size		held = side->parts[action.partition].bytes;

			Assert(action.kind == TESS_JOIN_EVICT);
			side_evict(state, side, (int) action.partition);
			join_side_compact(side);
			freed = side->parts[action.partition].bytes < held;
		}
		event = (TessJoinEvent) {
			.kind = TESS_JOIN_SENT,
			.flag = freed,
			.memory = building ? join_memory(state) : 0,
		};
	}
}

/* Keep spilling within hash_mem while building. */
static void
make_room(TessHashJoinState *state, bool building)
{
	if (building)
		evict_partitions(state, true);
	join_note_memory(state);
}

/* The type of column column of the outer child's batches: its target's. */
static void
outer_column_type(TessHashJoinState *state, int column, int16 *typlen, bool *byval)
{
	const TessLayout *layout = tess_input_layout(state->outer_input);
	List	   *targets = state->outer->plan->targetlist;

	for (int target = 0; target < layout->ntargets && target < list_length(targets); target++)
	{
		if (tess_layout_column(layout, target) == column)
		{
			TargetEntry *entry = list_nth_node(TargetEntry, targets, target);

			get_typlenbyval(exprType((Node *) entry->expr), typlen, byval);
			return;
		}
	}
	elog(ERROR, "TessHashJoin found no target of outer column %d", column);
}

/*
 * The level plan's shape of the join's levels: each partition keeps its
 * reserve, JOIN_SPILL_RESERVE, and the chunks take a sixteenth of the
 * limit among the partitions, from JOIN_SPILL_MIN_CHUNK to JOIN_CHUNK_LEN,
 * of four records at least.
 */
TessSpillShape
join_spill_shape(TessHashJoinState *state)
{
	return (TessSpillShape) {
		.reserve = JOIN_SPILL_RESERVE,
		.share = 16,
		.min_chunk = JOIN_SPILL_MIN_CHUNK,
		.max_chunk = JOIN_CHUNK_LEN,
		.record = state->record_size,
		.min_partitions = JOIN_SPILL_MIN_PARTITIONS,
		.max_partitions = JOIN_SPILL_MAX_PARTITIONS,
	};
}

/*
 * The words a level keeps of an outer row, each once: the columns of the
 * outer child the node reads, those asked for, by their types; then each
 * key, one a word does not hold, such as a text, by the 64-bit hash the
 * table keys by, a word of its own, so that its value is not copied and
 * a batch read back is not hashed again; a word key by its column, by its
 * own type when no column asked for it, such as an expression the child
 * computes. Into stored, the child's column of each word, -(k + 1) for
 * the hash of key k; word_of, the word of each of the nchild child columns
 * plus one, zeroed; key_words, the word of each key plus one; and the
 * words' types. Their count.
 */
static int
outer_layout(TessHashJoinState *state, int *stored, int *word_of, int *key_words,
			 int16 *typlens, bool *byvals)
{
	int			nstored = 0;

	for (int index = 0; index < state->compact.nouter; index++)
	{
		int			scan = state->compact.outer_columns[index];
		int			child = state->child_columns[scan];

		if (word_of[child] != 0)
			continue;
		stored[nstored] = child;
		typlens[nstored] = state->typlens[scan];
		byvals[nstored] = state->typbyvals[scan];
		word_of[child] = ++nstored;
	}
	for (int key = 0; key < state->keys.nkeys; key++)
	{
		int			child = state->keys.outer_keys[key];

		if (OidIsValid(state->keys.hashers[key].fn_oid))
		{
			stored[nstored] = -(key + 1);
			typlens[nstored] = sizeof(int64);
			byvals[nstored] = true;
			key_words[key] = ++nstored;
			continue;
		}
		if (word_of[child] == 0)
		{
			stored[nstored] = child;
			outer_column_type(state, child, &typlens[nstored], &byvals[nstored]);
			word_of[child] = ++nstored;
		}
		key_words[key] = word_of[child];
	}
	return nstored;
}

/* The outer child's columns an outer row is read from: one past the last. */
static int
outer_children(TessHashJoinState *state)
{
	int			nchild = 0;

	for (int key = 0; key < state->keys.nkeys; key++)
		nchild = Max(nchild, state->keys.outer_keys[key] + 1);
	for (int index = 0; index < state->compact.nouter; index++)
		nchild = Max(nchild, state->child_columns[state->compact.outer_columns[index]] + 1);
	return nchild;
}

/*
 * A build starts: the machine of the join's spill, whose table fits so
 * far, with what every level shares: hash_mem, the shape of the levels,
 * the outer rows a level keeps and the weights of the settings.
 */
void
join_spill_begin(TessHashJoinState *state)
{
	int			nwords = state->keys.nkeys + state->compact.nouter;
	int		   *stored = palloc(sizeof(int) * Max(nwords, 1));
	int		   *word_of = palloc0(sizeof(int) * Max(outer_children(state), 1));
	int			key_words[TESS_TABLE_MAX_KEYS];
	int16	   *typlens = palloc(sizeof(int16) * Max(nwords, 1));
	bool	   *byvals = palloc(sizeof(bool) * Max(nwords, 1));
	TessJoinParams *params = &state->spill_params;
	int			nstored = outer_layout(state, stored, word_of, key_words, typlens, byvals);

	*params = (TessJoinParams) {
		.limit = tess_hash_memory_limit(),
		.shape = join_spill_shape(state),
		.outer_words = (uint64) nstored,
		.page = BLCKSZ,
		.start = tess_join_spill_start,
		.target = tess_join_spill_target,
		.spilled = tess_join_spill_spilled_weight,
		.resident = tess_join_spill_resident_share,
		.tail = tess_join_spill_tail_weight,
		.preserve_inner = state->preserve_inner,
		.split_room = tess_join_spill_split_room,
		.split_key = tess_join_spill_split_key_share,
	};
	for (int word = 0; word < nstored; word++)
		params->outer_values |= !byvals[word];
	pfree(stored);
	pfree(word_of);
	pfree(typlens);
	pfree(byvals);
	check(state, state->kernels->join_init(&state->machine, params, &state->status));
}

/*
 * A level of partitions by the plan of start, a TESS_JOIN_START, below
 * parent or the first; its machine, when it has one, is the caller's to
 * set and to tell it is made (level_made).
 */
static JoinSpill *
spill_create(TessHashJoinState *state, JoinSpill *parent, const TessJoinAction *start)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	Size		chunk_len = start->plan.chunk_len;
	int			npartitions = (int) start->plan.partitions;
	JoinSpill  *spill;
	int16	   *typlens;
	bool	   *byvals;
	int			nchild = outer_children(state);
	int			nstored;

	spill = MemoryContextAllocZero(context, sizeof(JoinSpill));
	state->spill = spill;
	spill->parent = parent;
	spill->level = parent == NULL ? 0 : parent->level + 1;
	spill->context = AllocSetContextCreate(context, "TessHashJoin spill",
										   ALLOCSET_DEFAULT_SIZES);
	spill->part_context = AllocSetContextCreate(spill->context,
												"TessHashJoin partition",
												TESS_CHUNK_CONTEXT_SIZES);
	spill->block_context = AllocSetContextCreate(spill->context,
												 "TessHashJoin outer block",
												 TESS_CHUNK_CONTEXT_SIZES);
	spill->npartitions = npartitions;
	spill->shift = start->plan.shift;
	spill->below = start->plan.below;
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

	/* The outer side: the words of outer_layout, in chunks the plan sized. */
	spill->nchild = nchild;
	spill->word_of = MemoryContextAllocZero(spill->context, sizeof(int) * Max(nchild, 1));
	spill->stored = MemoryContextAlloc(spill->context,
									   sizeof(int) * (state->keys.nkeys + state->compact.nouter));
	typlens = repalloc(typlens, sizeof(int16) * (state->keys.nkeys + state->compact.nouter));
	byvals = repalloc(byvals, sizeof(bool) * (state->keys.nkeys + state->compact.nouter));
	nstored = outer_layout(state, spill->stored, spill->word_of, spill->key_words, typlens,
						   byvals);
	Assert((uint64) nstored == state->spill_params.outer_words);
	/* What tess_spill_columns_append_partitioned takes; no plan reaches it. */
	if (nstored > 2048)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin cannot spill more than 2048 outer columns")));
	side_init(state, &spill->probe, state->keys.nkeys, state->keys.outer_kinds, nstored, typlens, byvals,
			  start->outer_len, false, npartitions);
	/* The outer rows a shared table answers: one partition of their own. */
	if (state->parallel.shared != NULL && parent == NULL)
		side_init(state, &spill->resident, state->keys.nkeys, state->keys.outer_kinds, nstored,
				  typlens, byvals, start->outer_len, false, 1);
	spill->rows = &spill->probe;
	tess_columns_reader_init(&spill->outer, state->kernels, spill->block_context, nstored,
							 nstored, spill->probe.byvals, spill->probe.typlens);
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
	spill->batch.values_stored = true;
	return spill;
}

/*
 * A level its machine started is made: the words that count its
 * partitions' bytes and where the outer rows of each go, which the
 * machine then takes.
 */
static void
level_made(TessHashJoinState *state, JoinSpill *spill, TessJoinMachine *machine,
		   const TessJoinAction *start)
{
	SpillSide  *build = &spill->build;

	spill->machine = machine;
	build->spill_nwords = start->plan.words;
	build->spill_words = MemoryContextAllocZero(build->context,
												sizeof(uint64) * build->spill_nwords);
	build->kernels = state->kernels;
	spill->routes = MemoryContextAllocZero(spill->context, Max(spill->npartitions, 1));
	(void) spill_step(state, spill, (TessJoinEvent) {.kind = TESS_JOIN_MADE});
}

/* The level below parent its machine asked for, with its own machine. */
static JoinSpill *
spill_below(TessHashJoinState *state, JoinSpill *parent, const TessJoinBelow *below)
{
	TessJoinMachine machine;
	TessJoinAction start;
	JoinSpill  *spill;

	check(state, state->kernels->join_below(&machine, &state->spill_params, below, &start,
											&state->status));
	spill = spill_create(state, parent, &start);
	spill->own = machine;
	level_made(state, spill, &spill->own, &start);
	return spill;
}

/*
 * A shared table's first level, whose partitions the participant that
 * split the table planned: the shared build decides the rest of it, but
 * for whether a partition taken alone splits, which its machine does.
 */
JoinSpill *
join_spill_given(TessHashJoinState *state, uint32 partitions)
{
	TessJoinMachine machine;
	TessJoinAction start;
	JoinSpill  *spill;

	check(state, state->kernels->join_given(&machine, &state->spill_params, partitions, &start,
											&state->status));
	spill = spill_create(state, NULL, &start);
	spill->own = machine;
	spill->machine = &spill->own;
	return spill;
}

/*
 * Before an inner batch goes into the table in memory: past hash_mem with
 * needed bytes, as the machine decides, the first level of partitions,
 * for twice what was read, or what the planner expects if more; a Bloom
 * filter of every inner row, and the table built so far moved into the
 * partitions.
 */
void
join_spill_check(TessHashJoinState *state, uint64 needed)
{
	TessJoinAction start;
	JoinSpill  *spill;
	uint64		expected_rows;

	start = spill_step(state, NULL, (TessJoinEvent) {
		.kind = TESS_JOIN_INNER,
		.held = state->table_bytes + state->values.bytes,
		.rows = state->build_rows,
		.estimate = state->inner_rows,
		.memory = needed,
	});
	if (start.kind != TESS_JOIN_START)
		return;
	spill = spill_create(state, NULL, &start);
	level_made(state, spill, &state->machine, &start);

	/* A filter of every inner row, sized for the rows expected. */
	state->counters[JOIN_BLOOM_FILTERS]++;
	expected_rows = Max((uint64) state->inner_rows, state->build_rows * 2);
	check(state, state->kernels->table_bloom_words_within(Max(expected_rows, 1),
														  tess_hash_memory_limit(),
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
 * lens, of a chunk read back, are the value chunks' lengths: its values
 * are checked against them, and a chunk the split refuses is damaged.
 */
void
join_split_chunk(TessHashJoinState *state, void *base, Size len, char *const *values,
				 const Size *lens, int nvalues)
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
	side_reserve(side, side->nchunks);
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

		check_table(state, lens != NULL,
					state->kernels->table_split(&side->ref, side->nkeys, side->payload_size,
												side->current, spill->npartitions,
												spill->shift, source, &from,
												JOIN_COMPACT_ROWS, offsets, hashes,
												&count, &full, &state->status));
		bits = count == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << count) - 1;
		if (count > 0 && spill->bloom != NULL)
			check(state, (spill->shared ? state->kernels->bloom_add_atomic :
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
				const char *value;

				if (side->byvals[word])
					continue;
				/* Read back, a value is NULL by its record's bit, not its word. */
				if (lens != NULL)
				{
					if ((((uint64 *) payloads[index])[0] >> word) & 1)
						continue;
					value = tess_spill_value_at(values, lens, (uint64) nvalues, *slot,
												side->typlens[word]);
				}
				else if (*slot == 0)
					continue;
				else
				{
					if (values[(*slot >> 32) - 1] == NULL)
						elog(ERROR, "TessHashJoin split a record whose values it does not have");
					value = values[(*slot >> 32) - 1] + (*slot & 0xFFFFFFFF);
				}
				*slot = side_store(side, partition, PointerGetDatum(value),
								   side->typlens[word]);
			}
		}
		if (full >= 0)
			side_full(state, side, full);
		else if (count == 0)
			break;
		/* The records split so far refer to their copies: values may go. */
		side_flush_queue(state, side);
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
		tess_loop_turn("split");
		join_split_chunk(state, state->chunk_bases[chunk], state->chunk_lens[chunk],
					state->values.bases, NULL, 0);
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
 * Make room for the rows still pending: each partition whose chunk is
 * full, or that has none, once (side_full).
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
		side_full(state, side, partition);
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
					tess_spill_columns_word(base, word)[place] =
						side_store_pair(side, partition, columns[word].values[row],
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
	{
		/*
		 * The hash of a key, as the batch's keys hold it; a row with a NULL
		 * key keeps what the words of its later keys held, since a read
		 * stops at its first NULL key.
		 */
		if (children[word] < 0)
			columns[word] = state->keys.key_columns[-children[word] - 1];
		else
			join_child_column(batch, children[word], pending, TESS_COLUMN_FOR_PROJECTION,
							  &columns[word]);
	}
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
	check(state, (spill->shared ? state->kernels->bloom_add_atomic :
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

	/*
	 * Resident partitions holding too few of the inner rows go to disk too:
	 * probing them would cost every outer batch the whole probe for the few
	 * rows of theirs, more than writing them saves.
	 */
	evict_partitions(state, false);

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
	state->values.lens = NULL;
	state->values.nchunks = side->nvalues;
	spill->indexed = true;
	if (spill->parent == NULL)
		join_index_table(state, 0);
	else
	{
		uint64		buckets = state->counters[JOIN_BUCKETS];

		/* The buckets count for the build, not for a level below. */
		join_index_table(state, 0);
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
	side_free_pair_values(side, partition);
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
	(void) spill_step(state, spill, (TessJoinEvent) {.kind = TESS_JOIN_OUTER_END});
	/* Every outer row is written or answered: the filter's work is done. */
	if (spill->bloom != NULL)
		pfree(spill->bloom);
	spill->bloom = NULL;
	spill->bloom_words = 0;
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		tess_loop_turn("tails");
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

/* The outer chunk in hand goes, and so do the buffers of its reader. */
void
join_outer_forget(JoinSpill *spill)
{
	tess_columns_reader_free(&spill->outer);
	MemoryContextReset(spill->block_context);
	spill->block = NULL;
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
	side_release(&spill->build, partition);
	side_release(&spill->probe, partition);
	MemoryContextReset(spill->part_context);
	join_outer_forget(spill);
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

	join_reserve_chunks(state, chunk + 1);
	state->chunk_bases[chunk] = base;
	state->chunk_lens[chunk] = len;
	state->table.nchunks++;
}

/* The rest of spilling and a chunk of outer rows with its values: what a partition's table may not take. */
static Size
spill_used(TessHashJoinState *state)
{
	return join_spill_memory(state, state->spill, NULL) + 2 * state->spill->probe.chunk_len;
}

/*
 * The bytes a piece of a partition's table may take: what hash_mem leaves
 * besides spill_used, two thirds of it, the rest for the index.
 */
static Size
spill_room(TessHashJoinState *state)
{
	Size		limit = tess_hash_memory_limit();
	Size		used = spill_used(state);

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

/* The value chunks a side's writers made: every participant's for a shared table. */
static uint64
side_values_written(SpillSide *side)
{
	return side->area != NULL ? side->values_written : (uint64) side->nvalues;
}

/* A value chunk of the inner side read back, for a piece's table or a split. */
static void
add_loaded_values(JoinSpill *spill, const TessSpillHeader *header, void *body)
{
	SpillSide  *side = &spill->build;
	uint32		number = header->number;

	/* Another participant's number may be past this one's, not past them all. */
	tess_spill_expect_number(header, side_values_written(side));
	side_value_slot(side, (int) number);
	/* A chunk is written once and forgotten after its piece: twice is damage. */
	if (side->value_bases[number] != NULL)
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("TessHashJoin read back value chunk %u twice", number));
	side->nvalues = Max(side->nvalues, (int) number + 1);
	side->value_bases[number] = body;
	side->value_lens[number] = header->len;
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
	bool		values = false;
	int			nread;

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
		{
			add_loaded_values(spill, &spill->carried_header, body);
			values = true;
		}
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

		if (header.kind != TESS_SPILL_VALUES)
			tess_spill_expect_kind(&header, TESS_SPILL_RECORDS);

		/*
		 * A group ends where values follow records: the piece may end
		 * there. Records refer only to the values written before them with
		 * them, so while the piece has no values, records refer to none,
		 * and it may end at any chunk, as for a partition whose values are
		 * all NULL. Records after values may be the rest of their group: a
		 * partition in memory goes to disk with all its chunks after its
		 * values.
		 */
		if (records && loaded >= room &&
			(header.kind == TESS_SPILL_VALUES || !values))
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
			add_loaded_values(spill, &header, body);
			records = false;
			values = true;
			continue;
		}
		add_loaded_chunk(state, body, header.len);
		rows += (header.len - TESS_TABLE_CHUNK_HEADER) / record;
		records = true;
	}
	nread = state->table.nchunks;
	if (spill->carried == NULL)
	{
		/* The file is done: the tail goes with the last piece. */
		part_close(&spill->build_reader);
		spill->pieces_done = true;
		for (int chunk = 0; chunk < part->nchunks; chunk++)
		{
			int			index = part->chunks[chunk];
			Size		used = side_chunk_used(state, side, index);

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
	/* Lengths even with no value chunk, so that every reference is checked. */
	side_value_slot(side, 0);
	state->values.bases = side->value_bases;
	state->values.lens = side->value_lens;
	state->values.nchunks = side->nvalues;
	join_index_table(state, nread);
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
start_level(TessHashJoinState *state, int partition, const TessJoinBelow *below)
{
	JoinSpill  *parent = state->spill;
	SpillSide  *from = &parent->build;
	SpillPart  *part = &from->parts[partition];
	TessSpillHeader header;
	JoinSpill  *spill;
	bool		records = false;

	spill = spill_below(state, parent, below);
	spill->input_rows = from->rows[partition];
	spill->total_rows = from->rows[partition];
	state->counters[JOIN_SPLITS]++;
	state->counters[JOIN_BATCHES] = Max(state->counters[JOIN_BATCHES],
										(uint64) spill->npartitions);
	/* Lengths even with no value chunk, so that every reference is checked. */
	side_value_slot(from, 0);
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
			add_loaded_values(parent, &header, body);
			continue;
		}
		tess_spill_expect_kind(&header, TESS_SPILL_RECORDS);
		body = MemoryContextAllocExtended(spill->block_context, Max(header.len, 8),
										  MCXT_ALLOC_HUGE);
		part_body(&parent->build_reader, body, header.len);
		join_split_chunk(state, body, header.len, from->value_bases, from->value_lens,
						 from->nvalues);
		MemoryContextReset(spill->block_context);
		records = true;
		make_room(state, true);
	}
	part_close(&parent->build_reader);
	for (int chunk = 0; chunk < part->nchunks; chunk++)
	{
		int			index = part->chunks[chunk];

		if (side_chunk_used(state, from, index) > TESS_TABLE_CHUNK_HEADER)
			join_split_chunk(state, from->bases[index], from->lens[index], from->value_bases,
							 NULL, 0);
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
 * Load a partition on disk to join: split into the level below its
 * machine asked for, below, or joined in pieces when too large, whole
 * otherwise; its outer rows read from their first.
 */
bool
join_open_partition(TessHashJoinState *state, int partition, const TessJoinBelow *below)
{
	JoinSpill  *spill = state->spill;

	spill->partition = partition;
	join_part_open(&spill->build_reader, spill->build.file, partition, spill->writers);
	spill->pieces_done = false;
	if (below != NULL)
	{
		/*
		 * Too large, and much smaller than what this level split, so not
		 * one key: it splits by the next bits into a level below, whose
		 * outer rows are the partition's.
		 */
		open_outer_rows(state);
		start_level(state, partition, below);
		join_note_memory(state);
		return true;
	}
	/* The file larger than the room: pieces, and passes over the outer rows. */
	spill->multipass = spill->build.disk_bytes[partition] > spill_room(state);
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

/* A shared table's partition this participant took alone: split or whole, as its machine says. */
bool
join_take_partition(TessHashJoinState *state, int partition)
{
	JoinSpill  *spill = state->spill;
	TessJoinAction action;

	action = spill_step(state, spill, (TessJoinEvent) {
		.kind = TESS_JOIN_TAKE,
		.partition = (uint32) partition,
		.memory = join_spill_memory(state, spill, NULL),
	});
	return join_open_partition(state, partition,
							   action.kind == TESS_JOIN_SPLIT ? &action.below : NULL);
}

/*
 * The next partition to join, as the level's machine says: those with
 * nothing to join released, then one loaded, or split into a level below.
 * False when none is left.
 */
static bool
next_partition(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	end_partition(state);
	if (spill->shared)
		return join_shared_next_partition(state);
	for (;;)
	{
		TessJoinAction action;
		int			partition;

		action = spill_step(state, spill, (TessJoinEvent) {
			.kind = TESS_JOIN_NEXT,
			.memory = join_spill_memory(state, spill, NULL),
		});
		partition = (int) action.partition;
		switch (action.kind)
		{
			case TESS_JOIN_RELEASE:
				side_release(&spill->build, partition);
				side_release(&spill->probe, partition);
				continue;
			case TESS_JOIN_OPEN:
				return join_open_partition(state, partition, NULL);
			case TESS_JOIN_SPLIT:
				return join_open_partition(state, partition, &action.below);
			default:
				Assert(action.kind == TESS_JOIN_LEVEL_DONE);
				spill->partition = spill->npartitions;
				return false;
		}
	}
}

/*
 * The next batch of the joined partition's outer rows (or of a shared
 * table's outer rows it answers itself, spill->rows): up to
 * JOIN_COMPACT_ROWS rows of the chunk of columns being read, a window of
 * every stored column; the chunks come from the files as pairs with
 * their values, through the one reader. No tail is left in memory by
 * then: each went to disk as its level started joining, a shared table's
 * at OUTER. False when none is left.
 */
bool
join_next_spilled(TessHashJoinState *state, JoinSpill *spill)
{
	for (;;)
	{
		if (spill->block != NULL)
		{
			uint64		total = spill->outer.rows;

			if (spill->cursor < total)
			{
				int			count = (int) Min(total - spill->cursor, JOIN_COMPACT_ROWS);

				spill->bits[0] = count == 64 ? ~UINT64CONST(0) :
					(UINT64CONST(1) << count) - 1;
				spill->batch.rows.nrows = count;
				spill->batch_ordinal = spill->ordinal;
				spill->ordinal += count;
				tess_columns_reader_window(&spill->outer, (uint32) spill->cursor, (uint32) count,
										   spill->values, spill->isnull);
				spill->cursor += count;
				return true;
			}
			/*
			 * The chunk and its values go before the next is read, as a
			 * level counts what its reader holds: no more than one pair.
			 */
			join_outer_forget(spill);
		}
		if (!part_load(&spill->reader, &spill->outer))
		{
			Assert(spill->rows->current[spill->partition] == 0);
			return false;
		}
		spill->block = spill->outer.chunk;
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
	result->values = spill->values[word];
	result->isnull = spill->isnull[word];
	result->nrows = batch->rows.nrows;
}

/*
 * The 64-bit hashes of key key, one a word does not hold, of a batch of
 * outer rows read back, into result: false for another batch.
 */
bool
join_spill_key_hashes(TessBatch *batch, int key, TessDatumColumn *result)
{
	JoinSpill  *spill;
	int			word;

	if (batch->ops != &spill_batch_ops)
		return false;
	spill = (JoinSpill *) batch->private_data;
	word = spill->key_words[key] - 1;
	*result = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
	result->values = spill->values[word];
	result->isnull = spill->isnull[word];
	result->nrows = batch->rows.nrows;
	return true;
}

/*
 * An outer batch, while the table spills: the valid rows of the
 * partitions on disk leave the batch's probe, by the routes the level's
 * machine set. Those of a partition with no inner row, and those the
 * filter of every inner row rejects, have no pair and stay to be answered
 * now; the others are written as rows of their partitions, leave the rows
 * to answer and come back when their partition is joined.
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
			uint8		route = spill->routes[spill_partition(spill,
															  state->probe.hashes[word * 64 + bit])];

			bits &= bits - 1;
			if (route == TESS_JOIN_ROUTE_PROBE)
				continue;
			out |= UINT64CONST(1) << bit;
			if (route == TESS_JOIN_ROUTE_WRITE)
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
 * participant to leave it, which the others leave here, their tail done,
 * unless a participant left while it probed (join_stop_shared): the pairs
 * of the rows it left were never marked, and its plan wanted no more rows.
 */
bool
join_tail_turn(TessHashJoinState *state)
{
	if (!state->preserve_inner || state->tail.table_done)
		return false;
	if (state->marks_shared &&
		(!(state->parallel.round_partition >= 0 ? join_round_depart(state) :
		   join_leave_shared(state, true)) ||
		 join_shared_stopped(state)))
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
					/* Its table first: the build reserves rows for the inner batches. */
					if (state->lookup_key >= 0)
						join_lookup_batch(state, batch);
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
	state->values.lens = NULL;
	state->values.slots = 0;
	state->values.nchunks = 0;
	MemoryContextReset(state->table_context);
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->table_bytes = 0;
	state->bloom.bits = NULL;
	state->bloom.nwords = 0;
}


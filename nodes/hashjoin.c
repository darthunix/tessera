#include "postgres.h"

#include "commands/explain_format.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/optimizer.h"
#include "utils/datum.h"
#include "utils/expandeddatum.h"
#include "utils/lsyscache.h"
#include "storage/barrier.h"
#include "utils/dsa.h"
#include "utils/ruleutils.h"

#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"
#include "hashjoin.h"

/*
 * TessHashJoin joins two batch children on equalities of integer keys.
 * It builds the rows of the inner child into the hash table of the Rust
 * kernels (tessera/table.h), which it reaches through the bridge's kernel
 * registry, and probes the table with the batches of the outer child. A
 * batch it publishes has the physical rows of an outer batch and selects
 * the rows that found a record: the outer columns are the outer batch's
 * own, the inner columns come from the records' payload when a parent asks
 * for them. A key held by several inner rows has as many records, and the
 * outer batch is published once per record (a round). See docs/nodes.md.
 */






/*
 * Replace the values of a key a word does not hold by their 64-bit hashes
 * for the rows, NULL kept.
 */
static void
hash_key_column(TessHashJoinState *state, int key, const TessRowMask *rows,
				TessDatumColumn *column)
{
	FmgrInfo   *hasher = &state->keys.hashers[key];
	Datum	   *values = state->keys.hash_values[key];
	bool	   *isnull = state->keys.hash_isnull[key];
	MemoryContext old = MemoryContextSwitchTo(state->keys.hash_context);
	int			row = -1;

	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		isnull[row] = column->isnull[row];
		if (!isnull[row])
			values[row] = FunctionCall2Coll(hasher, state->keys.collations[key],
											column->values[row], Int64GetDatum(0));
	}
	MemoryContextSwitchTo(old);
	column->values = values;
	column->isnull = isnull;
}

/*
 * The keys of a batch of one side: each key column, read for the selected
 * rows and hashed in key order, the first key's hash folding in the
 * others'; valid gets the rows whose keys are all non-NULL, since an
 * equality with NULL is never true. The table's keys point at the columns.
 */
void
join_batch_keys(TessHashJoinState *state, TessBatch *batch, const int *columns,
		   const TessTableKeyKind *kinds, TessRowMask *valid)
{
	/*
	 * A NULL key never matches; RIGHT and FULL still keep such an inner row
	 * as a record, which no probe finds and the tail returns.
	 */
	TessNullKeys nulls = state->preserve_inner && kinds == state->keys.inner_kinds ?
		TESS_NULL_KEYS_GROUP : TESS_NULL_KEYS_REJECT;

	if (state->keys.hashed_keys)
		MemoryContextReset(state->keys.hash_context);
	for (int key = 0; key < state->keys.nkeys; key++)
	{
		TessDatumColumn *keys = &state->keys.key_columns[key];
		bool		int8 = kinds[key] == TESS_TABLE_KEY_INT8;
		const TessRowMask *rows = key == 0 ? &batch->rows : valid;

		join_child_column(batch, columns[key], rows, TESS_COLUMN_FOR_FILTER, keys);
		if (OidIsValid(state->keys.hashers[key].fn_oid))
			hash_key_column(state, key, rows, keys);
		if (key == 0)
			check(state, (int8 ? state->kernels->int8_hash :
						  state->kernels->int4_hash) (keys, NULL, &batch->rows,
													  nulls,
													  state->probe.hashes, valid,
													  &state->status));
		else
			check(state, (int8 ? state->kernels->int8_hash_next :
						  state->kernels->int4_hash_next) (keys, NULL,
														   nulls,
														   state->probe.hashes, valid,
														   &state->status));
		state->keys.table_keys[key].kind = kinds[key];
		state->keys.table_keys[key].column = keys;
		state->keys.table_keys[key].prepared = NULL;
	}
}

/* A column of a child's batch, checked. */
void
join_child_column(TessBatch *batch, int column, const TessRowMask *rows,
			 TessColumnPurpose purpose, TessDatumColumn *result)
{
	*result = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
	batch->ops->get_datum_column(batch, column, rows, purpose, result);
	if (result->values == NULL || result->isnull == NULL ||
		result->nrows != batch->rows.nrows)
		elog(ERROR, "Tessera batch returned an invalid column");
}

/* Make the buffers hold batches of nrows rows. */
void
join_reserve_rows(TessHashJoinState *state, int nrows)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	int			nwords;

	if (nrows <= state->probe.capacity)
		return;
	nrows = Max(nrows, JOIN_INITIAL_ROWS);
	nwords = tess_row_mask_word_count(nrows);
	if (state->probe.hashes != NULL)
	{
		pfree(state->probe.hashes);
		pfree(state->probe.offsets);
		pfree(state->probe.valid_bits);
		pfree(state->probe.pending_bits);
		pfree(state->probe.payload);
		pfree(state->probe.round_bits);
		pfree(state->probe.next_bits);
		pfree(state->probe.published_bits);
		pfree(state->compact.taken_bits);
		pfree(state->probe.null_words);
		pfree(state->matched_bits);
		pfree(state->active_bits);
		pfree(state->null_values);
		pfree(state->null_isnull);
		for (int word = 0; word < state->npayload; word++)
		{
			pfree(state->probe.inner_values[word]);
			pfree(state->probe.inner_isnull[word]);
		}
		for (int key = 0; key < state->keys.nkeys; key++)
		{
			if (state->keys.hash_values[key] != NULL)
			{
				pfree(state->keys.hash_values[key]);
				pfree(state->keys.hash_isnull[key]);
			}
		}
	}
	state->probe.hashes = MemoryContextAllocZero(context, sizeof(uint32) * nrows);
	state->probe.offsets = MemoryContextAllocZero(context, sizeof(uint32) * nrows);
	state->probe.valid_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->probe.pending_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->probe.payload = MemoryContextAllocZero(context,
											mul_size(sizeof(uint64) * nrows,
													 1 + state->npayload));
	state->probe.round_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->probe.next_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->probe.published_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->compact.taken_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->probe.null_words = MemoryContextAllocZero(context, sizeof(Datum) * nrows);
	state->matched_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->active_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->null_values = MemoryContextAllocZero(context, sizeof(Datum) * nrows);
	state->null_isnull = MemoryContextAlloc(context, sizeof(bool) * nrows);
	memset(state->null_isnull, true, sizeof(bool) * nrows);
	/* Zeroed: rows outside a round are initialized memory, as batches promise. */
	for (int word = 0; word < state->npayload; word++)
	{
		state->probe.inner_values[word] = MemoryContextAllocZero(context,
														   sizeof(Datum) * nrows);
		state->probe.inner_isnull[word] = MemoryContextAllocZero(context,
														   sizeof(bool) * nrows);
	}
	for (int key = 0; key < state->keys.nkeys; key++)
	{
		if (!OidIsValid(state->keys.hashers[key].fn_oid))
			continue;
		state->keys.hash_values[key] = MemoryContextAllocZero(context, sizeof(Datum) * nrows);
		state->keys.hash_isnull[key] = MemoryContextAllocZero(context, sizeof(bool) * nrows);
	}
	state->probe.capacity = nrows;
}

/* The bytes the table, the copies of inner values and spilling take now. */
Size
join_memory(TessHashJoinState *state)
{
	Size		memory = state->table_bytes + sizeof(uint64) * state->bloom.nwords +
		MemoryContextMemAllocated(state->values_context, true);

	if (state->spill != NULL)
		memory += join_spill_memory(state->spill, NULL);
	return memory;
}

void
join_note_memory(TessHashJoinState *state)
{
	state->peak_memory = Max(state->peak_memory, join_memory(state));
}

/*
 * RIGHT and FULL: the table in memory is another, or is built anew: its
 * records have no mark, and its tail is still to come.
 */
void
join_forget_marks(TessHashJoinState *state)
{
	if (state->marks_context != NULL)
		MemoryContextReset(state->marks_context);
	state->marks = NULL;
	state->mark_slots = 0;
	state->mark_chunks = 0;
	state->marks_shared = false;
	state->tail.table_done = false;
}

/* An atomic word of marks is a plain one: none simulated with a lock. */
StaticAssertDecl(sizeof(pg_atomic_uint64) == sizeof(uint64),
				 "TessHashJoin needs 64-bit atomics for shared marks");

/* The words of the marks of a chunk of chunk_len bytes: a bit per record. */
Size
join_marks_of(TessHashJoinState *state, Size chunk_len)
{
	Size		words;

	check(state, state->kernels->table_mark_words(chunk_len, state->record_size, &words,
												  &state->status));
	return words;
}

/* The words of a chunk's marks in shared memory: a bit per record of the largest chunk. */
Size
join_mark_words(TessHashJoinState *state)
{
	return join_marks_of(state, TESS_TABLE_MAX_CHUNK_LEN);
}

/* Room for the bases and lengths of nchunks chunks in this process. */
void
join_reserve_chunks(TessHashJoinState *state, int nchunks)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	int			slots = Max(state->chunk_slots, 16);

	if (nchunks <= state->chunk_slots)
		return;
	while (slots < nchunks)
		slots *= 2;
	if (state->chunk_bases == NULL)
	{
		state->chunk_bases = MemoryContextAlloc(context, sizeof(void *) * slots);
		state->chunk_lens = MemoryContextAlloc(context, sizeof(Size) * slots);
	}
	else
	{
		state->chunk_bases = repalloc(state->chunk_bases, sizeof(void *) * slots);
		state->chunk_lens = repalloc(state->chunk_lens, sizeof(Size) * slots);
	}
	state->chunk_slots = slots;
	state->table.chunks = state->chunk_bases;
	state->table.chunk_lens = state->chunk_lens;
}

/*
 * Take the filter back from the outer child before it goes: a new table
 * decides on its own.
 */
void
join_take_back_bloom(TessHashJoinState *state)
{
	if (!state->bloom.below)
		return;
	(void) tess_input_set_key_filter(state->outer_input, NULL);
	state->bloom.below = false;
}

/* An empty table with no index yet, for a build that appends first. */
static void
create_table(TessHashJoinState *state)
{
	join_take_back_bloom(state);
	MemoryContextReset(state->table_context);
	MemoryContextReset(state->values_context);
	join_reset_values(state);
	join_forget_marks(state);
	/* A new table: decide on its filter again. */
	state->bloom.bits = NULL;
	state->bloom.nwords = 0;
	state->bloom.decided = false;
	state->sample_rows = 0;
	state->sample_found = 0;
	state->table.index = NULL;
	state->table.index_len = 0;
	state->table.nchunks = 0;
	state->table_bytes = 0;
	join_reserve_chunks(state, 1);
	join_note_memory(state);
}

/*
 * The chunks of a table and of its values past the first: up to the
 * largest, and at most an eighth of hash_mem, so that a table spills only
 * near its limit; a shared table's participant's too.
 */
Size
join_chunk_len_for(Size largest)
{
	return Max(JOIN_FIRST_CHUNK,
			   Min(largest, TYPEALIGN_DOWN(8, tess_hash_memory_limit() / 8)));
}

/* Another chunk for the serial table, the last one being full. */
static void
add_table_chunk(TessHashJoinState *state)
{
	int			chunk = state->table.nchunks;
	Size		len = chunk == 0 ? JOIN_FIRST_CHUNK : join_chunk_len_for(JOIN_CHUNK_LEN);
	void	   *base;

	if (chunk == TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin hash table cannot hold more than %d chunks",
						TESS_TABLE_MAX_CHUNKS)));
	join_reserve_chunks(state, chunk + 1);
	base = MemoryContextAlloc(state->table_context, len);
	check(state, state->kernels->table_chunk_init(base, len, &state->status));
	state->chunk_bases[chunk] = base;
	state->chunk_lens[chunk] = len;
	state->table.nchunks++;
	state->table_bytes += len;
	state->counters[JOIN_CHUNKS]++;
}

/*
 * The index for the records appended, made once the inner side is read,
 * and the records linked into it, those of a key next to each other: the
 * rounds step from one to the next, and a table without duplicates has
 * no second round.
 */
void
join_index_table(TessHashJoinState *state)
{
	Size		payload_size = sizeof(uint64) * (1 + state->npayload);
	uint64		capacity = Max(state->build_rows, JOIN_INITIAL_ROWS);
	TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
	Size		size;

	/* One index per build, made once every chunk is appended. */
	Assert(state->table.index == NULL);
	check(state, state->kernels->table_size(state->keys.nkeys, state->keys.inner_kinds,
											payload_size, capacity,
											&size, &state->status));
	state->table.index = MemoryContextAllocExtended(state->table_context, size,
													MCXT_ALLOC_HUGE);
	state->table.index_len = size;
	check(state, state->kernels->table_create(state->table.index, size,
											  state->keys.nkeys, state->keys.inner_kinds,
											  payload_size, capacity,
											  &state->status));
	state->table_bytes += size;
	for (int chunk = 0; chunk < state->table.nchunks; chunk++)
	{
		Size		from = TESS_TABLE_CHUNK_HEADER;
		uint64		duplicates;

		CHECK_FOR_INTERRUPTS();
		check(state, state->kernels->table_link_grouped(&state->table, chunk,
														&from, NULL,
														&duplicates,
														&state->status));
		state->duplicates += duplicates;
	}
	check(state, state->kernels->table_stats(&state->table, &stats,
											 &state->status));
	state->counters[JOIN_BUCKETS] += stats.buckets;
	join_note_memory(state);
}


/* The head of a participant's list of its chunks, or of its value chunks. */
dsa_pointer *
join_participant_list(TessHashJoinState *state, int participant, bool values)
{
	dsa_pointer *heads = dsa_get_address(join_query_dsa(state), state->parallel.shared->lists);

	Assert(participant >= 0 && participant < state->parallel.shared->participants);
	return &heads[2 * participant + (values ? 1 : 0)];
}

dsa_pointer *
join_own_list(TessHashJoinState *state, bool values)
{
	return join_participant_list(state, state->parallel.spill_participant, values);
}

/* The words of a shared table's spilling, mapped in this process. */
uint64 *
join_shared_words(TessHashJoinState *state)
{
	if (state->parallel.spill_words == NULL)
		state->parallel.spill_words = dsa_get_address(join_query_dsa(state),
											 state->parallel.shared->spill_words);
	return state->parallel.spill_words;
}

/* The partitions of a shared table, 0 while it is whole. */
uint32
join_shared_partitions(TessHashJoinState *state)
{
	uint32		partitions;

	check(state, state->kernels->table_spill_partitions(join_shared_words(state),
														state->parallel.shared->spill_nwords,
														&partitions,
														&state->status));
	return partitions;
}

/* Count bytes of this participant's chunks while the table is whole. */
void
join_shared_count(TessHashJoinState *state, int64 delta)
{
	bool		over;

	check(state, state->kernels->table_spill_add_bytes(join_shared_words(state),
													   state->parallel.shared->spill_nwords,
													   true, delta, -1, &over,
													   &state->status));
	state->parallel.spill_over = over;
}

bool
join_shared_on_disk(TessHashJoinState *state, uint32 partition)
{
	bool		on_disk;
	bool		alone;

	check(state, state->kernels->table_spill_flags(join_shared_words(state),
												   state->parallel.shared->spill_nwords,
												   partition, &on_disk, &alone,
												   &state->status));
	return on_disk;
}

/* Room for the bases of nchunks value chunks in this process. */
void
join_reserve_values(TessHashJoinState *state, int nchunks)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	int			slots = Max(state->values.slots, 16);

	if (nchunks <= state->values.slots)
		return;
	while (slots < nchunks)
		slots *= 2;
	state->values.bases = state->values.bases == NULL ?
		MemoryContextAllocZero(context, sizeof(char *) * slots) :
		repalloc0(state->values.bases, sizeof(char *) * state->values.slots,
				  sizeof(char *) * slots);
	state->values.slots = slots;
}

/* Forget the value chunks, which went with the table. */
void
join_reset_values(TessHashJoinState *state)
{
	state->values.nchunks = 0;
	state->values.current = -1;
	state->values.len = 0;
	state->values.used = 0;
	state->values.own = 0;
	state->values.bytes = 0;
}

/*
 * A value chunk of len bytes: in the node's memory, or in the query's
 * shared memory, numbered under the lock and entered in the table's list.
 * Returns its number.
 */
static int
new_value_chunk(TessHashJoinState *state, Size len)
{
	int			number;
	char	   *base;

	if (state->parallel.shared != NULL)
	{
		dsa_area   *area = join_query_dsa(state);
		dsa_pointer block = dsa_allocate_extended(area, add_size(JOIN_CHUNK_HEADER, len),
												  DSA_ALLOC_HUGE);
		JoinChunk  *header = dsa_get_address(area, block);

		SpinLockAcquire(&state->parallel.shared->lock);
		number = (int) state->parallel.shared->next_value_chunk++;
		SpinLockRelease(&state->parallel.shared->lock);
		header->next = *join_own_list(state, true);
		*join_own_list(state, true) = block;
		header->number = number;
		header->len = len;
		header->owner = state->parallel.spill_participant;
		base = (char *) header + JOIN_CHUNK_HEADER;
		state->values.bytes = add_size(state->values.bytes, JOIN_CHUNK_HEADER);
		join_shared_count(state, (int64) (JOIN_CHUNK_HEADER + len));
	}
	else
	{
		number = state->values.nchunks;
		base = MemoryContextAllocExtended(state->values_context, len, MCXT_ALLOC_HUGE);
	}
	if (number >= INT_MAX - 1)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin cannot hold more chunks of values")));
	join_reserve_values(state, number + 1);
	state->values.bases[number] = base;
	state->values.nchunks = Max(state->values.nchunks, number + 1);
	state->values.own++;
	state->values.bytes = add_size(state->values.bytes, len);
	return number;
}

/*
 * Copy a by-reference inner value into the table's value chunks and
 * return its reference: the bytes datumCopy would copy, an expanded
 * object flattened.
 */
uint64
join_store_value(TessHashJoinState *state, Datum value, int16 typlen)
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
	if (aligned > JOIN_VALUE_CHUNK / 4)
	{
		number = new_value_chunk(state, aligned);
		byte = 0;
	}
	else
	{
		if (state->values.current < 0 ||
			state->values.used + aligned > state->values.len)
		{
			Size		len = state->values.own == 0 ? JOIN_VALUE_FIRST :
				join_chunk_len_for(JOIN_VALUE_CHUNK);

			state->values.len = Max(len, aligned);
			state->values.current = new_value_chunk(state, state->values.len);
			state->values.used = 0;
		}
		number = state->values.current;
		byte = state->values.used;
		state->values.used += aligned;
	}
	if (expanded != NULL)
		EOH_flatten_into(expanded, state->values.bases[number] + byte, size);
	else
		memcpy(state->values.bases[number] + byte, DatumGetPointer(value), size);
	return JOIN_VALUE_REF(number, byte);
}

/*
 * The payload of every valid row: the NULL bits of the kept inner
 * columns, then each value, a by-reference one copied into the node's
 * memory, where it lives as long as the table.
 */
static void
fill_payload(TessHashJoinState *state, TessBatch *batch, const TessRowMask *valid)
{
	int			width = 1 + state->npayload;
	int			row = -1;

	while ((row = tess_row_mask_next(valid, row)) >= 0)
		state->probe.payload[row * width] = 0;
	/* The columns' NULL bits, gathered below for the whole table. */
	for (int word = 0; word < state->npayload; word++)
	{
		int			scan_column = state->payload_columns[word];
		int16		typlen = state->typlens[scan_column];
		bool		byval = state->typbyvals[scan_column];
		TessDatumColumn values;

		join_child_column(batch, state->child_columns[scan_column], valid,
					 TESS_COLUMN_FOR_PROJECTION, &values);
		row = -1;
		while ((row = tess_row_mask_next(valid, row)) >= 0)
		{
			uint64	   *record = &state->probe.payload[row * width];

			if (values.isnull[row])
			{
				record[0] |= UINT64CONST(1) << word;
				record[1 + word] = 0;
				state->null_columns |= UINT64CONST(1) << word;
			}
			else if (byval)
				record[1 + word] = values.values[row];
			else
				record[1 + word] = join_store_value(state, values.values[row], typlen);
		}
	}
}

/*
 * Append the pending rows of an inner batch to chunk `chunk` of table:
 * false when it had room for none of them.
 */
bool
join_append_rows(TessHashJoinState *state, const TessTableRef *table, int chunk,
			TessRowMask *pending)
{
	int			before = tess_row_mask_count(pending);

	check(state, state->kernels->table_append(table, chunk,
											  sizeof(uint64) * (1 + state->npayload),
											  state->probe.hashes, state->keys.nkeys,
											  state->keys.table_keys,
											  (const uint8 *) state->probe.payload,
											  pending, state->probe.offsets,
											  &state->status));
	return tess_row_mask_count(pending) < before;
}

/*
 * The valid rows of an inner batch, with their payload filled, into
 * pending: their count.
 */
int
join_prepare_inner(TessHashJoinState *state, TessBatch *batch, TessRowMask *pending)
{
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask valid;
	int			count;

	join_reserve_rows(state, nrows);
	/* A shorter batch than the last: no bits past its rows may remain. */
	memset(state->probe.valid_bits, 0, sizeof(uint64) * nwords);
	valid = (TessRowMask) {nrows, state->probe.valid_bits};
	*pending = (TessRowMask) {nrows, state->probe.pending_bits};
	join_batch_keys(state, batch, state->keys.inner_keys, state->keys.inner_kinds, &valid);
	count = tess_row_mask_count(&valid);
	if (count == 0)
		return 0;
	fill_payload(state, batch, &valid);
	memcpy(state->probe.pending_bits, state->probe.valid_bits, sizeof(uint64) * nwords);
	return count;
}

/*
 * Append the rows of one inner batch, adding chunks until they fit; once
 * the table outgrows hash_mem, it spills, and the rows go into its
 * partitions.
 */
static void
insert_batch(TessHashJoinState *state, TessBatch *batch)
{
	TessRowMask pending;
	int			count;
	bool		fresh = false;

	/* A batch adds a chunk of records and one of values at most. */
	if (state->spill == NULL &&
		state->table_bytes + state->values.bytes +
		2 * join_chunk_len_for(JOIN_CHUNK_LEN) > tess_hash_memory_limit())
		join_start_spill(state);
	if (state->spill != NULL)
	{
		join_insert_spill(state, batch);
		return;
	}
	count = join_prepare_inner(state, batch, &pending);
	if (count == 0)
		return;
	if (state->table.nchunks == 0)
	{
		add_table_chunk(state);
		fresh = true;
	}
	for (;;)
	{
		bool		appended = join_append_rows(state, &state->table,
										   state->table.nchunks - 1, &pending);

		if (tess_row_mask_count(&pending) == 0)
			break;
		if (!appended && fresh)
			elog(ERROR, "TessHashJoin cannot fit a row of its table in a chunk");
		add_table_chunk(state);
		fresh = true;
	}
	state->build_rows += count;
	state->counters[JOIN_BUILD_ROWS] += count;
	join_note_memory(state);
}

/* This build's keys for the outer side's pruning: none seen yet. */
void
join_reset_prune_keys(TessHashJoinState *state)
{
	state->prune.keys.rows = 0;
	state->prune.keys.min = PG_INT64_MAX;
	state->prune.keys.max = PG_INT64_MIN;
	state->prune.keys.nvalues = 0;
	state->prune.sent = false;
}

/*
 * The keys of an inner batch's rows for the outer side's pruning: the
 * lowest and the highest, and the keys themselves while the build has at
 * most JOIN_PRUNE_VALUES rows with one. A NULL key pairs with nothing.
 */
void
join_note_prune_keys(TessHashJoinState *state, TessBatch *batch)
{
	TessJoinKeys *keys = &state->prune.keys;
	TessDatumColumn column;
	int			row = -1;

	if (!state->prune.on)
		return;
	join_child_column(batch, state->keys.inner_keys[state->prune.key], &batch->rows,
				 TESS_COLUMN_FOR_FILTER, &column);
	while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
	{
		int64		value;

		if (column.isnull[row])
			continue;
		value = keys->int8 ? DatumGetInt64(column.values[row]) :
			(int64) DatumGetInt32(column.values[row]);
		keys->min = Min(keys->min, value);
		keys->max = Max(keys->max, value);
		keys->rows++;
		if (keys->nvalues >= 0 && keys->nvalues < JOIN_PRUNE_VALUES)
			keys->values[keys->nvalues++] = value;
		else
			keys->nvalues = -1;
	}
}

/*
 * A shared build: this participant's keys added to every participant's,
 * whole once the build's barrier is passed.
 */
void
join_share_prune_keys(TessHashJoinState *state)
{
	TessJoinKeys *keys = &state->prune.keys;
	JoinShared *shared = state->parallel.shared;

	if (!state->prune.on || keys->rows == 0)
		return;
	SpinLockAcquire(&shared->prune_lock);
	shared->prune_rows += keys->rows;
	shared->prune_min = Min(shared->prune_min, keys->min);
	shared->prune_max = Max(shared->prune_max, keys->max);
	if (shared->prune_nvalues >= 0 &&
		(keys->nvalues < 0 || shared->prune_nvalues + keys->nvalues > JOIN_PRUNE_VALUES))
		shared->prune_nvalues = -1;
	else if (shared->prune_nvalues >= 0)
	{
		memcpy(&shared->prune_values[shared->prune_nvalues], keys->values,
			   sizeof(int64) * keys->nvalues);
		shared->prune_nvalues += keys->nvalues;
	}
	SpinLockRelease(&shared->prune_lock);
}

/*
 * The table built: the outer side's partitions its keys cannot pair with
 * are pruned before the outer node is read, once a build (a shared table's
 * keys are every participant's); a table kept over a rescan keeps them.
 */
void
join_prune_outer(TessHashJoinState *state)
{
	if (!state->prune.on || state->prune.sent)
		return;
	if (state->parallel.shared != NULL)
	{
		JoinShared *shared = state->parallel.shared;
		TessJoinKeys *keys = &state->prune.keys;

		SpinLockAcquire(&shared->prune_lock);
		keys->rows = shared->prune_rows;
		keys->min = shared->prune_min;
		keys->max = shared->prune_max;
		keys->nvalues = shared->prune_nvalues;
		if (keys->nvalues > 0)
			memcpy(keys->values, shared->prune_values, sizeof(int64) * keys->nvalues);
		SpinLockRelease(&shared->prune_lock);
	}
	tess_append_join_prune(state->outer, &state->prune.keys);
	state->prune.sent = true;
}

/* Read every batch of the inner child into a new table. */
static void
build_table(TessHashJoinState *state)
{
	join_spill_free(state);
	create_table(state);
	/* Another table, maybe with other duplicates: decide compact mode again. */
	state->compact_decided = false;
	state->build_rows = 0;
	state->duplicates = 0;
	state->null_columns = 0;
	/* A column without NULLs is never gathered for them: no flag may stay set. */
	if (state->probe.inner_isnull != NULL && state->probe.capacity > 0)
		for (int word = 0; word < state->npayload; word++)
			memset(state->probe.inner_isnull[word], 0, sizeof(bool) * state->probe.capacity);
	join_reset_prune_keys(state);
	for (;;)
	{
		TessBatch  *batch = tess_input_next(state->inner_input);

		if (batch == NULL)
			break;
		if (tess_row_mask_count(&batch->rows) > 0)
		{
			join_note_prune_keys(state, batch);
			insert_batch(state, batch);
		}
		tess_input_finish(state->inner_input);
	}
	if (state->spill != NULL)
		join_finish_spill_build(state);
	else
		join_index_table(state);
	state->counters[JOIN_BUILDS]++;
	state->built = true;
}


/*
 * The inner column kept in payload word `word`, for the rows of the
 * round: the records' NULL bits once per round, then the column's word.
 * A parent asks with a subset of the round's rows, which the whole round
 * covers.
 */
static void
gather_inner(TessHashJoinState *state, int word)
{
	TessRowMask round = {state->batch.rows.nrows, state->current_bits};
	bool		nullable = (state->null_columns >> word) & 1;
	int			row = -1;

	if (state->probe.gathered[word])
		return;
	/* A column no inner row left NULL keeps its flags false. */
	if (nullable && !state->probe.nulls_gathered)
	{
		check(state, state->kernels->table_gather(&state->table,
												  state->current_offsets, &round, 0,
												  state->probe.null_words,
												  &state->status));
		state->probe.nulls_gathered = true;
	}
	check(state, state->kernels->table_gather(&state->table,
											  state->current_offsets, &round,
											  sizeof(uint64) * (1 + word),
											  state->probe.inner_values[word],
											  &state->status));
	/* A by-reference value's word is its reference: its address here. */
	if (!state->typbyvals[state->payload_columns[word]])
	{
		Datum	   *values = state->probe.inner_values[word];

		while ((row = tess_row_mask_next(&round, row)) >= 0)
		{
			uint64		ref = DatumGetUInt64(values[row]);

			if (ref == 0)
				continue;
			Assert((ref >> 32) - 1 < (uint64) state->values.nchunks);
			values[row] = PointerGetDatum(state->values.bases[(ref >> 32) - 1] +
										  (ref & 0xFFFFFFFF));
		}
		row = -1;
	}
	if (nullable)
		while ((row = tess_row_mask_next(&round, row)) >= 0)
			state->probe.inner_isnull[word][row] =
				(DatumGetUInt64(state->probe.null_words[row]) >> word) & 1;
	state->probe.gathered[word] = true;
}

/*
 * A column of the published batch: an outer column is the outer batch's
 * own, an inner one is gathered from the round's records.
 */
static void
join_get_column(TessBatch *batch, int column, const TessRowMask *rows,
				TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessHashJoinState *state = (TessHashJoinState *) batch->private_data;
	int			word;

	if (column < 0 || column >= state->ncolumns)
		elog(ERROR, "TessHashJoin has no column %d", column);
	if (state->sides[column] == JOIN_SIDE_OUTER && state->output_compact)
	{
		if (state->compact.values[column] == NULL)
			elog(ERROR, "TessHashJoin column %d was not requested", column);
		result->values = state->compact.values[column];
		result->isnull = state->compact.isnull[column];
		result->nrows = batch->rows.nrows;
		return;
	}
	/* The tail: inner rows without a pair, their outer columns NULL. */
	if (state->sides[column] == JOIN_SIDE_OUTER && state->tail.on)
	{
		result->values = state->null_values;
		result->isnull = state->null_isnull;
		result->nrows = batch->rows.nrows;
		return;
	}
	if (state->sides[column] == JOIN_SIDE_OUTER)
	{
		TessBatch  *outer = state->outer_batch;

		outer->ops->get_datum_column(outer, state->child_columns[column], rows,
									 purpose, result);
		return;
	}
	word = state->payload_words[column];
	if (word == 0)
		elog(ERROR, "TessHashJoin column %d was not requested", column);
	/* LEFT: the rows without a match have NULL inner columns. */
	if (state->null_round)
	{
		result->values = state->null_values;
		result->isnull = state->null_isnull;
		result->nrows = batch->rows.nrows;
		return;
	}
	gather_inner(state, word - 1);
	result->values = state->probe.inner_values[word - 1];
	result->isnull = state->probe.inner_isnull[word - 1];
	result->nrows = batch->rows.nrows;
}

const TessBatchOps join_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = join_get_column,
};



/*
 * Offer the filter to the outer child, which may check its rows against it
 * before its costlier work (TessFilter's row-wise clauses): only when a
 * row without a pair leaves the join's output, as INNER and SEMI drop it,
 * and a RIGHT join, which runs as INNER keeping its inner side.
 * A child that takes it passes only the rows the filter lets through, and
 * the join checks no more.
 */
static void
hand_down_bloom(TessHashJoinState *state)
{
	TessKeyFilter filter = TESS_STRUCT_INITIALIZER(TessKeyFilter);

	/*
	 * A table that spills: the filter knows only the resident rows. A
	 * hashed key: the scan below has the value, not its hash.
	 */
	if ((state->jointype != JOIN_INNER && state->jointype != JOIN_SEMI) ||
		state->spill != NULL || state->keys.hashed_keys)
		return;
	filter.nkeys = state->keys.nkeys;
	filter.columns = state->keys.outer_keys;
	filter.kinds = state->keys.outer_kinds;
	filter.words = state->bloom.bits;
	filter.nwords = state->bloom.nwords;
	filter.shared = state->bloom.shared;
	state->bloom.below = tess_input_set_key_filter(state->outer_input, &filter);
	if (state->bloom.below)
		state->counters[JOIN_BLOOM_BELOW]++;
}

/*
 * After the first JOIN_BLOOM_SAMPLE valid probe rows, a Bloom filter of
 * the table's keys when most of them found no record and the table is
 * past the cache: from then on a row the filter rejects skips the table.
 */
static void
decide_bloom(TessHashJoinState *state, uint64 rows, uint64 found)
{
	double		ratio = tess_join_bloom_ratio;

	state->sample_rows += rows;
	state->sample_found += found;
	/* At 1 the filter comes at once, whatever the sizes; at 0 never. */
	if (ratio < 1.0 && state->sample_rows < JOIN_BLOOM_SAMPLE)
		return;
	state->bloom.decided = true;
	if (!join_bloom_wanted(ratio, (double) state->build_rows, (double) state->sample_found,
						   (double) state->sample_rows))
		return;
	/*
	 * A shared table's filter: the first participant that wants it builds
	 * it for all; until it is ready, the others probe without it. The
	 * elected participant allocated none for a table that may not want
	 * one, which the same rule decides.
	 */
	if (state->parallel.shared != NULL)
	{
		bool		built;

		if (!DsaPointerIsValid(state->parallel.shared->filter))
			return;
		state->bloom.bits = dsa_get_address(join_query_dsa(state), state->parallel.shared->filter);
		state->bloom.nwords = state->parallel.shared->filter_words;
		state->bloom.shared = true;
		check(state, state->kernels->table_try_build_bloom(&state->table,
														   state->bloom.bits,
														   state->bloom.nwords,
														   &built, &state->status));
		if (built)
			state->counters[JOIN_BLOOM_FILTERS]++;
		hand_down_bloom(state);
		return;
	}
	check(state, state->kernels->table_bloom_words(state->build_rows,
												   &state->bloom.nwords,
												   &state->status));
	state->bloom.bits = MemoryContextAllocExtended(state->table_context,
											  mul_size(sizeof(uint64),
													   state->bloom.nwords),
											  MCXT_ALLOC_HUGE);
	check(state, state->kernels->table_bloom(&state->table,
											 state->bloom.bits, state->bloom.nwords,
											 &state->status));
	state->counters[JOIN_BLOOM_FILTERS]++;
	join_note_memory(state);
	hand_down_bloom(state);
}

/*
 * Probe the table with one outer batch: the rows whose key found a record
 * become the first round, after the Bloom filter, when there is one, let
 * them through. False when none did.
 */
bool
join_probe_batch(TessHashJoinState *state, TessBatch *batch)
{
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask valid;
	TessRowMask found;
	TessRowMask passed;
	int			count;
	int			matches;

	join_reserve_rows(state, nrows);
	/* A shorter batch than the last: no bits past its rows may remain. */
	memset(state->probe.valid_bits, 0, sizeof(uint64) * nwords);
	memset(state->probe.round_bits, 0, sizeof(uint64) * nwords);
	valid = (TessRowMask) {nrows, state->probe.valid_bits};
	found = (TessRowMask) {nrows, state->probe.round_bits};

	join_batch_keys(state, batch, state->keys.outer_keys, state->keys.outer_kinds, &valid);
	count = tess_row_mask_count(&valid);
	if (count == 0)
		return false;
	/* The rows of the partitions on disk wait for their partition. */
	if (state->spill != NULL && !state->spill->joining)
	{
		join_spill_outer(state, batch, &valid);
		count = tess_row_mask_count(&valid);
		if (count == 0)
			return false;
	}
	/*
	 * Passes over a partition's outer rows: the last has no table, and a
	 * semi or anti join's row that found a pair in an earlier one is done.
	 */
	if (state->spill != NULL && state->spill->multipass &&
		batch == &state->spill->batch)
	{
		if (state->spill->final_pass)
			return false;
		if (state->jointype == JOIN_SEMI || state->jointype == JOIN_ANTI)
		{
			valid.bits[0] &= ~join_matched_word(state->spill, nrows);
			count = tess_row_mask_count(&valid);
			if (count == 0)
				return false;
		}
	}
	if (state->bloom.bits != NULL && !state->bloom.below && state->bloom.shared &&
		!state->bloom.ready)
		check(state, state->kernels->bloom_shared_ready(state->bloom.bits, state->bloom.nwords,
														&state->bloom.ready,
														&state->status));
	if (state->bloom.bits != NULL && !state->bloom.below &&
		(!state->bloom.shared || state->bloom.ready))
	{
		int			through;

		/* The kernel fills the mask whole, but checks it is a mask of nrows. */
		memset(state->probe.pending_bits, 0, sizeof(uint64) * nwords);
		passed = (TessRowMask) {nrows, state->probe.pending_bits};
		if (state->bloom.shared)
			check(state, state->kernels->bloom_shared_probe(state->bloom.bits,
															state->bloom.nwords,
															state->probe.hashes, &valid,
															&passed, &state->status));
		else
			check(state, state->kernels->bloom_probe(state->bloom.bits, state->bloom.nwords,
													 state->probe.hashes, &valid, &passed,
													 &state->status));
		through = tess_row_mask_count(&passed);
		state->counters[JOIN_BLOOM_REMOVED] += count - through;
		if (through == 0)
			return false;
		valid = passed;
	}
	check(state, state->kernels->table_probe(&state->table,
											 state->probe.hashes, state->keys.nkeys,
											 state->keys.table_keys, &valid,
											 state->probe.offsets, &found,
											 &state->status));
	matches = tess_row_mask_count(&found);
	if (!state->bloom.decided)
		decide_bloom(state, count, matches);
	return matches > 0;
}








/* Publish each round to a batch-aware parent, which finishes it there. */
static TupleTableSlot *
exec_batches(TessHashJoinState *state)
{
	/* Releasing a projection's wrapper forgets the pairs, which stay the node's. */
	tess_output_release(state->output);
	if (!join_next_output(state))
	{
		state->done = true;
		return NULL;
	}
	state->published = state->projection == NULL ? &state->batch :
		tess_projection_wrap(state->projection, &state->batch);
	return tess_output_publish(state->output, state->published);
}

/* Row mode: the column of every slot attribute, for the whole round. */
static void
fetch_columns(TessHashJoinState *state)
{
	int			natts = state->css.ss.ps.ps_ResultTupleSlot->tts_tupleDescriptor->natts;

	for (int attribute = 0; attribute < natts; attribute++)
	{
		TessDatumColumn *column = &state->columns[attribute];

		*column = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
		state->published->ops->get_datum_column(state->published,
												tess_layout_column(&state->layout, attribute),
												&state->published->rows,
												TESS_COLUMN_FOR_PROJECTION, column);
		if (column->values == NULL || column->isnull == NULL ||
			column->nrows != state->batch.rows.nrows)
			elog(ERROR, "Tessera batch returned an invalid column");
	}
}

/* Serve the rows of each round from the node's own slot. */
static TupleTableSlot *
exec_rows(TessHashJoinState *state)
{
	TupleTableSlot *slot = state->css.ss.ps.ps_ResultTupleSlot;
	int			natts = slot->tts_tupleDescriptor->natts;
	int			row;

	for (;;)
	{
		if (!state->serving)
		{
			/* The previous round's wrapper, and its computed values, go now. */
			if (state->published != NULL && state->published != &state->batch)
				state->published->ops->release(state->published);
			state->published = NULL;
			if (!join_next_output(state))
			{
				state->done = true;
				return NULL;
			}
			state->published = state->projection == NULL ? &state->batch :
				tess_projection_wrap(state->projection, &state->batch);
			fetch_columns(state);
			state->next_row = tess_row_mask_next(&state->batch.rows, -1);
			state->serving = true;
		}
		if (state->next_row >= 0)
			break;
		state->serving = false;
	}
	row = state->next_row;
	state->next_row = tess_row_mask_next(&state->batch.rows, row);
	ExecClearTuple(slot);
	for (int attribute = 0; attribute < natts; attribute++)
	{
		slot->tts_values[attribute] = state->columns[attribute].values[row];
		slot->tts_isnull[attribute] = state->columns[attribute].isnull[row];
	}
	return ExecStoreVirtualTuple(slot);
}

/*
 * The children's requests, from the parent's: the outer columns asked for
 * come from the outer batches, the inner ones are kept in the payload, and
 * each child also gives its key. A row-wise parent reads every column of
 * the result slot.
 */
static void
send_requests(TessHashJoinState *state)
{
	TessRequest outer_request = TESS_STRUCT_INITIALIZER(TessRequest);
	TessRequest inner_request = TESS_STRUCT_INITIALIZER(TessRequest);
	const TessRequest *request = tess_output_request(state->output);
	Bitmapset  *needed = bms_union(request->filter_columns,
								   request->projection_columns);
	Bitmapset  *outer_columns = NULL;
	Bitmapset  *inner_columns = NULL;
	Bitmapset  *outer_key = NULL;
	Bitmapset  *inner_key = NULL;
	int			column = -1;

	for (int key = 0; key < state->keys.nkeys; key++)
	{
		outer_key = bms_add_member(outer_key, state->keys.outer_keys[key]);
		inner_key = bms_add_member(inner_key, state->keys.inner_keys[key]);
	}
	/*
	 * The residual clauses read their columns of the pairs too, and an outer
	 * join's filters theirs of the rows it returns, a column no parent may
	 * ask for, as in WHERE inner.c IS NULL above a left join.
	 */
	if (state->qual != NULL)
		needed = bms_add_members(needed, tess_qual_columns(state->qual));
	if (state->filter != NULL)
		needed = bms_add_members(needed, tess_qual_columns(state->filter));
	if (request->output_mode == TESS_OUTPUT_ROWS)
	{
		int			natts = state->css.ss.ps.ps_ResultTupleSlot->tts_tupleDescriptor->natts;

		for (int attribute = 0; attribute < natts; attribute++)
			needed = bms_add_member(needed,
									tess_layout_column(&state->layout, attribute));
	}
	/* A computed column is the node's: the pairs give the columns it reads. */
	foreach_node(TargetEntry, entry, state->computed)
	{
		int			target = state->ncolumns + foreach_current_index(entry);

		if (!bms_is_member(target, needed))
			continue;
		needed = bms_del_member(needed, target);
		foreach_node(Var, var, pull_var_clause((Node *) entry->expr, 0))
			needed = bms_add_member(needed, var->varattno - 1);
	}
	state->npayload = 0;
	while ((column = bms_next_member(needed, column)) >= 0)
	{
		if (column >= state->ncolumns)
			elog(ERROR, "TessHashJoin has no column %d", column);
		if (state->sides[column] == JOIN_SIDE_OUTER)
		{
			outer_columns = bms_add_member(outer_columns,
										   state->child_columns[column]);
			state->compact.outer_columns[state->compact.nouter++] = column;
			continue;
		}
		if (state->npayload == JOIN_MAX_PAYLOAD)
			elog(ERROR, "TessHashJoin keeps at most %d inner columns",
				 JOIN_MAX_PAYLOAD);
		inner_columns = bms_add_member(inner_columns, state->child_columns[column]);
		state->payload_columns[state->npayload] = column;
		state->payload_words[column] = ++state->npayload;
	}
	/* A record: its header, a slot per key and the payload's words. */
	check(state, state->kernels->table_record_size(state->keys.nkeys,
												   sizeof(uint64) * (1 + state->npayload),
												   &state->record_size, &state->status));
	/* The keys before any other column, then the rows that survive them. */
	outer_request.filter_columns = outer_key;
	outer_request.projection_columns = outer_columns;
	outer_request.output_mode = TESS_OUTPUT_BATCH;
	outer_request.max_batch_rows = request->max_batch_rows;
	tess_input_set_request(state->outer_input, &outer_request);
	inner_request.filter_columns = inner_key;
	inner_request.projection_columns = inner_columns;
	inner_request.output_mode = TESS_OUTPUT_BATCH;
	tess_input_set_request(state->inner_input, &inner_request);
	state->probe.inner_values = palloc0_array(Datum *, Max(state->npayload, 1));
	state->probe.inner_isnull = palloc0_array(bool *, Max(state->npayload, 1));
	state->probe.gathered = palloc0_array(bool, Max(state->npayload, 1));
	state->request = request;
}




/*
 * Compact mode for a batch-aware parent over a table with duplicate keys;
 * a by-reference outer value is copied, since it must outlive its outer
 * batch.
 */
void
join_decide_compact(TessHashJoinState *state)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;

	state->compact_decided = true;
	state->compact.on = false;
	/*
	 * SEMI and ANTI return outer rows, not pairs; LEFT with join clauses
	 * must see each round whole to know the rows without a match.
	 */
	if (state->request->output_mode != TESS_OUTPUT_BATCH ||
		state->inner_unique || state->duplicates == 0 ||
		state->jointype == JOIN_SEMI || state->jointype == JOIN_ANTI ||
		(state->jointype == JOIN_LEFT && state->qual != NULL))
		return;
	for (int index = 0; index < state->compact.nouter; index++)
	{
		int			column = state->compact.outer_columns[index];

		if (state->compact.values[column] != NULL)
			continue;
		state->compact.values[column] =
			MemoryContextAllocZero(context, sizeof(Datum) * JOIN_COMPACT_ROWS);
		state->compact.isnull[column] =
			MemoryContextAllocZero(context, sizeof(bool) * JOIN_COMPACT_ROWS);
	}
	/* The copies of by-reference values, for one compact batch at a time. */
	if (state->compact_context == NULL)
		state->compact_context = AllocSetContextCreate(context,
													   "TessHashJoin compact values",
													   ALLOCSET_DEFAULT_SIZES);
	state->compact.on = true;
}

TupleTableSlot *
join_exec(CustomScanState *css)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	if (state->done)
		return NULL;
	if (state->request == NULL)
		send_requests(state);
	if (!state->built)
	{
		if (state->parallel.shared != NULL)
			join_build_shared(state);
		else
			build_table(state);
		if (state->done)
			return NULL;
	}
	join_prune_outer(state);
	/*
	 * Nothing to match: the outer child is never read, as in the core,
	 * unless its rows go out without a match (LEFT, ANTI).
	 */
	if ((state->spill != NULL ? state->spill->total_rows : state->build_rows) == 0 &&
		(state->jointype == JOIN_INNER || state->jointype == JOIN_SEMI))
	{
		state->done = true;
		return NULL;
	}
	if (!state->compact_decided)
		join_decide_compact(state);
	return state->request->output_mode == TESS_OUTPUT_BATCH ?
		exec_batches(state) : exec_rows(state);
}














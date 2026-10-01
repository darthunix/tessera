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


static const CustomExecMethods join_exec_methods;




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
batch_keys(TessHashJoinState *state, TessBatch *batch, const int *columns,
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

		child_column(batch, columns[key], rows, TESS_COLUMN_FOR_FILTER, keys);
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
child_column(TessBatch *batch, int column, const TessRowMask *rows,
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
reserve_rows(TessHashJoinState *state, int nrows)
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
		memory += spill_memory(state->spill, NULL);
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
forget_marks(TessHashJoinState *state)
{
	if (state->marks_context != NULL)
		MemoryContextReset(state->marks_context);
	state->marks = NULL;
	state->mark_slots = 0;
	state->marks_shared = false;
	state->tail.table_done = false;
}

/* An atomic word of marks is a plain one: none simulated with a lock. */
StaticAssertDecl(sizeof(pg_atomic_uint64) == sizeof(uint64),
				 "TessHashJoin needs 64-bit atomics for shared marks");

/* The words of a chunk's marks in shared memory: a bit per record of the largest chunk. */
Size
mark_words(TessHashJoinState *state)
{
	return ((TESS_TABLE_MAX_CHUNK_LEN - TESS_TABLE_CHUNK_HEADER) / state->record_size + 63) / 64;
}

/* Room for the bases and lengths of nchunks chunks in this process. */
void
reserve_chunks(TessHashJoinState *state, int nchunks)
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
take_back_bloom(TessHashJoinState *state)
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
	take_back_bloom(state);
	MemoryContextReset(state->table_context);
	MemoryContextReset(state->values_context);
	reset_values(state);
	forget_marks(state);
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
	reserve_chunks(state, 1);
	join_note_memory(state);
}

/*
 * The chunks of a table and of its values past the first: up to the
 * largest, and at most an eighth of hash_mem, so that a table spills only
 * near its limit; a shared table's participant's too.
 */
Size
chunk_len_for(Size largest)
{
	return Max(JOIN_FIRST_CHUNK,
			   Min(largest, TYPEALIGN_DOWN(8, get_hash_memory_limit() / 8)));
}

/* Another chunk for the serial table, the last one being full. */
static void
add_table_chunk(TessHashJoinState *state)
{
	int			chunk = state->table.nchunks;
	Size		len = chunk == 0 ? JOIN_FIRST_CHUNK : chunk_len_for(JOIN_CHUNK_LEN);
	void	   *base;

	if (chunk == TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin hash table cannot hold more than %d chunks",
						TESS_TABLE_MAX_CHUNKS)));
	reserve_chunks(state, chunk + 1);
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
index_table(TessHashJoinState *state)
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
participant_list(TessHashJoinState *state, int participant, bool values)
{
	dsa_pointer *heads = dsa_get_address(query_dsa(state), state->parallel.shared->lists);

	Assert(participant >= 0 && participant < state->parallel.shared->participants);
	return &heads[2 * participant + (values ? 1 : 0)];
}

dsa_pointer *
own_list(TessHashJoinState *state, bool values)
{
	return participant_list(state, state->parallel.spill_participant, values);
}

/* The words of a shared table's spilling, mapped in this process. */
uint64 *
shared_words(TessHashJoinState *state)
{
	if (state->parallel.spill_words == NULL)
		state->parallel.spill_words = dsa_get_address(query_dsa(state),
											 state->parallel.shared->spill_words);
	return state->parallel.spill_words;
}

/* The partitions of a shared table, 0 while it is whole. */
uint32
shared_partitions(TessHashJoinState *state)
{
	uint32		partitions;

	check(state, state->kernels->table_spill_partitions(shared_words(state),
														state->parallel.shared->spill_nwords,
														&partitions,
														&state->status));
	return partitions;
}

/* Count bytes of this participant's chunks while the table is whole. */
void
shared_count(TessHashJoinState *state, int64 delta)
{
	bool		over;

	check(state, state->kernels->table_spill_add_bytes(shared_words(state),
													   state->parallel.shared->spill_nwords,
													   delta, -1, &over,
													   &state->status));
	state->parallel.spill_over = over;
}

bool
shared_on_disk(TessHashJoinState *state, uint32 partition)
{
	bool		on_disk;
	bool		alone;

	check(state, state->kernels->table_spill_flags(shared_words(state),
												   state->parallel.shared->spill_nwords,
												   partition, &on_disk, &alone,
												   &state->status));
	return on_disk;
}

/* Room for the bases of nchunks value chunks in this process. */
void
reserve_values(TessHashJoinState *state, int nchunks)
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
reset_values(TessHashJoinState *state)
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
		dsa_area   *area = query_dsa(state);
		dsa_pointer block = dsa_allocate_extended(area, add_size(JOIN_CHUNK_HEADER, len),
												  DSA_ALLOC_HUGE);
		JoinChunk  *header = dsa_get_address(area, block);

		SpinLockAcquire(&state->parallel.shared->lock);
		number = (int) state->parallel.shared->next_value_chunk++;
		SpinLockRelease(&state->parallel.shared->lock);
		header->next = *own_list(state, true);
		*own_list(state, true) = block;
		header->number = number;
		header->len = len;
		header->owner = state->parallel.spill_participant;
		base = (char *) header + JOIN_CHUNK_HEADER;
		state->values.bytes = add_size(state->values.bytes, JOIN_CHUNK_HEADER);
		shared_count(state, (int64) (JOIN_CHUNK_HEADER + len));
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
	reserve_values(state, number + 1);
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
store_value(TessHashJoinState *state, Datum value, int16 typlen)
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
				chunk_len_for(JOIN_VALUE_CHUNK);

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

		child_column(batch, state->child_columns[scan_column], valid,
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
				record[1 + word] = store_value(state, values.values[row], typlen);
		}
	}
}

/*
 * Append the pending rows of an inner batch to chunk `chunk` of table:
 * false when it had room for none of them.
 */
bool
append_rows(TessHashJoinState *state, const TessTableRef *table, int chunk,
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
prepare_inner(TessHashJoinState *state, TessBatch *batch, TessRowMask *pending)
{
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask valid;
	int			count;

	reserve_rows(state, nrows);
	/* A shorter batch than the last: no bits past its rows may remain. */
	memset(state->probe.valid_bits, 0, sizeof(uint64) * nwords);
	valid = (TessRowMask) {nrows, state->probe.valid_bits};
	*pending = (TessRowMask) {nrows, state->probe.pending_bits};
	batch_keys(state, batch, state->keys.inner_keys, state->keys.inner_kinds, &valid);
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
		2 * chunk_len_for(JOIN_CHUNK_LEN) > get_hash_memory_limit())
		start_spill(state);
	if (state->spill != NULL)
	{
		insert_spill(state, batch);
		return;
	}
	count = prepare_inner(state, batch, &pending);
	if (count == 0)
		return;
	if (state->table.nchunks == 0)
	{
		add_table_chunk(state);
		fresh = true;
	}
	for (;;)
	{
		bool		appended = append_rows(state, &state->table,
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

/* Read every batch of the inner child into a new table. */
/* This build's keys for the outer side's pruning: none seen yet. */
void
reset_prune_keys(TessHashJoinState *state)
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
note_prune_keys(TessHashJoinState *state, TessBatch *batch)
{
	TessJoinKeys *keys = &state->prune.keys;
	TessDatumColumn column;
	int			row = -1;

	if (!state->prune.on)
		return;
	child_column(batch, state->keys.inner_keys[state->prune.key], &batch->rows,
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
share_prune_keys(TessHashJoinState *state)
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
prune_outer(TessHashJoinState *state)
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

static void
build_table(TessHashJoinState *state)
{
	spill_free(state);
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
	reset_prune_keys(state);
	for (;;)
	{
		TessBatch  *batch = tess_input_next(state->inner_input);

		if (batch == NULL)
			break;
		if (tess_row_mask_count(&batch->rows) > 0)
		{
			note_prune_keys(state, batch);
			insert_batch(state, batch);
		}
		tess_input_finish(state->inner_input);
	}
	if (state->spill != NULL)
		finish_spill_build(state);
	else
		index_table(state);
	state->counters[JOIN_BUILDS]++;
	state->built = true;
}

/*
 * The next record of each row's key: one step in a table grouped by
 * insertion, a walk down the chain in a shared table, whose participants
 * inserted without grouping, and in a round's; a partition a participant
 * joins alone is a table of its own.
 */
static TessStatusCode
next_record(TessHashJoinState *state, const TessRowMask *rows, TessRowMask *found)
{
	if (state->parallel.chain_table)
		return state->kernels->table_next_match(&state->table, state->probe.offsets,
												rows, found, &state->status);
	return state->kernels->table_next_in_group(&state->table, state->probe.offsets,
											   rows, found, &state->status);
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

static const TessBatchOps join_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = join_get_column,
};

/* Make the round's rows the published batch's selection. */
static void
start_round(TessHashJoinState *state)
{
	int			nrows = state->outer_batch->rows.nrows;
	TessRowMask round = {nrows, state->probe.round_bits};

	memcpy(state->probe.published_bits, state->probe.round_bits,
		   sizeof(uint64) * tess_row_mask_word_count(nrows));
	state->batch.rows.nrows = nrows;
	state->batch.rows.bits = state->probe.published_bits;
	state->current_offsets = state->probe.offsets;
	state->current_bits = state->probe.round_bits;
	state->probe.nulls_gathered = false;
	memset(state->probe.gathered, 0, sizeof(bool) * Max(state->npayload, 1));
	state->counters[JOIN_MATCHES] += tess_row_mask_count(&round);
}

/*
 * LEFT: the selected rows of the outer batch without a match, published
 * with NULL inner columns. False when every row had one.
 */
static bool
start_null_round(TessHashJoinState *state)
{
	TessBatch  *outer = state->outer_batch;
	int			nrows = outer->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	uint64		any = 0;

	/* The rows still to answer: those written to disk go with their partition. */
	for (int word = 0; word < nwords; word++)
	{
		state->probe.published_bits[word] = state->active_bits[word] &
			~state->matched_bits[word];
		any |= state->probe.published_bits[word];
	}
	if (any == 0)
		return false;
	state->null_round = true;
	state->batch.rows.nrows = nrows;
	state->batch.rows.bits = state->probe.published_bits;
	state->current_offsets = state->probe.offsets;
	state->current_bits = state->probe.published_bits;
	state->probe.nulls_gathered = false;
	memset(state->probe.gathered, 0, sizeof(bool) * Max(state->npayload, 1));
	return true;
}

/*
 * Offer the filter to the outer child, which may check its rows against it
 * before its costlier work (TessFilter's row-wise clauses): only when a
 * row without a pair leaves the join's output, as INNER and SEMI drop it.
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
	if (ratio <= 0.0 ||
		(ratio < 1.0 &&
		 ((double) state->sample_found >= ratio * state->sample_rows ||
		  state->build_rows < JOIN_BLOOM_MIN_ROWS)))
		return;
	/*
	 * A shared table's filter: the first participant that wants it builds
	 * it for all; until it is ready, the others probe without it.
	 */
	if (state->parallel.shared != NULL)
	{
		bool		built;

		state->bloom.bits = dsa_get_address(query_dsa(state), state->parallel.shared->filter);
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
static bool
probe_batch(TessHashJoinState *state, TessBatch *batch)
{
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask valid;
	TessRowMask found;
	TessRowMask passed;
	int			count;
	int			matches;

	reserve_rows(state, nrows);
	/* A shorter batch than the last: no bits past its rows may remain. */
	memset(state->probe.valid_bits, 0, sizeof(uint64) * nwords);
	memset(state->probe.round_bits, 0, sizeof(uint64) * nwords);
	valid = (TessRowMask) {nrows, state->probe.valid_bits};
	found = (TessRowMask) {nrows, state->probe.round_bits};

	batch_keys(state, batch, state->keys.outer_keys, state->keys.outer_kinds, &valid);
	count = tess_row_mask_count(&valid);
	if (count == 0)
		return false;
	/* The rows of the partitions on disk wait for their partition. */
	if (state->spill != NULL && !state->spill->joining)
	{
		spill_outer(state, batch, &valid);
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
			valid.bits[0] &= ~matched_word(state->spill, nrows);
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

/*
 * The next round: the next record of each row of the current round, as
 * long as some row has one; then the first round of the next outer batch
 * that matches. False at the end of the outer input.
 */
static bool
next_round(TessHashJoinState *state)
{
	for (;;)
	{
		TessBatch  *batch;
		bool		found;

		CHECK_FOR_INTERRUPTS();
		if (state->outer_batch != NULL && !state->null_round)
		{
			int			nrows = state->outer_batch->rows.nrows;

			/*
			 * Without duplicates no row has a next record; the last pass over
			 * a partition's outer rows has no table.
			 */
			if (!state->inner_unique && state->duplicates > 0 &&
				state->table.index != NULL)
			{
				TessRowMask rows = {nrows, state->probe.next_bits};
				TessRowMask found = {nrows, state->probe.round_bits};

				memcpy(state->probe.next_bits, state->probe.round_bits,
					   sizeof(uint64) * tess_row_mask_word_count(nrows));
				check(state, next_record(state, &rows, &found));
				if (tess_row_mask_count(&found) > 0)
				{
					start_round(state);
					return true;
				}
			}
			/* LEFT: after the pairs, the rows that had none. */
			if (state->jointype == JOIN_LEFT && start_null_round(state))
				return true;
		}
		if (state->outer_batch != NULL)
		{
			state->null_round = false;
			outer_finish(state, state->outer_batch);
			state->outer_batch = NULL;
		}
		batch = outer_next(state);
		if (batch == NULL)
			return false;
		found = probe_batch(state, batch);
		if (state->jointype == JOIN_LEFT)
		{
			int			nwords = tess_row_mask_word_count(batch->rows.nrows);

			/* Without join clauses every row found has its match. */
			state->outer_batch = batch;
			if (found && state->qual == NULL)
				memcpy(state->matched_bits, state->probe.round_bits, sizeof(uint64) * nwords);
			else
				memset(state->matched_bits, 0, sizeof(uint64) * nwords);
			if (found)
			{
				start_round(state);
				return true;
			}
			if (start_null_round(state))
				return true;
			continue;
		}
		if (!found)
		{
			outer_finish(state, batch);
			continue;
		}
		state->outer_batch = batch;
		start_round(state);
		return true;
	}
}

/*
 * Fill a compact batch with the pairs of the rounds, from where the last
 * one stopped, or give a dense round to publish as it is: the record of each pair, and the outer columns copied by
 * value from the round's outer batch, fetched once per round. False when
 * no pair is left.
 */
static bool
fill_compact(TessHashJoinState *state)
{
	int			count = 0;

	/* The parent released the previous compact batch: its copies go. */
	MemoryContextReset(state->compact_context);
	/* LEFT: the rows without a match the last compact batch held back. */
	if (state->null_held)
	{
		state->null_held = false;
		(void) start_null_round(state);
		state->output_compact = false;
		return true;
	}

	while (count < JOIN_COMPACT_ROWS)
	{
		int			nrows;
		int			nwords;

		if (!state->compact.round_open)
		{
			/* The pairs copied so far need the table they came from. */
			state->holding = count > 0;
			if (!next_round(state))
				break;
			/*
			 * LEFT: the rows without a match go out over the outer batch,
			 * after the pairs copied so far.
			 */
			if (state->null_round)
			{
				if (count == 0)
				{
					state->output_compact = false;
					return true;
				}
				/* The pairs go first, their inner columns gathered. */
				state->null_held = true;
				state->null_round = false;
				break;
			}
			nrows = state->outer_batch->rows.nrows;
			/*
			 * A dense round, met with nothing copied yet, goes out as it is:
			 * copying it would only cost, a by-reference value most.
			 */
			if (count == 0 &&
				tess_row_mask_count(&(TessRowMask) {nrows, state->probe.round_bits}) >=
				JOIN_DENSE_ROUND)
			{
				state->output_compact = false;
				return true;
			}
			memcpy(state->compact.taken_bits, state->probe.round_bits,
				   sizeof(uint64) * tess_row_mask_word_count(nrows));
			for (int index = 0; index < state->compact.nouter; index++)
			{
				int			column = state->compact.outer_columns[index];

				/*
				 * The round's rows only: the next rounds are among them,
				 * and a lazy child need not read the rows without a pair.
				 */
				child_column(state->outer_batch, state->child_columns[column],
							 &(TessRowMask) {nrows, state->probe.round_bits},
							 TESS_COLUMN_FOR_PROJECTION,
							 &state->compact.round_columns[index]);
			}
			state->compact.round_open = true;
		}
		nrows = state->outer_batch->rows.nrows;
		nwords = tess_row_mask_word_count(nrows);
		for (int index = 0; index < nwords && count < JOIN_COMPACT_ROWS; index++)
		{
			uint64		bits = state->compact.taken_bits[index];

			while (bits != 0 && count < JOIN_COMPACT_ROWS)
			{
				int			row = index * 64 + pg_rightmost_one_pos64(bits);

				bits &= bits - 1;
				state->compact.offsets[count] = state->probe.offsets[row];
				for (int column = 0; column < state->compact.nouter; column++)
				{
					int			scan = state->compact.outer_columns[column];

					Datum		value = state->compact.round_columns[column].values[row];
					bool		isnull = state->compact.round_columns[column].isnull[row];

					/*
					 * A by-reference value points into the outer batch,
					 * which goes before the compact batch does: a copy.
					 */
					if (!isnull && !state->typbyvals[scan])
					{
						MemoryContext oldcontext =
							MemoryContextSwitchTo(state->compact_context);

						value = datumCopy(value, false, state->typlens[scan]);
						MemoryContextSwitchTo(oldcontext);
					}
					state->compact.values[scan][count] = value;
					state->compact.isnull[scan][count] = isnull;
				}
				count++;
			}
			state->compact.taken_bits[index] = bits;
		}
		/* A round copied whole: the next call of next_round advances. */
		for (int index = 0; index < nwords; index++)
			if (state->compact.taken_bits[index] != 0)
				goto more;
		state->compact.round_open = false;
more:
		;
	}
	if (count == 0)
		return false;
	state->output_compact = true;
	state->compact.bits[0] = count == 64 ? ~UINT64CONST(0) :
		(UINT64CONST(1) << count) - 1;
	state->counters[JOIN_COMPACT_BATCHES]++;
	state->batch.rows.nrows = JOIN_COMPACT_ROWS;
	state->batch.rows.bits = state->compact.bits;
	state->current_offsets = state->compact.offsets;
	state->current_bits = state->compact.bits;
	state->probe.nulls_gathered = false;
	memset(state->probe.gathered, 0, sizeof(bool) * Max(state->npayload, 1));
	return true;
}

/*
 * SEMI and ANTI: each outer batch once, its rows with a match (SEMI) or
 * without one (ANTI), a row with a NULL key never having one. A pair
 * counts when it passes the join clauses; rounds walk each row's records
 * until it has one, the rows that do leaving the next rounds. ANTI's
 * filters then apply to the rows it returns. False at the end.
 */
static bool
next_matches(TessHashJoinState *state)
{
	ExprContext *econtext = state->css.ss.ps.ps_ExprContext;

	for (;;)
	{
		TessBatch  *batch;
		int			nrows;
		int			nwords;
		uint64		any = 0;

		CHECK_FOR_INTERRUPTS();
		if (state->outer_batch != NULL)
		{
			outer_finish(state, state->outer_batch);
			state->outer_batch = NULL;
		}
		batch = outer_next(state);
		if (batch == NULL)
			return false;
		nrows = batch->rows.nrows;
		nwords = tess_row_mask_word_count(nrows);
		reserve_rows(state, nrows);
		memset(state->matched_bits, 0, sizeof(uint64) * nwords);
		state->outer_batch = batch;
		/* A table that spills has inner rows on disk even with none resident. */
		if ((state->build_rows > 0 ||
			 (state->spill != NULL && !state->spill->joining)) &&
			probe_batch(state, batch))
		{
			if (state->qual == NULL)
				memcpy(state->matched_bits, state->probe.round_bits, sizeof(uint64) * nwords);
			else
				for (;;)
				{
					TessRowMask round = {nrows, state->probe.round_bits};
					TessRowMask rest = {nrows, state->probe.next_bits};
					uint64		left = 0;

					CHECK_FOR_INTERRUPTS();
					start_round(state);
					ResetExprContext(econtext);
					(void) tess_qual_apply(state->qual, &state->batch, econtext,
										   tess_row_mask_count(&round));
					for (int word = 0; word < nwords; word++)
						state->matched_bits[word] |= state->probe.published_bits[word];
					if (state->inner_unique || state->duplicates == 0)
						break;
					for (int word = 0; word < nwords; word++)
					{
						state->probe.next_bits[word] = state->probe.round_bits[word] &
							~state->matched_bits[word];
						left |= state->probe.next_bits[word];
					}
					if (left == 0)
						break;
					check(state, next_record(state, &rest, &round));
					if (tess_row_mask_count(&round) == 0)
						break;
				}
		}
		/* The rows returned: SEMI the matched ones, ANTI the others. */
		for (int word = 0; word < nwords; word++)
		{
			state->probe.published_bits[word] = state->jointype == JOIN_SEMI ?
				state->matched_bits[word] :
				state->active_bits[word] & ~state->matched_bits[word];
			any |= state->probe.published_bits[word];
		}
		if (any == 0)
			continue;
		state->batch.rows.nrows = nrows;
		state->batch.rows.bits = state->probe.published_bits;
		state->current_offsets = state->probe.offsets;
		state->current_bits = state->probe.published_bits;
		state->probe.nulls_gathered = false;
		memset(state->probe.gathered, 0, sizeof(bool) * Max(state->npayload, 1));
		if (state->filter != NULL)
		{
			ResetExprContext(econtext);
			if (tess_qual_apply(state->filter, &state->batch, econtext,
								tess_row_mask_count(&state->batch.rows)) == 0)
				continue;
		}
		return true;
	}
}

/*
 * RIGHT and FULL: mark the records of the published pairs, which passed
 * the join clauses. A reference is a chunk's number and a place in 8-byte
 * units (tessera/table.h); a chunk's records follow its header, each of
 * record_size bytes.
 */
static void
mark_pairs(TessHashJoinState *state)
{
	const TessRowMask *rows = &state->batch.rows;
	int			nwords = tess_row_mask_word_count(rows->nrows);

	if (state->marks == NULL || state->mark_slots < state->table.nchunks)
	{
		int			slots = Max(state->table.nchunks, 16);
		uint64	  **marks;

		if (state->marks_context == NULL)
			state->marks_context = AllocSetContextCreate(state->css.ss.ps.state->es_query_cxt,
														 "TessHashJoin marks",
														 ALLOCSET_DEFAULT_SIZES);
		marks = MemoryContextAllocZero(state->marks_context, sizeof(uint64 *) * slots);

		if (state->marks != NULL)
			memcpy(marks, state->marks, sizeof(uint64 *) * state->mark_slots);
		state->marks = marks;
		state->mark_slots = slots;
	}
	for (int word = 0; word < nwords; word++)
		for (uint64 bits = rows->bits[word]; bits != 0; bits &= bits - 1)
		{
			uint32		ref = state->current_offsets[word * 64 +
												   pg_rightmost_one_pos64(bits)];
			int			chunk = (int) (ref >> TESS_TABLE_UNIT_BITS);
			Size		byte = (Size) (ref & ((1u << TESS_TABLE_UNIT_BITS) - 1)) * 8;
			Size		index = (byte - TESS_TABLE_CHUNK_HEADER) / state->record_size;
			uint64		bit = UINT64CONST(1) << (index % 64);

			/* A shared table's: other participants set bits of the same words. */
			if (state->marks_shared)
			{
				pg_atomic_uint64 *word = (pg_atomic_uint64 *) &state->marks[chunk][index / 64];

				if ((pg_atomic_read_u64(word) & bit) == 0)
					(void) pg_atomic_fetch_or_u64(word, bit);
				continue;
			}
			if (state->marks[chunk] == NULL)
				state->marks[chunk] =
					MemoryContextAllocZero(state->marks_context,
										   sizeof(uint64) *
										   ((state->table.chunk_lens[chunk] /
											 state->record_size + 63) / 64));
			state->marks[chunk][index / 64] |= bit;
		}
}

/* Start the tail: the inner rows without a pair, from the first chunk on. */
static void
start_tail(TessHashJoinState *state)
{
	reserve_rows(state, JOIN_COMPACT_ROWS);
	state->tail.on = true;
	state->tail.chunk = 0;
	state->tail.byte = TESS_TABLE_CHUNK_HEADER;
	state->output_compact = false;
	state->null_round = false;
}

/*
 * The next records without a pair, up to a batch of them, published with
 * NULL outer columns: a chunk's used mark is its first word. False when
 * the walk is over.
 */
static bool
next_tail(TessHashJoinState *state)
{
	int			count = 0;

	while (count < JOIN_COMPACT_ROWS && state->tail.chunk < state->table.nchunks)
	{
		int			chunk = state->tail.chunk;
		uint64		used = *(const uint64 *) state->table.chunks[chunk];
		Size		index;

		if (state->tail.byte >= used)
		{
			state->tail.chunk++;
			state->tail.byte = TESS_TABLE_CHUNK_HEADER;
			continue;
		}
		index = (state->tail.byte - TESS_TABLE_CHUNK_HEADER) / state->record_size;
		if (state->marks == NULL || chunk >= state->mark_slots ||
			state->marks[chunk] == NULL ||
			((state->marks[chunk][index / 64] >> (index % 64)) & 1) == 0)
			state->tail.refs[count++] = ((uint32) chunk << TESS_TABLE_UNIT_BITS) |
				(uint32) (state->tail.byte / 8);
		state->tail.byte += state->record_size;
	}
	if (count == 0)
		return false;
	state->tail.bits[0] = count == 64 ? ~UINT64CONST(0) :
		(UINT64CONST(1) << count) - 1;
	state->batch.rows.nrows = count;
	state->batch.rows.bits = state->tail.bits;
	state->current_offsets = state->tail.refs;
	state->current_bits = state->tail.bits;
	state->probe.nulls_gathered = false;
	memset(state->probe.gathered, 0, sizeof(bool) * Max(state->npayload, 1));
	return true;
}

/*
 * The next batch of pairs, a round or a compact batch, with the residual
 * join clauses applied: a batch they leave empty is skipped. The clauses
 * narrow the published selection only; a round's own rows stay whole for
 * the next round. False at the end.
 */
static bool
next_output(TessHashJoinState *state)
{
	ExprContext *econtext = state->css.ss.ps.ps_ExprContext;

	if (state->jointype == JOIN_SEMI || state->jointype == JOIN_ANTI)
		return next_matches(state);
	for (;;)
	{
		/*
		 * Rounds go on without a return to the executor while the join's
		 * clauses reject their rows: in the tail, and over outer rows read
		 * back from disk, no child checks for interrupts. A round at a time.
		 */
		CHECK_FOR_INTERRUPTS();
		if (state->tail.on)
		{
			if (!next_tail(state))
			{
				/*
				 * The table's tail is done: the join goes on past the table
				 * that asked for it, or is over.
				 */
				state->tail.on = false;
				state->tail.table_done = true;
				if (!state->tail.request)
					return false;
				state->tail.request = false;
				continue;
			}
		}
		else if (state->compact.on ? !fill_compact(state) : !next_round(state))
		{
			/* RIGHT and FULL: then the inner rows without a pair, unless asked for already. */
			if (!state->tail.request && !tail_turn(state))
				return false;
			start_tail(state);
			continue;
		}
		/* The join clauses decide the pairs; the rows without one have none. */
		if (state->qual != NULL && !state->null_round && !state->tail.on)
		{
			ResetExprContext(econtext);
			if (tess_qual_apply(state->qual, &state->batch, econtext,
								tess_row_mask_count(&state->batch.rows)) == 0)
				continue;
			/* LEFT: the pairs that passed; no compact batch with join clauses. */
			if (state->jointype == JOIN_LEFT)
				for (int word = 0; word < tess_row_mask_word_count(state->batch.rows.nrows); word++)
					state->matched_bits[word] |= state->probe.published_bits[word];
		}
		if (state->preserve_inner && !state->null_round && !state->tail.on)
			mark_pairs(state);
		/* An outer join's filters over every row it returns. */
		if (state->filter != NULL)
		{
			ResetExprContext(econtext);
			if (tess_qual_apply(state->filter, &state->batch, econtext,
								tess_row_mask_count(&state->batch.rows)) == 0)
				continue;
		}
		return true;
	}
}

/* Publish each round to a batch-aware parent, which finishes it there. */
static TupleTableSlot *
exec_batches(TessHashJoinState *state)
{
	/* Releasing a projection's wrapper forgets the pairs, which stay the node's. */
	tess_output_release(state->output);
	if (!next_output(state))
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
			if (!next_output(state))
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
	state->record_size = 16 + sizeof(uint64) * (state->keys.nkeys + 1 + state->npayload);
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

/* The clauses that run in batches and the others, each in evaluation order. */
static void
split_clauses(List *clauses, List *flags, List **batch, List **rows)
{
	ListCell   *clause;
	ListCell   *flag;

	*batch = NIL;
	*rows = NIL;
	forboth(clause, clauses, flag, flags)
	{
		if (lfirst_int(flag) != 0)
			*batch = lappend(*batch, lfirst(clause));
		else
			*rows = lappend(*rows, lfirst(clause));
	}
}

/* The plan's own data, written by the planner (join_planner.c). */
static void
read_node_data(TessHashJoinState *state, const List *data)
{
	TessPlanReader *reader = tess_plan_reader_create(data, TESS_HASH_JOIN_DATA,
													 TESS_HASH_JOIN_DATA_VERSION);
	List	   *sides = tess_plan_read_int_list(reader, "sides");
	List	   *columns = tess_plan_read_int_list(reader, "child_columns");
	List	   *outer_keys = tess_plan_read_int_list(reader, "outer_keys");
	List	   *inner_keys = tess_plan_read_int_list(reader, "inner_keys");
	List	   *outer_kinds = tess_plan_read_int_list(reader, "outer_kinds");
	List	   *inner_kinds = tess_plan_read_int_list(reader, "inner_kinds");
	List	   *hashers = tess_plan_read_int_list(reader, "key_hashers");
	List	   *collations = tess_plan_read_int_list(reader, "key_collations");
	List	   *prune_params;
	ListCell   *side;
	ListCell   *column;
	int			index = 0;

	state->residual_batch = tess_plan_read_int_list(reader, "residual_batch");
	state->filter_batch = tess_plan_read_int_list(reader, "filter_batch");
	state->jointype = (JoinType) tess_plan_read_int(reader, "jointype");
	state->plan_jointype = state->jointype;
	/* RIGHT is INNER and FULL is LEFT, the unmatched inner rows added last. */
	if (state->jointype == JOIN_RIGHT || state->jointype == JOIN_FULL)
	{
		state->preserve_inner = true;
		state->jointype = state->jointype == JOIN_RIGHT ? JOIN_INNER : JOIN_LEFT;
	}
	state->inner_unique = tess_plan_read_int(reader, "inner_unique") != 0;
	state->inner_rows = tess_plan_read_int(reader, "inner_rows");
	state->parallel.shared_mode = tess_plan_read_int(reader, "shared") != 0;
	state->prune.key = tess_plan_read_int(reader, "prune_key");
	prune_params = tess_plan_read_int_list(reader, "prune_params");
	state->prune.values = (PartitionPruneInfo *) tess_plan_read_node(reader, "prune_values");
	state->prune.range = (PartitionPruneInfo *) tess_plan_read_node(reader, "prune_range");
	state->parallel.round_partition = -1;
	tess_plan_reader_finish(reader);
	if (state->prune.key >= 0)
	{
		if (list_length(prune_params) != 3 || state->prune.values == NULL ||
			!IsA(state->prune.values, PartitionPruneInfo) ||
			(state->prune.range != NULL && !IsA(state->prune.range, PartitionPruneInfo)))
			elog(ERROR, "TessHashJoin received foreign plan data");
		for (int param = 0; param < 3; param++)
			state->prune.params[param] = list_nth_int(prune_params, param);
	}
	state->keys.nkeys = list_length(outer_keys);
	if (list_length(sides) != state->ncolumns ||
		list_length(columns) != state->ncolumns ||
		state->keys.nkeys < 1 || state->keys.nkeys > TESS_TABLE_MAX_KEYS ||
		list_length(inner_keys) != state->keys.nkeys ||
		list_length(outer_kinds) != state->keys.nkeys ||
		list_length(inner_kinds) != state->keys.nkeys ||
		list_length(hashers) != state->keys.nkeys ||
		list_length(collations) != state->keys.nkeys ||
		(state->jointype != JOIN_INNER && state->jointype != JOIN_SEMI &&
		 state->jointype != JOIN_ANTI && state->jointype != JOIN_LEFT) ||
		(state->filter_batch != NIL &&
		 state->jointype != JOIN_LEFT && state->jointype != JOIN_ANTI &&
		 !state->preserve_inner))
		elog(ERROR, "TessHashJoin received foreign plan data");
	for (int key = 0; key < state->keys.nkeys; key++)
	{
		state->keys.outer_keys[key] = list_nth_int(outer_keys, key);
		state->keys.inner_keys[key] = list_nth_int(inner_keys, key);
		state->keys.outer_kinds[key] = list_nth_int(outer_kinds, key);
		state->keys.inner_kinds[key] = list_nth_int(inner_kinds, key);
		if ((state->keys.outer_kinds[key] != TESS_TABLE_KEY_INT4 &&
			 state->keys.outer_kinds[key] != TESS_TABLE_KEY_INT8) ||
			(state->keys.inner_kinds[key] != TESS_TABLE_KEY_INT4 &&
			 state->keys.inner_kinds[key] != TESS_TABLE_KEY_INT8))
			elog(ERROR, "TessHashJoin received foreign plan data");
		state->keys.collations[key] = (Oid) list_nth_int(collations, key);
		state->keys.hashers[key].fn_oid = InvalidOid;
		if (OidIsValid((Oid) list_nth_int(hashers, key)))
		{
			if (state->keys.outer_kinds[key] != TESS_TABLE_KEY_INT8 ||
				state->keys.inner_kinds[key] != TESS_TABLE_KEY_INT8)
				elog(ERROR, "TessHashJoin received foreign plan data");
			fmgr_info((Oid) list_nth_int(hashers, key), &state->keys.hashers[key]);
			state->keys.hashed_keys = true;
		}
	}
	if (state->prune.key >= state->keys.nkeys ||
		(state->prune.key >= 0 && OidIsValid(state->keys.hashers[state->prune.key].fn_oid)))
		elog(ERROR, "TessHashJoin received foreign plan data");
	if (state->keys.hashed_keys)
		state->keys.hash_context = AllocSetContextCreate(CurrentMemoryContext,
													"TessHashJoin key hashes",
													ALLOCSET_DEFAULT_SIZES);
	state->sides = palloc_array(int, state->ncolumns);
	state->child_columns = palloc_array(int, state->ncolumns);
	forboth(side, sides, column, columns)
	{
		state->sides[index] = lfirst_int(side);
		state->child_columns[index] = lfirst_int(column);
		if (state->sides[index] != JOIN_SIDE_OUTER &&
			state->sides[index] != JOIN_SIDE_INNER)
			elog(ERROR, "TessHashJoin received foreign plan data");
		index++;
	}
}

static void
join_begin(CustomScanState *css, EState *estate, int eflags)
{
	TessHashJoinState *state = (TessHashJoinState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	int			index = 0;

	/* The planner puts Material above a batch subtree for these. */
	if (eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK))
		elog(ERROR, "TessHashJoin supports neither backward scan nor mark/restore");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_hash_join_node || info.nchildren != 2 ||
		info.child_names[0] == NULL || info.child_names[1] == NULL ||
		cscan->custom_scan_tlist == NIL)
		elog(ERROR, "TessHashJoin received a foreign plan");
	state->kernels = tess_runtime_kernels();
	if (state->kernels == NULL)
		ereport(ERROR,
				(errcode(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE),
				 errmsg("TessHashJoin needs the Tessera kernels"),
				 errhint("Load tessera_kernels, or preload it with the other Tessera modules.")));
	state->ncolumns = list_length(cscan->custom_scan_tlist);
	read_node_data(state, (List *) info.node_data);
	state->typlens = palloc_array(int16, state->ncolumns);
	state->typbyvals = palloc_array(bool, state->ncolumns);
	foreach_ptr(TargetEntry, entry, cscan->custom_scan_tlist)
	{
		get_typlenbyval(exprType((Node *) entry->expr), &state->typlens[index],
						&state->typbyvals[index]);
		index++;
	}
	state->payload_words = palloc0_array(int, state->ncolumns);
	state->compact.outer_columns = palloc0_array(int, state->ncolumns);
	state->compact.values = palloc0_array(Datum *, state->ncolumns);
	state->compact.isnull = palloc0_array(bool *, state->ncolumns);
	state->compact.round_columns = palloc0_array(TessDatumColumn, Max(state->ncolumns, 1));
	state->payload_columns = palloc0_array(int, JOIN_MAX_PAYLOAD);

	state->outer = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
	state->inner = ExecInitNode(lsecond(cscan->custom_plans), estate, eflags);
	css->custom_ps = list_make2(state->outer, state->inner);
	if (state->prune.key >= 0 &&
		tess_append_join_prune_begin(state->outer, state->prune.values, state->prune.range,
									 state->prune.params))
	{
		state->prune.on = true;
		state->prune.keys.int8 = state->keys.inner_kinds[state->prune.key] == TESS_TABLE_KEY_INT8;
		state->prune.keys.values = palloc_array(int64, JOIN_PRUNE_VALUES);
	}
	state->outer_input = tess_input_create(estate->es_query_cxt, state->outer);
	state->inner_input = tess_input_create(estate->es_query_cxt, state->inner);
	state->layout = info.layout;
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   css->ss.ps.ps_ResultTupleSlot,
									   &info.layout);
	state->table_context = AllocSetContextCreate(estate->es_query_cxt,
												 "TessHashJoin table",
												 ALLOCSET_DEFAULT_SIZES);
	state->values_context = AllocSetContextCreate(estate->es_query_cxt,
												  "TessHashJoin values",
												  ALLOCSET_DEFAULT_SIZES);
	reset_values(state);
	/* The residual clauses: those the compiler took in batches, then by rows. */
	state->scan_layout = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	state->scan_layout.ncolumns = state->ncolumns;
	state->scan_layout.ntargets = state->ncolumns;
	/* custom_exprs: the key clauses, the residual ones, an outer join's filters. */
	if (list_length(cscan->custom_exprs) !=
		state->keys.nkeys + list_length(state->residual_batch) +
		list_length(state->filter_batch))
		elog(ERROR, "TessHashJoin received a foreign plan");
	if (state->filter_batch != NIL)
	{
		TessQualConfig filter = TESS_STRUCT_INITIALIZER(TessQualConfig);
		List	   *filters = list_copy_tail(cscan->custom_exprs,
											 state->keys.nkeys +
											 list_length(state->residual_batch));

		filter.parent_context = estate->es_query_cxt;
		filter.parent = &css->ss.ps;
		split_clauses(filters, state->filter_batch, &filter.batch_clauses,
					  &filter.row_clauses);
		filter.order = state->filter_batch;
		filter.scan_slot = css->ss.ss_ScanTupleSlot;
		filter.scan_tuple = &state->scan_layout;
		state->filter = tess_qual_create(&filter);
	}
	if (state->residual_batch != NIL)
	{
		TessQualConfig qual = TESS_STRUCT_INITIALIZER(TessQualConfig);

		List	   *residual = list_copy_head(list_copy_tail(cscan->custom_exprs,
															  state->keys.nkeys),
											  list_length(state->residual_batch));

		qual.parent_context = estate->es_query_cxt;
		qual.parent = &css->ss.ps;
		split_clauses(residual, state->residual_batch, &qual.batch_clauses,
					  &qual.row_clauses);
		qual.order = state->residual_batch;
		qual.scan_slot = css->ss.ss_ScanTupleSlot;
		qual.scan_tuple = &state->scan_layout;
		state->qual = tess_qual_create(&qual);
	}
	if (info.computed != NIL)
	{
		TessProjectionConfig projection = TESS_STRUCT_INITIALIZER(TessProjectionConfig);

		/* Computed columns follow the scan tuple's, as for TessFilter. */
		projection.parent_context = estate->es_query_cxt;
		projection.parent = &css->ss.ps;
		projection.econtext = css->ss.ps.ps_ExprContext;
		projection.scan_slot = css->ss.ss_ScanTupleSlot;
		projection.scan_tuple = &state->scan_layout;
		projection.base_columns = state->ncolumns;
		projection.computed = info.computed;
		state->projection = tess_projection_create(&projection);
		state->computed = info.computed;
	}
	state->status = (TessStatus) TESS_STRUCT_INITIALIZER(TessStatus);
	state->batch = (TessBatch) {
		TESS_ABI_INITIALIZER(TESS_BATCH_ABI_VERSION, TessBatch),
	};
	state->batch.table_oid = InvalidOid;
	state->batch.ops = &join_batch_ops;
	state->batch.private_data = state;
	state->columns = palloc0_array(TessDatumColumn,
								   Max(css->ss.ps.ps_ResultTupleSlot->tts_tupleDescriptor->natts, 1));
	state->next_row = -1;
}

/*
 * Compact mode for a batch-aware parent over a table with duplicate keys;
 * a by-reference outer value is copied, since it must outlive its outer
 * batch.
 */
void
decide_compact(TessHashJoinState *state)
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

static TupleTableSlot *
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
			build_shared(state);
		else
			build_table(state);
		if (state->done)
			return NULL;
	}
	prune_outer(state);
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
		decide_compact(state);
	return state->request->output_mode == TESS_OUTPUT_BATCH ?
		exec_batches(state) : exec_rows(state);
}

static void
join_end(CustomScanState *css)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	tess_output_end(state->output);
	ExecEndNode(state->outer);
	ExecEndNode(state->inner);
	spill_free(state);
	MemoryContextDelete(state->values_context);
	MemoryContextDelete(state->table_context);
}

/*
 * The core passes changed parameters to the children; the table is built
 * again only when the inner child depends on one, as the core's hash join
 * decides, and is otherwise probed by the rescanned outer child again.
 */
static void
join_rescan(CustomScanState *css)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	tess_output_clear(state->output);
	ExecClearTuple(css->ss.ps.ps_ResultTupleSlot);
	if (state->projection != NULL)
		tess_projection_reset(state->projection);
	state->published = NULL;
	/* The rescan forgets the outer batch with the child's other state. */
	state->outer_batch = NULL;
	state->compact.round_open = false;
	state->null_round = false;
	state->null_held = false;
	state->serving = false;
	/* RIGHT and FULL: a table kept for the next scan has no pair yet. */
	state->tail.on = false;
	state->tail.request = false;
	state->tail.table_done = false;
	/* A shared table's go with it: the build starts anew. */
	if (state->marks_shared)
		forget_marks(state);
	for (int chunk = 0; state->marks != NULL && chunk < state->mark_slots; chunk++)
		if (state->marks[chunk] != NULL)
			memset(state->marks[chunk], 0,
				   sizeof(uint64) * ((state->table.chunk_lens[chunk] /
									  state->record_size + 63) / 64));
	state->next_row = -1;
	state->done = false;
	if (css->ss.ps.chgParam != NULL)
	{
		UpdateChangedParamSet(state->outer, css->ss.ps.chgParam);
		UpdateChangedParamSet(state->inner, css->ss.ps.chgParam);
	}
	/*
	 * A shared table goes with its build, which starts anew with the
	 * rescan's workers; the leader leaves the one it took part in.
	 */
	if (state->parallel.round_partition >= 0)
		round_leave(state);
	if (state->parallel.shared != NULL)
		leave_shared(state, false);
	/*
	 * A table that spilled is no longer whole: the inner child is read
	 * again, rescanned here when no parameter of it changed.
	 */
	if (state->spill != NULL)
	{
		spill_free(state);
		if (state->inner->chgParam == NULL)
			ExecReScan(state->inner);
		tess_input_rescan(state->inner_input);
		state->built = false;
	}
	else if (state->inner->chgParam != NULL || state->parallel.shared != NULL)
	{
		/* The inner child rescans at its next execution. */
		tess_input_rescan(state->inner_input);
		state->built = false;
	}
	/* Without changed parameters, the executor would not rescan it. */
	if (state->outer->chgParam == NULL)
		ExecReScan(state->outer);
	tess_input_rescan(state->outer_input);
}

/* This participant's counters, the memory ones as of now. */
static void
join_counters(TessHashJoinState *state, uint64 *values)
{
	Size		limit = get_hash_memory_limit();

	memcpy(values, state->counters, sizeof(state->counters));
	if (state->qual != NULL)
	{
		const TessQualStats *removed = tess_qual_stats(state->qual);

		values[JOIN_FILTER_REMOVED] = removed->batch_removed + removed->row_removed;
	}
	if (state->filter != NULL)
	{
		const TessQualStats *removed = tess_qual_stats(state->filter);

		values[JOIN_OUTPUT_REMOVED] = removed->batch_removed + removed->row_removed;
	}
	values[JOIN_MEMORY] = state->peak_memory;
	/* A shared table's participants share a budget: EXPLAIN compares the total. */
	values[JOIN_OVERRUN] = state->parallel.shared_budget == 0 && state->peak_memory > limit ?
		state->peak_memory - limit : 0;
}

/*
 * The join clause; with ANALYZE, the table and the rows through it, the
 * totals of every participant in a parallel plan, each of which builds a
 * table of its own. The memory is the most the tables and the copies of
 * inner values took, and Overrun what of it exceeded hash_mem: the node
 * keeps the whole inner side in memory rather than spilling it.
 */
static void
join_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	TessHashJoinState *state = (TessHashJoinState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	bool		useprefix = es->rtable_size > 1 || es->verbose;
	List	   *context;
	const uint64 *totals;
	uint64		own[JOIN_NCOUNTERS];
	uint64		overrun;

	context = set_deparse_context_plan(es->deparse_cxt, css->ss.ps.plan,
									   ancestors);
	if (state->plan_jointype != JOIN_INNER)
		ExplainPropertyText("Join Type",
							state->plan_jointype == JOIN_SEMI ? "Semi" :
							state->plan_jointype == JOIN_ANTI ? "Anti" :
							state->plan_jointype == JOIN_RIGHT ? "Right" :
							state->plan_jointype == JOIN_FULL ? "Full" : "Left", es);
	ExplainPropertyText("Hash Cond",
						deparse_expression((Node *) make_ands_explicit(list_copy_head(cscan->custom_exprs,
																					  state->keys.nkeys)),
										   context, useprefix, false), es);
	if (state->parallel.shared_mode)
		ExplainPropertyBool("Shared Table", true, es);
	for (int part = 0; part < 2; part++)
	{
		/*
		 * The residual join clauses, then an outer join's filters: those
		 * the compiler took in batches, and the others.
		 */
		List	   *flags = part == 0 ? state->residual_batch : state->filter_batch;
		int			first = state->keys.nkeys +
			(part == 0 ? 0 : list_length(state->residual_batch));
		List	   *clauses = list_copy_head(list_copy_tail(cscan->custom_exprs, first),
											 list_length(flags));
		List	   *batch;
		List	   *rows;

		split_clauses(clauses, flags, &batch, &rows);
		if (batch != NIL)
			ExplainPropertyText(part == 0 ? "Batch Join Filter" : "Batch Filter",
								deparse_expression((Node *) make_ands_explicit(batch),
												   context, useprefix, false), es);
		if (rows != NIL)
			ExplainPropertyText(part == 0 ? "Join Filter" : "Filter",
								deparse_expression((Node *) make_ands_explicit(rows),
												   context, useprefix, false), es);
	}
	if (!es->analyze)
		return;
	join_counters(state, own);
	totals = tess_shared_stats_totals_or(state->stats, own);
	ExplainPropertyInteger("Buckets", NULL,
						   totals[JOIN_BUILDS] > 0 ?
						   totals[JOIN_BUCKETS] / totals[JOIN_BUILDS] : 0, es);
	ExplainPropertyInteger("Memory Usage", "kB",
						   (totals[JOIN_MEMORY] + 1023) / 1024, es);
	if (state->parallel.shared_budget > 0)
		overrun = totals[JOIN_MEMORY] > state->parallel.shared_budget ?
			totals[JOIN_MEMORY] - state->parallel.shared_budget : 0;
	else
		overrun = totals[JOIN_OVERRUN];
	if (overrun > 0)
		ExplainPropertyInteger("Overrun", "kB", (overrun + 1023) / 1024, es);
	/* As the core's hash shows its batches on disk. */
	if (totals[JOIN_BATCHES] > 0)
	{
		ExplainPropertyInteger("Batches", NULL, totals[JOIN_BATCHES], es);
		ExplainPropertyInteger("Disk Usage", "kB", (totals[JOIN_DISK] + 1023) / 1024, es);
	}
	if (state->qual != NULL)
		ExplainPropertyInteger("Rows Removed by Join Filter", NULL,
							   totals[JOIN_FILTER_REMOVED], es);
	if (state->filter != NULL)
		ExplainPropertyInteger("Rows Removed by Filter", NULL,
							   totals[JOIN_OUTPUT_REMOVED], es);
	if (totals[JOIN_BLOOM_FILTERS] > 0 &&
		(totals[JOIN_BLOOM_BELOW] == 0 || totals[JOIN_BLOOM_REMOVED] > 0))
		ExplainPropertyInteger("Rows Removed by Bloom Filter", NULL,
							   totals[JOIN_BLOOM_REMOVED], es);
	/* The builds, the table's chunks, the spill and the probe: VERBOSE only. */
	if (!es->verbose)
		return;
	ExplainPropertyInteger("Builds", NULL, totals[JOIN_BUILDS], es);
	ExplainPropertyInteger("Build Rows", NULL, totals[JOIN_BUILD_ROWS], es);
	ExplainPropertyInteger("Chunks", NULL, totals[JOIN_CHUNKS], es);
	if (totals[JOIN_BATCHES] > 0)
	{
		ExplainPropertyInteger("Resident Partitions", NULL, totals[JOIN_RESIDENT], es);
		ExplainPropertyInteger("Spilled Chunks", NULL, totals[JOIN_SPILLED], es);
		ExplainPropertyInteger("Tail Chunks Kept", NULL, totals[JOIN_TAILS], es);
		if (totals[JOIN_SPLITS] > 0)
			ExplainPropertyInteger("Split Partitions", NULL, totals[JOIN_SPLITS], es);
		if (totals[JOIN_PASSES] > 0)
			ExplainPropertyInteger("Extra Passes", NULL, totals[JOIN_PASSES], es);
		if (totals[JOIN_ROUNDS] > 0)
			ExplainPropertyInteger("Partitions Joined Together", NULL, totals[JOIN_ROUNDS], es);
		if (totals[JOIN_ALONE] > 0)
			ExplainPropertyInteger("Partitions Joined Alone", NULL, totals[JOIN_ALONE], es);
	}
	ExplainPropertyInteger("Probe Rows", NULL, totals[JOIN_PROBE_ROWS], es);
	ExplainPropertyInteger("Matches", NULL, totals[JOIN_MATCHES], es);
	if (totals[JOIN_COMPACT_BATCHES] > 0)
		ExplainPropertyInteger("Compact Batches", NULL,
							   totals[JOIN_COMPACT_BATCHES], es);
	if (totals[JOIN_BLOOM_FILTERS] > 0)
		ExplainPropertyInteger("Bloom Filters", NULL,
							   totals[JOIN_BLOOM_FILTERS], es);
	/* The outer child checked its rows: it shows the rows removed. */
	if (totals[JOIN_BLOOM_BELOW] > 0)
		ExplainPropertyBool("Bloom Filter Below", true, es);
}

/*
 * A parallel plan: the outer child divides the rows, and every
 * participant builds the whole inner side into a table of its own, as the
 * core's hash join without a shared table does. The node shares only its
 * counters, in the rows of its chunk.
 */
/* The bytes of the chunk a shared build takes before the counters. */
static Size
shared_size(TessHashJoinState *state)
{
	return state->parallel.shared_mode ? MAXALIGN(sizeof(JoinShared)) : 0;
}

/* A shared build's state before any participant attaches. */
static void
init_shared(TessHashJoinState *state, int participants, dsm_segment *segment)
{
	dsa_area   *area = query_dsa(state);
	Size		budget = get_hash_memory_limit();

	BarrierInit(&state->parallel.shared->build, 0);
	state->parallel.shared->index = InvalidDsaPointer;
	state->parallel.shared->index_len = 0;
	state->parallel.shared->directory = InvalidDsaPointer;
	state->parallel.shared->nchunks = 0;
	state->parallel.shared->marks = InvalidDsaPointer;
	state->parallel.shared->filter = InvalidDsaPointer;
	state->parallel.shared->filter_words = 0;
	SpinLockInit(&state->parallel.shared->lock);
	state->parallel.shared->next_value_chunk = 0;
	state->parallel.shared->value_directory = InvalidDsaPointer;
	state->parallel.shared->nvalue_chunks = 0;
	check(state, state->kernels->build_counters_init(state->parallel.shared->counters,
													 &state->status));
	/*
	 * Spilling: the words for the most partitions, and the files; the
	 * budget is every participant's hash_mem, as the core's shared table
	 * has. A rescan keeps the files' set and deletes the files.
	 */
	if (segment != NULL)
	{
		check(state, state->kernels->table_spill_words(JOIN_SPILL_MAX_PARTITIONS,
													   &state->parallel.shared->spill_nwords,
													   &state->status));
		state->parallel.shared->spill_words =
			dsa_allocate(area, sizeof(uint64) * state->parallel.shared->spill_nwords);
		state->parallel.shared->participants = participants;
		state->parallel.shared->lists = dsa_allocate(area, sizeof(dsa_pointer) * 2 * participants);
		state->parallel.shared->part_stats =
			dsa_allocate(area, sizeof(pg_atomic_uint64) * 2 * JOIN_SPILL_MAX_PARTITIONS);
		state->parallel.shared->rounds = InvalidDsaPointer;
		state->parallel.shared->nrounds = 0;
		state->parallel.shared->segment = dsm_segment_handle(segment);
		tess_spill_shared_init(&state->parallel.shared->fileset, segment);
	}
	else
		SharedFileSetDeleteAll(&state->parallel.shared->fileset);
	if (budget > SIZE_MAX / Max(state->parallel.shared->participants, 1))
		budget = SIZE_MAX / Max(state->parallel.shared->participants, 1);
	state->parallel.shared_budget = budget * state->parallel.shared->participants;
	check(state, state->kernels->table_spill_init(dsa_get_address(area,
																  state->parallel.shared->spill_words),
												  state->parallel.shared->spill_nwords,
												  (uint64) budget * state->parallel.shared->participants,
												  &state->status));
	for (int list = 0; list < 2 * state->parallel.shared->participants; list++)
		*participant_list(state, list / 2, list % 2 == 1) = InvalidDsaPointer;
	for (int partition = 0; partition < JOIN_SPILL_MAX_PARTITIONS; partition++)
	{
		pg_atomic_init_u64(&part_stats(state, partition)[0], 0);
		pg_atomic_init_u64(&part_stats(state, partition)[1], 0);
	}
	state->parallel.shared->spill_filter = InvalidDsaPointer;
	state->parallel.shared->spill_filter_words = 0;
	state->parallel.shared->resident_rows = 0;
	SpinLockInit(&state->parallel.shared->prune_lock);
	state->parallel.shared->prune_rows = 0;
	state->parallel.shared->prune_min = PG_INT64_MAX;
	state->parallel.shared->prune_max = PG_INT64_MIN;
	state->parallel.shared->prune_nvalues = 0;
	memset(&state->parallel.participant, 0, sizeof(state->parallel.participant));
	state->parallel.participating = false;
}

static Size
join_estimate_dsm(CustomScanState *css, ParallelContext *pcxt)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	return add_size(shared_size(state),
					tess_shared_stats_estimate(JOIN_NCOUNTERS, pcxt->nworkers));
}

static void
join_initialize_dsm(CustomScanState *css, ParallelContext *pcxt,
					void *coordinate)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	if (state->parallel.shared_mode)
	{
		state->parallel.shared = coordinate;
		init_shared(state, pcxt->nworkers + 1, pcxt->seg);
	}
	state->stats = tess_shared_stats_setup(state->stats,
										   css->ss.ps.state->es_query_cxt,
										   (char *) coordinate + shared_size(state),
										   JOIN_NCOUNTERS, pcxt->nworkers, pcxt->seg);
}

/*
 * Before a rescan's workers start: the leader leaves a build it still
 * takes part in, and the table, which a participant that stopped early
 * may have left behind, is freed; the next execution builds anew.
 */
static void
join_reinitialize_dsm(CustomScanState *css, ParallelContext *pcxt,
					  void *coordinate)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	if (state->parallel.shared != NULL)
	{
		if (state->parallel.round_partition >= 0)
			round_leave(state);
		leave_shared(state, false);
		/* Its files go with the set's. */
		spill_free(state);
		free_shared_table(state);
		free_rounds(state);
		init_shared(state, state->parallel.shared->participants, NULL);
		state->built = false;
	}
	tess_shared_stats_reset(state->stats);
}

static void
join_initialize_worker(CustomScanState *css, shm_toc *toc, void *coordinate)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	if (state->parallel.shared_mode)
	{
		dsm_segment *segment;

		state->parallel.shared = coordinate;
		state->parallel.shared_budget = Min(get_hash_memory_limit(),
								   SIZE_MAX / Max(state->parallel.shared->participants, 1)) *
			state->parallel.shared->participants;
		/* The files, through the segment the worker already maps. */
		segment = dsm_find_mapping(state->parallel.shared->segment);
		if (segment == NULL)
			elog(ERROR, "TessHashJoin found no segment for its shared files");
		tess_spill_shared_attach(&state->parallel.shared->fileset, segment);
	}
	state->stats = tess_shared_stats_attach(css->ss.ps.state->es_query_cxt,
											(char *) coordinate + shared_size(state),
											ParallelWorkerNumber + 1);
}

static void
join_shutdown(CustomScanState *css)
{
	TessHashJoinState *state = (TessHashJoinState *) css;
	uint64		values[JOIN_NCOUNTERS];

	if (state->parallel.round_partition >= 0)
		round_leave(state);
	if (state->parallel.shared != NULL)
		leave_shared(state, false);
	if (state->stats == NULL)
		return;
	join_counters(state, values);
	tess_shared_stats_store(state->stats, values);
}

static Node *
join_create_state(CustomScan *cscan)
{
	TessHashJoinState *state = (TessHashJoinState *)
		newNode(sizeof(TessHashJoinState), T_CustomScanState);

	state->css.methods = &join_exec_methods;
	return (Node *) state;
}

static const CustomExecMethods join_exec_methods = {
	.CustomName = "TessHashJoin",
	.BeginCustomScan = join_begin,
	.ExecCustomScan = join_exec,
	.EndCustomScan = join_end,
	.ReScanCustomScan = join_rescan,
	.ExplainCustomScan = join_explain,
	.EstimateDSMCustomScan = join_estimate_dsm,
	.InitializeDSMCustomScan = join_initialize_dsm,
	.ReInitializeDSMCustomScan = join_reinitialize_dsm,
	.InitializeWorkerCustomScan = join_initialize_worker,
	.ShutdownCustomScan = join_shutdown,
};

const CustomScanMethods tess_hash_join_scan_methods = {
	.CustomName = "TessHashJoin",
	.CreateCustomScanState = join_create_state,
};

const TessNode tess_hash_join_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_HASH_JOIN_NODE_NAME,
};

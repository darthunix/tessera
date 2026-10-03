/*
 * TessAgg with GROUP BY: the table of groups, the batches that find or
 * make their groups in it, and the walk over the groups that puts them
 * out, INTERSECT and EXCEPT among them; their spill is agg_spill.c. See
 * agg.c.
 */
#include "postgres.h"

#include "executor/executor.h"
#include "miscadmin.h"
#include "parser/parse_agg.h"

#include "agg.h"
#include "agg_node.h"

/*
 * The bytes of the table now, and the most so far; once it spills, the
 * index and every chunk of every level, which live in the levels' memory.
 */
void
note_memory(TessAggState *state)
{
	Size		memory = state->spill == NULL ? state->table_bytes :
		agg_spill_memory(state);

	if (state->has_distinct)
		memory += agg_distinct_bytes(state);
	state->peak_memory = Max(state->peak_memory, memory);
}

/*
 * A first index for capacity groups at most, and at most a quarter of
 * hash_mem, about 8 bytes of buckets per group: an estimate too large
 * would take the memory the groups need; the index grows as they come.
 */
uint64
first_capacity(uint64 capacity)
{
	uint64		most = get_hash_memory_limit() / 32;

	return Max(Min(capacity, most), AGG_INITIAL_GROUPS);
}

/* An index for capacity groups in the table's memory. */
void *
new_index(TessAggState *state, uint64 capacity, Size *size)
{
	Size		payload_size = state->payload_size;

	check(state, state->kernels->table_size(state->nkeys, state->kinds,
											payload_size, capacity, size,
											&state->status));
	return MemoryContextAllocExtended(state->table_context, *size, MCXT_ALLOC_HUGE);
}

/* An empty table of groups, its index sized for the planner's estimate. */
static void
create_table(TessAggState *state)
{
	Size		payload_size = state->payload_size;
	uint64		capacity = first_capacity(state->groups_estimate);
	Size		size;

	MemoryContextReset(state->table_context);
	state->capacity = 0;
	state->chunk_slots = 16;
	state->chunk_bases = MemoryContextAlloc(state->table_context,
											sizeof(void *) * state->chunk_slots);
	state->chunk_lens = MemoryContextAlloc(state->table_context,
										   sizeof(Size) * state->chunk_slots);
	state->table.index = new_index(state, capacity, &size);
	state->table.index_len = size;
	state->table.chunks = state->chunk_bases;
	state->table.chunk_lens = state->chunk_lens;
	state->table.nchunks = 0;
	state->table_bytes = size;
	check(state, state->kernels->table_create(state->table.index, size, state->nkeys,
											  state->kinds, payload_size,
											  capacity, &state->status));
	note_memory(state);
}

/* Another chunk of records, the last one being full. */
static void
add_chunk(TessAggState *state)
{
	int			chunk = state->table.nchunks;
	/* Past the first, an eighth of hash_mem, so that a small one spills late. */
	Size		len = chunk == 0 ? AGG_FIRST_CHUNK :
		Max(AGG_FIRST_CHUNK, Min(TESS_TABLE_MAX_CHUNK_LEN,
								 TYPEALIGN_DOWN(8, get_hash_memory_limit() / 8)));
	void	   *base;

	if (chunk == TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessAgg group table cannot hold more than %d chunks",
						TESS_TABLE_MAX_CHUNKS)));
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
	base = MemoryContextAlloc(state->table_context, len);
	check(state, state->kernels->table_chunk_init(base, len, &state->status));
	state->chunk_bases[chunk] = base;
	state->chunk_lens[chunk] = len;
	state->table.nchunks++;
	state->table_bytes += len;
	note_memory(state);
}

/*
 * An index for twice the groups: the buckets are filled anew from the
 * records, which stay where they are, and the old index is freed.
 */
void
regrow_table(TessAggState *state, uint64 groups)
{
	void	   *old = state->table.index;
	Size		size;
	void	   *index;

	/* find_or_insert stopped at half the buckets, over existing chunks. */
	Assert(groups > 0 && state->table.nchunks > 0);
	index = new_index(state, groups * 2, &size);

	check(state, state->kernels->table_regrow(&state->table, index, size,
											  groups * 2, &state->status));
	state->table_bytes = state->table_bytes - state->table.index_len + size;
	state->peak_memory = Max(state->peak_memory, state->table_bytes +
							 state->table.index_len);
	state->table.index = index;
	state->table.index_len = size;
	pfree(old);
	state->grows++;
}

/* The buffers of a batch of nrows rows, in the table's memory. */
static void
reserve_rows(TessAggState *state, int nrows)
{
	int			nwords = tess_row_mask_word_count(nrows);

	if (state->capacity >= nrows)
		return;
	state->hashes = MemoryContextAlloc(state->table_context, sizeof(uint32) * nrows);
	state->offsets = MemoryContextAlloc(state->table_context, sizeof(uint32) * nrows);
	state->valid_bits = MemoryContextAlloc(state->table_context, sizeof(uint64) * nwords);
	state->pending_bits = MemoryContextAlloc(state->table_context, sizeof(uint64) * nwords);
	state->inserted_bits = MemoryContextAlloc(state->table_context, sizeof(uint64) * nwords);
	state->call_bits = MemoryContextAlloc(state->table_context, sizeof(uint64) * nwords);
	state->sum_rest_bits = MemoryContextAlloc(state->table_context,
											  sizeof(uint64) * nwords * TESS_TABLE_MAX_SUMS);
	state->capacity = nrows;
}

/* The bytes the groups take: the table and, with generic aggregates, their states. */
static Size
groups_memory(TessAggState *state)
{
	Size		bytes = state->table_bytes;

	if (state->has_generic)
		bytes += MemoryContextMemAllocated(state->generic_agg->curaggcontext->ecxt_per_tuple_memory,
										   true);
	for (int key = 0; key < state->nkeys; key++)
		if (state->dicts[key] != NULL)
			bytes += MemoryContextMemAllocated(state->dicts[key]->context, true);
	return bytes;
}

/* The child's columns the node reads, in their order, before it computes anything. */
void
agg_read_in_order(TessAggState *state, TessBatch *batch)
{
	if (state->nread_columns < 2)
		return;
	for (int index = 0; index < state->nread_columns; index++)
	{
		TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);

		batch->ops->get_datum_column(batch, state->read_columns[index], &batch->rows,
									 TESS_COLUMN_FOR_PROJECTION, &column);
	}
}

/* A computed column of the projection's wrapper, checked. */
static void
computed_column(TessAggState *state, TessBatch *batch, int computed,
				TessColumnPurpose purpose, bool decimals, TessDatumColumn *result)
{
	*result = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
	result->accept_decimals = decimals;
	batch->ops->get_datum_column(batch, state->child_layout.ncolumns + computed,
								 &batch->rows, purpose, result);
	if (result->values == NULL || result->isnull == NULL ||
		result->nrows != batch->rows.nrows)
		elog(ERROR, "Tessera projection returned an invalid column");
}

/*
 * One batch into the groups: its keys, hashed in key order with NULL as a
 * key of its own, give each row the record of its group, created where
 * none exists (in another chunk or a larger index when the table has no
 * room), and each aggregate folds the
 * rows into the records' states. The batch is the projection's wrapper,
 * which computes the keys and the arguments.
 */
static void
group_batch(TessAggState *state, TessBatch *batch)
{
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask valid;
	TessRowMask pending;
	TessRowMask inserted;
	int			nsums;

	/*
	 * The right side of INTERSECT or EXCEPT while every group of the left
	 * side is in the table: its rows only count into the groups they find,
	 * and a row of no group, which cannot change what goes out, is dropped,
	 * as the core's SetOp does; its values get no numbers either.
	 */
	bool		probe = state->setop >= 0 && state->side == 1 && !state->replaying &&
		!state->frozen && state->spill == NULL;

	reserve_rows(state, nrows);
	if (!state->replaying)
		agg_read_in_order(state, batch);
	memset(state->valid_bits, 0, sizeof(uint64) * nwords);
	memset(state->inserted_bits, 0, sizeof(uint64) * nwords);
	valid = (TessRowMask) {nrows, state->valid_bits};
	pending = (TessRowMask) {nrows, state->pending_bits};
	inserted = (TessRowMask) {nrows, state->inserted_bits};
	for (int key = 0; key < state->nkeys; key++)
	{
		TessDatumColumn *column = &state->key_columns[key];
		bool		int8 = state->kinds[key] == TESS_TABLE_KEY_INT8;
		KeyDict    *dict = state->dicts[key];

		computed_column(state, batch, key, TESS_COLUMN_FOR_FILTER, false, column);
		/* A key through a dictionary: the table groups by its values' numbers. */
		if (dict != NULL)
		{
			if (dict->capacity < nrows)
			{
				MemoryContext query = state->css.ss.ps.state->es_query_cxt;

				dict->capacity = nrows;
				dict->batch_numbers = MemoryContextAlloc(query, sizeof(Datum) * nrows);
				dict->batch_hashes = MemoryContextAlloc(query, sizeof(uint32) * nrows);
			}
			agg_keydict_numbers(dict, column, &batch->rows, !state->frozen && !probe,
							dict->batch_numbers, dict->batch_hashes);
			state->number_columns[key] = *column;
			state->number_columns[key].values = dict->batch_numbers;
			column = &state->number_columns[key];
		}
		if (key == 0)
			check(state, (int8 ? state->kernels->int8_hash :
						  state->kernels->int4_hash) (column, NULL, &batch->rows,
													  TESS_NULL_KEYS_GROUP,
													  state->hashes, &valid,
													  &state->status));
		else
			check(state, (int8 ? state->kernels->int8_hash_next :
						  state->kernels->int4_hash_next) (column, NULL,
														   TESS_NULL_KEYS_GROUP,
														   state->hashes, &valid,
														   &state->status));
		state->table_keys[key].kind = state->kinds[key];
		state->table_keys[key].column = column;
		state->table_keys[key].prepared = NULL;
	}
	/* A frozen table's rows spill by their values' hashes. */
	if (state->has_dicts && state->frozen)
	{
		int			row = -1;

		if (state->value_hash_rows < nrows)
		{
			state->value_hash_rows = nrows;
			state->value_hashes = MemoryContextAlloc(state->css.ss.ps.state->es_query_cxt,
													 sizeof(uint32) * nrows);
		}
		while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
		{
			uint32		hash = 0;

			for (int key = 0; key < state->nkeys; key++)
			{
				const TessDatumColumn *column = &state->key_columns[key];
				uint32		part = state->dicts[key] != NULL ?
					state->dicts[key]->batch_hashes[row] :
					column->isnull[row] ? 0 :
					(uint32) murmurhash64((uint64) column->values[row]);

				hash = hash_combine(hash, part);
			}
			state->value_hashes[row] = hash;
		}
	}
	memcpy(state->pending_bits, state->valid_bits, sizeof(uint64) * nwords);
	/*
	 * A frozen table takes no new group: the rows of the groups it has go
	 * on into them, the others, their computed values, to disk.
	 */
	if (state->frozen)
	{
		uint64	   *missing_bits;
		TessRowMask missing;

		if (state->missing_words < nwords)
		{
			state->missing_words = nwords;
			state->missing_bits = state->missing_bits == NULL ?
				MemoryContextAlloc(state->css.ss.ps.state->es_query_cxt,
								   sizeof(uint64) * nwords) :
				repalloc(state->missing_bits, sizeof(uint64) * nwords);
		}
		missing_bits = state->missing_bits;
		missing = (TessRowMask) {nrows, missing_bits};

		check(state, state->kernels->table_probe(&state->table, state->hashes, state->nkeys,
												 state->table_keys, &valid, state->offsets,
												 &pending, &state->status));
		for (int word = 0; word < nwords; word++)
		{
			missing_bits[word] = state->valid_bits[word] & ~state->pending_bits[word];
			state->valid_bits[word] = state->pending_bits[word];
		}
		for (int column = 0; column < state->ncomputed; column++)
		{
			if (column < state->nkeys)
				state->computed_columns[column] = state->key_columns[column];
			else
				computed_column(state, batch, column, TESS_COLUMN_FOR_PROJECTION, false,
								&state->computed_columns[column]);
		}
		rows_write(state, state->rows_spill, &missing);
	}
	else if (probe)
	{
		/* No chunk yet: an empty left side, no group to find. */
		if (state->table.nchunks == 0)
			memset(state->valid_bits, 0, sizeof(uint64) * nwords);
		else
		{
			check(state, state->kernels->table_probe(&state->table, state->hashes,
													 state->nkeys, state->table_keys,
													 &valid, state->offsets, &pending,
													 &state->status));
			memcpy(state->valid_bits, state->pending_bits, sizeof(uint64) * nwords);
		}
	}
	else if (state->spill != NULL)
		agg_find_partitioned(state, &pending, &inserted);
	else if (state->table.nchunks == 0)
		add_chunk(state);
	for (; !probe && state->spill == NULL && !state->frozen;)
	{
		TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
		TessRowMask call = {nrows, state->call_bits};

		/*
		 * Each call fills its mask of new groups whole: they add up, or the
		 * groups made before a chunk ran out would miss their initial
		 * states. A mask has no bits past its rows on entry, which a longer
		 * batch left.
		 */
		memset(state->call_bits, 0, sizeof(uint64) * nwords);
		check(state, state->kernels->table_find_or_insert(&state->table,
														  state->table.nchunks - 1,
														  state->hashes,
														  state->nkeys,
														  state->table_keys,
														  &pending,
														  state->offsets,
														  &call,
														  &state->status));
		for (int word = 0; word < nwords; word++)
			state->inserted_bits[word] |= state->call_bits[word];
		if (tess_row_mask_count(&pending) == 0)
			break;
		/*
		 * The rows left pending find room in a larger index, when the
		 * groups reached half the buckets, or else in another chunk.
		 */
		check(state, state->kernels->table_stats(&state->table, &stats,
												 &state->status));
		if (stats.records * 2 >= stats.buckets)
			regrow_table(state, stats.records);
		else
			add_chunk(state);
	}
	if (state->has_forms)
		agg_key_forms(state, &inserted);
	nsums = 0;
	for (int index = 0; index < state->nvalues; index++)
	{
		AggValue   *value = &state->values[index];
		TessDatumColumn column;

		/* FILTER: the rows it keeps; the groups those rows made count still. */
		TessRowMask rows = value->filter >= 0 ?
			agg_filtered_rows(state, batch, value->filter, &valid) : valid;

		if (value->computed >= 0)
			computed_column(state, batch, value->computed,
							TESS_COLUMN_FOR_PROJECTION, fast_decimals(value), &column);
		if (value->generic != NULL)
		{
			value->generic->columns[0] = column;
			for (int arg = 1; arg < value->generic->nargs; arg++)
				computed_column(state, batch, value->computed + arg,
								TESS_COLUMN_FOR_PROJECTION, false,
								&value->generic->columns[arg]);
			/* The sum states over every valid row go in one call, below. */
			if (value->generic->sum_state && value->filter < 0 && value->distinct == NULL)
			{
				state->sum_indexes[nsums++] = index;
				continue;
			}
			if (value->distinct != NULL)
				rows = agg_distinct_rows(state, value, nrows, state->hashes, &rows, &column);
			agg_generic_group_accumulate(state, index, &rows, &inserted);
			continue;
		}
		if (value->distinct != NULL)
			rows = agg_distinct_rows(state, value, nrows, state->hashes, &rows,
								 &column);
		state->calls++;
		check(state, state->kernels->table_accumulate(&state->table,
													  state->offsets, &rows,
													  value->accumulate,
													  value->computed >= 0 ? &column : NULL,
													  NULL,
													  sizeof(uint64) * value->slot,
													  0, (uint32) index,
													  &state->status));
	}
#ifdef HAVE_INT128
	if (nsums > 0)
		agg_sum_states_accumulate(state, nsums, state->sum_indexes, &valid);
#endif
	/*
	 * Past seven eighths of hash_mem, the rest left for a batch's chunk and
	 * index: the groups go into partitions, and the largest to disk; in
	 * partial mode they go out instead (agg_group_drain).
	 */
	if (state->spill == NULL && (!state->partial || state->partial_spill) &&
		!state->has_distinct && !state->row_spill &&
		state->table_bytes > get_hash_memory_limit() / 8 * 7)
		agg_start_spill(state);
	/*
	 * Generic states past hash_mem: the table freezes, new groups' rows go
	 * to disk; in partial mode the groups go out instead (agg_group_drain).
	 */
	if (state->row_spill && !state->partial && !state->has_distinct && !state->frozen &&
		state->rows_level < ROWS_MAX_LEVELS &&
		groups_memory(state) > get_hash_memory_limit() / 8 * 7)
	{
		TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);

		/*
		 * A table of fewer groups than a batch's is its own overhead past a
		 * tiny hash_mem, not groups too many: freezing it would split every
		 * partition again, level after level.
		 */
		check(state, state->kernels->table_stats(&state->table, &stats, &state->status));
		if (stats.records >= AGG_GROUP_ROWS)
		{
			state->frozen = true;
			state->rows_spill = rows_spill_create(state, state->rows_level);
		}
	}
	if (state->spill != NULL)
		agg_make_room(state);
}

/* Read every batch of the child into the table of groups. */
/* Read side `side` of INTERSECT or EXCEPT from now on. */
void
agg_setop_side(TessAggState *state, int side)
{
	if (side == 0)
		state->setop_left_rows = 0;
	state->side = side;
	state->child = state->sides[side];
	state->input = state->side_inputs[side];
	state->projection = state->side_projections[side];
	state->child_layout = state->side_layouts[side];
}

void
agg_group_drain(TessAggState *state)
{
	agg_spill_free(state);
	rows_spill_free(state);
	state->rows_level = 0;
	for (int key = 0; key < state->nkeys; key++)
		if (state->dicts[key] != NULL)
			agg_key_dict_reset(state->dicts[key], state->groups_estimate);
	/* The groups of a previous table and their states go together. */
	if (state->generic_agg != NULL)
		ReScanExprContext(state->generic_agg->curaggcontext);
	create_table(state);
	agg_reset_distinct(state);
	for (;;)
	{
		TessBatch  *batch;
		int			rows;

		/*
		 * Partial mode: a table near hash_mem, the states' memory counted,
		 * goes out now, as partials the Finalize Aggregate merges, and the
		 * input goes on after it.
		 */
		if (state->partial && !state->partial_spill &&
			groups_memory(state) > get_hash_memory_limit() / 8 * 7)
		{
			TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);

			check(state, state->kernels->table_stats(&state->table, &stats,
													 &state->status));
			/*
			 * More groups than half the rows read since the table started:
			 * sending it up would fold nothing. The groups go into
			 * partitions and to disk from now on, and out once the input is
			 * done, still as partials. Sum states spill no state (their
			 * records merge a word an aggregate): they go up still.
			 */
			if (!state->row_spill && stats.records * 2 > state->rows - state->emit_rows)
			{
				state->partial_spill = true;
				agg_start_spill(state);
				continue;
			}
			state->emit_rows = state->rows;
			state->early_emits++;
			state->drained = true;
			state->cursor = 0;
			return;
		}
		batch = tess_input_next(state->input);
		/*
		 * INTERSECT or EXCEPT: the right side after the left. An empty left
		 * side makes no group, and its right side is not read, as the
		 * core's SetOp does not read its inner input: an error the right
		 * side's rows would raise is not raised either.
		 */
		if (batch == NULL && state->setop >= 0 && state->side == 0)
		{
			if (state->setop_left_rows == 0)
				break;
			agg_setop_side(state, 1);
			continue;
		}
		if (batch == NULL)
			break;
		rows = tess_row_mask_count(&batch->rows);
		if (state->setop >= 0 && state->side == 0)
			state->setop_left_rows += rows;
		state->batches++;
		state->rows += rows;
		if (rows > 0)
		{
			TessBatch  *input = tess_projection_wrap(state->projection, batch);

			ResetExprContext(state->css.ss.ps.ps_ExprContext);
			group_batch(state, input);
			input->ops->release(input);
		}
		tess_input_finish(state->input);
	}
	state->drained = true;
	state->input_done = true;
	state->cursor = 0;
	if (state->spill != NULL)
		agg_finish_input(state);
}

/*
 * Aggregate index of a group into the scan slot: a count as it is, a sum
 * or an extreme NULL without the flag of a value, an int4 extreme as an
 * int4 Datum.
 */
static void
group_value_into(TessAggState *state, int index, int group, Datum *datum, bool *isnull)
{
	AggValue   *value = &state->values[index];
	uint64		word = state->state_words[index * AGG_GROUP_ROWS + group];
	bool		seen = ((state->flag_words[group] >> index) & 1) != 0;

	*isnull = value->kind != AGG_COUNT && !seen;
	switch (value->kind)
	{
		case AGG_COUNT:
		case AGG_SUM:
			*datum = Int64GetDatum((int64) word);
			break;
		case AGG_MIN:
		case AGG_MAX:
			*datum = value->wide ?
				Int64GetDatum((int64) word) : Int32GetDatum((int32) (int64) word);
			break;
		case AGG_GENERIC:
			{
				MemoryContext old = MemoryContextSwitchTo(state->generic_output);

#ifdef HAVE_INT128
				if (value->generic->sum_state)
				{
					const uint64 *words = agg_record_payload(state, state->walked[group]) +
						value->slot;

					*datum = state->partial ?
						agg_sum_state_partial(value->generic, words, isnull) :
						agg_sum_state_value(value->generic, words, isnull);
					MemoryContextSwitchTo(old);
					break;
				}
				if (value->generic->extreme_state)
				{
					const uint64 *words = agg_record_payload(state, state->walked[group]) +
						value->slot;

					*datum = agg_extreme_state_value(words, isnull);
					MemoryContextSwitchTo(old);
					break;
				}
#endif
				value->generic->state = (Datum) word;
				value->generic->state_null = !seen;
				*datum = agg_generic_value(value->generic, isnull);
				MemoryContextSwitchTo(old);
				break;
			}
	}
}

static void
group_value(TessAggState *state, int index, int group, TupleTableSlot *scan)
{
	int			attribute = state->nkeys + index;

	group_value_into(state, index, group, &scan->tts_values[attribute],
					 &scan->tts_isnull[attribute]);
}

/*
 * The rows of the next partition into a table of their own, their groups
 * from the initial states; false when no partition is left.
 */
static bool
rows_drain(TessAggState *state)
{
	TessBatch  *batch;

	rows_spill_close(state);
	if (state->reader == NULL)
		rows_reader_init(state);
	if (!rows_next_partition(state))
		return false;
	if (state->generic_agg != NULL)
		ReScanExprContext(state->generic_agg->curaggcontext);
	for (int key = 0; key < state->nkeys; key++)
		if (state->dicts[key] != NULL)
			agg_key_dict_reset(state->dicts[key], 256);
	create_table(state);
	state->frozen = false;
	state->replaying = true;
	while ((batch = reader_next(state)) != NULL)
	{
		ResetExprContext(state->css.ss.ps.ps_ExprContext);
		group_batch(state, batch);
	}
	state->replaying = false;
	state->cursor = 0;
	return true;
}

/*
 * The walk's next groups, up to a batch of them: their keys (a number as
 * its value), flags and states gathered into the node's arrays; 0 when
 * the walk is over, partitions and partial tables included.
 */
static int
next_chunk(TessAggState *state)
{
	for (;;)
	{
		uint64		all;
		TessRowMask groups;
		int			count;

		/*
		 * The groups go out without a return to the executor while HAVING
		 * rejects them, and partitions merge on the way: a chunk at a time.
		 */
		CHECK_FOR_INTERRUPTS();

		/* A table that spilled has no index once every partition is out. */
		if (state->table.index == NULL)
			return 0;
		check(state, state->kernels->table_scan(&state->table,
												&state->cursor, state->walked,
												AGG_GROUP_ROWS, &count,
												&state->status));
		if (count == 0)
		{
			/* Partial mode: the rest of the input into a table anew. */
			if (state->spill == NULL && !state->input_done)
			{
				agg_group_drain(state);
				continue;
			}
			/* The next partition of rows of groups a frozen table lacked. */
			if ((state->rows_spill != NULL || state->rows_pending != NIL) &&
				rows_drain(state))
				continue;
			/* The next partition of a table that spilled. */
			if (state->spill == NULL || !agg_advance(state))
				return 0;
			state->cursor = 0;
			continue;
		}
		state->groups += count;
		all = count == 64 ? UINT64_MAX : (UINT64CONST(1) << count) - 1;
		groups = (TessRowMask) {count, &all};
		for (int key = 0; key < state->nkeys; key++)
		{
			check(state, state->kernels->table_gather_key(&state->table,
														  state->walked, &groups,
														  key, state->key_values[key],
														  state->key_isnull[key],
														  &state->status));
			/* A number goes out as its value, or as the group's own form. */
			if (state->dicts[key] != NULL)
			{
				KeyDict    *dict = state->dicts[key];

				for (int group = 0; group < count; group++)
					if (!state->key_isnull[key][group])
						state->key_values[key][group] =
							dict->values[DatumGetInt64(state->key_values[key][group])];
				for (int group = 0; dict->form_table != NULL && group < count; group++)
				{
					KeyForm    *form = keyform_lookup(dict->form_table, state->walked[group]);

					if (form != NULL)
						state->key_values[key][group] = form->value;
				}
			}
		}
		check(state, state->kernels->table_gather(&state->table,
												  state->walked, &groups, 0,
												  (Datum *) state->flag_words,
												  &state->status));
		for (int index = 0; index < state->nvalues; index++)
			check(state, state->kernels->table_gather(&state->table,
													  state->walked, &groups,
													  sizeof(uint64) * state->values[index].slot,
													  (Datum *) &state->state_words[index * AGG_GROUP_ROWS],
													  &state->status));
		return count;
	}
}

/*
 * The copies of a group INTERSECT or EXCEPT puts out, from its rows and
 * its right side's: EXCEPT a group of the left side alone, INTERSECT one
 * of both, and with ALL as many as the left side's rows exceed the
 * right's, or the fewer of the two.
 */
static int64
setop_copies(TessAggState *state, int group)
{
	int64		rows = (int64) state->state_words[0 * AGG_GROUP_ROWS + group];
	int64		right = (int64) state->state_words[1 * AGG_GROUP_ROWS + group];
	int64		left = rows - right;

	switch ((SetOpCmd) state->setop)
	{
		case SETOPCMD_EXCEPT:
			return left > 0 && right == 0 ? 1 : 0;
		case SETOPCMD_EXCEPT_ALL:
			return Max(left - right, 0);
		case SETOPCMD_INTERSECT:
			return left > 0 && right > 0 ? 1 : 0;
		case SETOPCMD_INTERSECT_ALL:
			return Min(left, right);
	}
	return 0;
}

/*
 * The next rows of INTERSECT or EXCEPT, up to a batch: each group of the
 * walk its copies, a group's copies going on into the next batch when
 * they do not fit; NULL at the end.
 */
static TessBatch *
setop_groups(TessAggState *state)
{
	TupleTableSlot *scan = state->css.ss.ss_ScanTupleSlot;
	int			emitted = 0;

	tess_builder_reset(state->builder);
	for (;;)
	{
		int			group;
		TupleTableSlot *row;

		if (state->setop_group >= state->setop_count)
		{
			/*
			 * The rows so far go first: the next groups may come from a
			 * partition whose reading frees the values they point into.
			 */
			if (emitted > 0)
				return tess_builder_finish(state->builder, InvalidOid);
			state->setop_count = next_chunk(state);
			state->setop_group = 0;
			state->setop_copies = -1;
			if (state->setop_count == 0)
				return tess_builder_finish(state->builder, InvalidOid);
		}
		group = state->setop_group;
		if (state->setop_copies < 0)
			state->setop_copies = setop_copies(state, group);
		if (state->setop_copies > 0)
		{
			ExecClearTuple(scan);
			for (int key = 0; key < state->nkeys; key++)
			{
				scan->tts_values[key] = state->key_values[key][group];
				scan->tts_isnull[key] = state->key_isnull[key][group];
			}
			for (int index = 0; index < state->nvalues; index++)
				group_value(state, index, group, scan);
			ExecStoreVirtualTuple(scan);
			ResetExprContext(state->css.ss.ps.ps_ExprContext);
			state->css.ss.ps.ps_ExprContext->ecxt_scantuple = scan;
			row = state->css.ss.ps.ps_ProjInfo != NULL ?
				ExecProject(state->css.ss.ps.ps_ProjInfo) :
				ExecCopySlot(state->css.ss.ps.ps_ResultTupleSlot, scan);
			while (state->setop_copies > 0 && emitted < AGG_GROUP_ROWS)
			{
				tess_builder_append_slot(state->builder, row);
				state->setop_copies--;
				emitted++;
			}
		}
		if (state->setop_copies == 0)
		{
			state->setop_group++;
			state->setop_copies = -1;
		}
		if (emitted == AGG_GROUP_ROWS)
			return tess_builder_finish(state->builder, InvalidOid);
	}
}

/*
 * The next groups of the walk, up to a batch of them, as result rows:
 * the keys and the aggregates in the scan slot, HAVING over them and the
 * plan's projection, as for the one row without GROUP BY. NULL when the
 * walk is over; a batch HAVING left empty is not returned.
 */
TessBatch *
agg_next_groups(TessAggState *state)
{
	ExprContext *econtext = state->css.ss.ps.ps_ExprContext;
	TupleTableSlot *scan = state->css.ss.ss_ScanTupleSlot;

	if (state->setop >= 0)
		return setop_groups(state);
	/* The walk's arrays are the batch's columns. */
	if (state->direct)
	{
		int			count = next_chunk(state);

		if (count == 0)
			return NULL;
		for (int index = 0; index < state->nvalues; index++)
			for (int group = 0; group < count; group++)
				group_value_into(state, index, group,
								 &state->agg_values[index * AGG_GROUP_ROWS + group],
								 &state->agg_isnull[index * AGG_GROUP_ROWS + group]);
		state->groups_bits[0] = count == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << count) - 1;
		state->groups_batch.rows.nrows = count;
		state->groups_batch.rows.bits = state->groups_bits;
		return &state->groups_batch;
	}
	for (;;)
	{
		int			count = next_chunk(state);
		TessBatch  *batch;

		if (count == 0)
			return NULL;
		tess_builder_reset(state->builder);
		for (int group = 0; group < count; group++)
		{
			TupleTableSlot *row;

			ExecClearTuple(scan);
			if (state->generic_output != NULL)
				MemoryContextReset(state->generic_output);
			for (int key = 0; key < state->nkeys; key++)
			{
				scan->tts_values[key] = state->key_values[key][group];
				scan->tts_isnull[key] = state->key_isnull[key][group];
			}
			for (int index = 0; index < state->nvalues; index++)
				group_value(state, index, group, scan);
			ExecStoreVirtualTuple(scan);
			ResetExprContext(econtext);
			econtext->ecxt_scantuple = scan;
			if (state->css.ss.ps.qual != NULL &&
				!ExecQual(state->css.ss.ps.qual, econtext))
				continue;
			row = state->css.ss.ps.ps_ProjInfo != NULL ?
				ExecProject(state->css.ss.ps.ps_ProjInfo) :
				ExecCopySlot(state->css.ss.ps.ps_ResultTupleSlot, scan);
			tess_builder_append_slot(state->builder, row);
		}
		batch = tess_builder_finish(state->builder, InvalidOid);
		if (batch != NULL)
			return batch;
	}
}

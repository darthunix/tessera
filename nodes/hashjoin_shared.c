#include "postgres.h"

#include "miscadmin.h"
#include "utils/memutils.h"
#include "storage/barrier.h"
#include "utils/dsa.h"
#include "utils/wait_event.h"

#include "tessera/runtime.h"

#include "internal.h"
#include "hashjoin.h"

/*
 * The query's dynamic shared memory, where a shared build keeps its
 * table: the Gather installs it only while it runs the plan, so it is
 * kept for leaving the build and freeing the table at shutdown, as the
 * core's parallel hash join keeps it.
 */
dsa_area *
query_dsa(TessHashJoinState *state)
{
	dsa_area   *area = state->css.ss.ps.state->es_query_dsa;

	if (area != NULL)
		state->parallel.area = area;
	if (state->parallel.area == NULL)
		elog(ERROR, "TessHashJoin found no shared memory for its shared table");
	return state->parallel.area;
}

/*
 * RIGHT and FULL: the marks of a shared table of nchunks chunks, at
 * `marks` in the query's shared memory, which every participant sets.
 */
static void
share_marks(TessHashJoinState *state, dsa_pointer marks, int nchunks)
{
	uint64	   *words;

	forget_marks(state);
	if (!state->preserve_inner)
		return;
	if (!DsaPointerIsValid(marks))
		elog(ERROR, "TessHashJoin found no marks of its shared table");
	if (state->marks_context == NULL)
		state->marks_context = AllocSetContextCreate(state->css.ss.ps.state->es_query_cxt,
													 "TessHashJoin marks",
													 ALLOCSET_DEFAULT_SIZES);
	state->marks = MemoryContextAlloc(state->marks_context,
									  sizeof(uint64 *) * Max(nchunks, 1));
	words = dsa_get_address(query_dsa(state), marks);
	for (int chunk = 0; chunk < nchunks; chunk++)
		state->marks[chunk] = words + chunk * mark_words(state);
	state->mark_slots = nchunks;
	state->marks_shared = true;
}

/* The marks of a shared table of nchunks chunks, none set, for the elected participant. */
static dsa_pointer
allocate_marks(TessHashJoinState *state, int nchunks)
{
	if (!state->preserve_inner)
		return InvalidDsaPointer;
	return dsa_allocate_extended(query_dsa(state),
								 mul_size(mul_size(Max(nchunks, 1), mark_words(state)),
										  sizeof(uint64)),
								 DSA_ALLOC_ZERO | DSA_ALLOC_HUGE);
}

/*
 * The shared table as the elected participant published it: its index,
 * and the bases of its chunks in this process, from the directory.
 */
static void
attach_shared_table(TessHashJoinState *state)
{
	dsa_area   *area = query_dsa(state);
	int			nchunks = state->parallel.shared->nchunks;
	dsa_pointer *bases;
	Size	   *lens;

	/* SIZE published both before the barrier that led here. */
	Assert(DsaPointerIsValid(state->parallel.shared->index));
	Assert(DsaPointerIsValid(state->parallel.shared->directory));
	state->table.index = dsa_get_address(area, state->parallel.shared->index);
	state->table.index_len = state->parallel.shared->index_len;
	reserve_chunks(state, Max(nchunks, 1));
	bases = dsa_get_address(area, state->parallel.shared->directory);
	lens = (Size *) (bases + nchunks);
	for (int chunk = 0; chunk < nchunks; chunk++)
	{
		state->chunk_bases[chunk] = dsa_get_address(area, bases[chunk]);
		state->chunk_lens[chunk] = lens[chunk];
	}
	state->table.nchunks = nchunks;
	/* Every participant's value chunks, which a gather reads. */
	reserve_values(state, Max(state->parallel.shared->nvalue_chunks, 1));
	bases = dsa_get_address(area, state->parallel.shared->value_directory);
	for (int chunk = 0; chunk < state->parallel.shared->nvalue_chunks; chunk++)
		state->values.bases[chunk] = dsa_get_address(area, bases[chunk]);
	state->values.nchunks = state->parallel.shared->nvalue_chunks;
}

/*
 * A cleared shared Bloom filter for a table of `records` records, in
 * place of the one before; only the elected participant, whom the build
 * barrier separates from the probes, calls it.
 */
static void
allocate_shared_filter(TessHashJoinState *state, uint64 records)
{
	dsa_area   *area = query_dsa(state);
	Size		nwords;

	if (DsaPointerIsValid(state->parallel.shared->filter))
		dsa_free(area, state->parallel.shared->filter);
	check(state, state->kernels->bloom_shared_words(records, &nwords, &state->status));
	state->parallel.shared->filter = dsa_allocate_extended(area, mul_size(sizeof(uint64), nwords),
												  DSA_ALLOC_HUGE);
	state->parallel.shared->filter_words = nwords;
	check(state, state->kernels->bloom_shared_init(dsa_get_address(area,
																   state->parallel.shared->filter),
												   nwords, &state->status));
}

/* The memory a shared build's elected participant holds, in bytes. */
static void
note_shared_memory(TessHashJoinState *state, Size bytes)
{
	bytes = add_size(bytes, mul_size(sizeof(uint64), state->parallel.shared->filter_words));
	state->peak_memory = Max(state->peak_memory, bytes);
}

/*
 * BUILD: another chunk of this participant's, the last one being full:
 * numbered by the build counters and entered in the table's list.
 */
static void
add_own_chunk(TessHashJoinState *state)
{
	dsa_area   *area = query_dsa(state);
	/* A small hash_mem takes small chunks: they count against it before the table spills. */
	Size		len = state->parallel.nown == 0 ? JOIN_FIRST_CHUNK : chunk_len_for(JOIN_CHUNK_LEN);
	uint64		number;
	dsa_pointer block;
	JoinChunk  *header;

	check(state, state->kernels->build_take_chunk(state->parallel.shared->counters, &number,
												  &state->status));
	if (number >= TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin hash table cannot hold more than %d chunks",
						TESS_TABLE_MAX_CHUNKS)));
	if (state->parallel.nown == state->parallel.own_slots)
	{
		MemoryContext context = state->css.ss.ps.state->es_query_cxt;
		int			slots = Max(state->parallel.own_slots * 2, 8);

		state->parallel.own_chunks = state->parallel.own_chunks == NULL ?
			MemoryContextAlloc(context, sizeof(int) * slots) :
			repalloc(state->parallel.own_chunks, sizeof(int) * slots);
		state->parallel.own_slots = slots;
	}
	block = dsa_allocate_extended(area, JOIN_CHUNK_HEADER + len, DSA_ALLOC_HUGE);
	header = dsa_get_address(area, block);
	header->number = number;
	header->len = len;
	header->owner = state->parallel.spill_participant;
	state->parallel.own_base = (char *) header + JOIN_CHUNK_HEADER;
	state->parallel.own_len = len;
	check(state, state->kernels->table_chunk_init(state->parallel.own_base, len,
												  &state->status));
	header->next = *own_list(state, false);
	*own_list(state, false) = block;
	state->parallel.own_chunks[state->parallel.nown++] = (int) number;
	state->parallel.own_bytes += JOIN_CHUNK_HEADER + len;
	shared_count(state, record_chunk_cost(len, state->record_size));
	state->counters[JOIN_CHUNKS]++;
}

/*
 * BUILD: append the rows of one inner batch to this participant's
 * chunks, adding chunks until they fit. The index is not made yet: the
 * table seen here is the one chunk appended to.
 */
static void
insert_shared_batch(TessHashJoinState *state, TessBatch *batch)
{
	TessRowMask pending;
	int			count = prepare_inner(state, batch, &pending);
	bool		fresh = false;

	if (count == 0)
		return;
	if (state->parallel.nown == 0)
	{
		add_own_chunk(state);
		fresh = true;
	}
	for (;;)
	{
		TessTableRef own = {NULL, 0, &state->parallel.own_base, &state->parallel.own_len, 1};
		bool		appended = append_rows(state, &own, 0, &pending);

		if (tess_row_mask_count(&pending) == 0)
			break;
		if (!appended && fresh)
			elog(ERROR, "TessHashJoin cannot fit a row of its table in a chunk");
		add_own_chunk(state);
		fresh = true;
	}
	state->parallel.appended += count;
	state->build_rows += count;
	state->counters[JOIN_BUILD_ROWS] += count;
	state->peak_memory = Max(state->peak_memory,
							 add_size(state->parallel.own_bytes, state->values.bytes));
}

/* BUILD: this participant's share of the inner side, then its report. */
static void
build_shared_inner(TessHashJoinState *state)
{
	for (;;)
	{
		TessBatch  *batch = tess_input_next(state->inner_input);

		if (batch == NULL)
			break;
		if (tess_row_mask_count(&batch->rows) > 0)
		{
			note_prune_keys(state, batch);
			if (state->spill != NULL)
				insert_spill(state, batch);
			else
				insert_shared_batch(state, batch);
			shared_check(state);
		}
		tess_input_finish(state->inner_input);
	}
	share_prune_keys(state);
	check(state, state->kernels->build_report(state->parallel.shared->counters,
											  state->parallel.appended,
											  state->null_columns,
											  &state->status));
}

/*
 * SIZE: the index for exactly the records appended, the directory of the
 * chunks by number, and the filter, as the elected participant. A table
 * that spilled holds the records of the partitions in memory only, in
 * chunks no counter numbered: they are numbered here, in the lists'
 * order; the value chunks of the partitions on disk leave holes in the
 * values' directory.
 */
static void
size_shared_table(TessHashJoinState *state)
{
	Size		payload_size = sizeof(uint64) * (1 + state->npayload);
	dsa_area   *area = query_dsa(state);
	TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
	bool		split = shared_partitions(state) > 0;
	uint64		records;
	uint64		nulls;
	uint64		nchunks;
	uint64		capacity;
	dsa_pointer *bases;
	Size	   *lens;
	dsa_pointer block;
	Size		size;

	/* The elected one alone, once per build. */
	Assert(!DsaPointerIsValid(state->parallel.shared->index));
	Assert(!DsaPointerIsValid(state->parallel.shared->directory));
	check(state, state->kernels->build_totals(state->parallel.shared->counters, &records,
											  &nulls, &nchunks, NULL,
											  &state->status));
	if (split)
	{
		records = 0;
		for (uint32 partition = 0; partition < shared_partitions(state); partition++)
		{
			uint64		held;

			if (shared_on_disk(state, partition))
				continue;
			check(state, state->kernels->table_spill_records(shared_words(state),
															 state->parallel.shared->spill_nwords,
															 partition, 0, &held,
															 &state->status));
			records += held;
		}
		nchunks = 0;
		for (int participant = 0; participant < state->parallel.shared->participants; participant++)
			for (block = *participant_list(state, participant, false);
				 DsaPointerIsValid(block);)
			{
				JoinChunk  *header = dsa_get_address(area, block);

				header->number = nchunks++;
				block = header->next;
			}
		if (nchunks > TESS_TABLE_MAX_CHUNKS)
			ereport(ERROR,
					(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
					 errmsg("TessHashJoin hash table cannot hold more than %d chunks",
							TESS_TABLE_MAX_CHUNKS)));
	}
	state->parallel.shared->resident_rows = records;
	capacity = Max(records, JOIN_INITIAL_ROWS);
	check(state, state->kernels->table_size(state->keys.nkeys, state->keys.inner_kinds,
											payload_size, capacity,
											&size, &state->status));
	state->parallel.shared->index = dsa_allocate_extended(area, size, DSA_ALLOC_HUGE);
	state->parallel.shared->index_len = size;
	check(state, state->kernels->table_create(dsa_get_address(area, state->parallel.shared->index),
											  size, state->keys.nkeys, state->keys.inner_kinds,
											  payload_size, capacity,
											  &state->status));
	/* The directory: every chunk under the number it took, none left out. */
	state->parallel.shared->directory =
		dsa_allocate_extended(area,
							  mul_size(Max(nchunks, 1), sizeof(dsa_pointer) + sizeof(Size)),
							  DSA_ALLOC_ZERO);
	bases = dsa_get_address(area, state->parallel.shared->directory);
	lens = (Size *) (bases + nchunks);
	for (int participant = 0; participant < state->parallel.shared->participants; participant++)
		for (block = *participant_list(state, participant, false);
			 DsaPointerIsValid(block);)
		{
			JoinChunk  *header = dsa_get_address(area, block);

			if (header->number >= nchunks || lens[header->number] != 0)
				elog(ERROR, "TessHashJoin found chunk %llu out of the directory",
					 (unsigned long long) header->number);
			bases[header->number] = block + JOIN_CHUNK_HEADER;
			lens[header->number] = header->len;
			block = header->next;
		}
	for (uint64 chunk = 0; chunk < nchunks; chunk++)
		if (lens[chunk] == 0)
			elog(ERROR, "TessHashJoin is missing chunk %llu of its table",
				 (unsigned long long) chunk);
	state->parallel.shared->nchunks = (int) nchunks;
	state->parallel.shared->marks = allocate_marks(state, (int) nchunks);
	/* The value chunks' directory: dsa_pointers of their bases by number. */
	state->parallel.shared->nvalue_chunks = (int) state->parallel.shared->next_value_chunk;
	state->parallel.shared->value_directory =
		dsa_allocate_extended(area,
							  mul_size(Max(state->parallel.shared->nvalue_chunks, 1),
									   sizeof(dsa_pointer)),
							  DSA_ALLOC_ZERO | DSA_ALLOC_HUGE);
	bases = dsa_get_address(area, state->parallel.shared->value_directory);
	for (int participant = 0; participant < state->parallel.shared->participants; participant++)
		for (block = *participant_list(state, participant, true);
			 DsaPointerIsValid(block);)
		{
			JoinChunk  *header = dsa_get_address(area, block);

			if (header->number >= (uint64) state->parallel.shared->nvalue_chunks ||
				DsaPointerIsValid(bases[header->number]))
				elog(ERROR, "TessHashJoin found value chunk %llu out of the directory",
					 (unsigned long long) header->number);
			bases[header->number] = block + JOIN_CHUNK_HEADER;
			block = header->next;
		}
	for (int chunk = 0; chunk < state->parallel.shared->nvalue_chunks && !split; chunk++)
		if (!DsaPointerIsValid(bases[chunk]))
			elog(ERROR, "TessHashJoin is missing value chunk %d of its table", chunk);
	if (split)
		make_rounds(state);
	attach_shared_table(state);
	check(state, state->kernels->table_stats(&state->table, &stats,
											 &state->status));
	allocate_shared_filter(state, capacity);
	note_shared_memory(state, add_size(size, add_size(state->parallel.own_bytes,
													  state->values.bytes)));
	state->counters[JOIN_BUILDS]++;
	state->counters[JOIN_BUCKETS] += stats.buckets;
}

/*
 * LINK: this participant's own chunks into the index, counting the
 * records whose keys the table held already unless the planner knows the
 * inner side unique: the probes then skip the rounds a table without
 * duplicates has no use for.
 */
static void
link_own_chunks(TessHashJoinState *state)
{
	uint64		duplicates = 0;

	attach_shared_table(state);
	/* A table that spilled: this participant's chunks of the partitions in memory, as SIZE numbered them. */
	if (shared_partitions(state) > 0)
	{
		state->parallel.nown = 0;
		for (dsa_pointer block = *own_list(state, false); DsaPointerIsValid(block);)
		{
			JoinChunk  *header = dsa_get_address(query_dsa(state), block);

			if (state->parallel.nown == state->parallel.own_slots)
			{
				int			slots = Max(state->parallel.own_slots * 2, 8);

				state->parallel.own_chunks = state->parallel.own_chunks == NULL ?
					MemoryContextAlloc(state->css.ss.ps.state->es_query_cxt,
									   sizeof(int) * slots) :
					repalloc(state->parallel.own_chunks, sizeof(int) * slots);
				state->parallel.own_slots = slots;
			}
			state->parallel.own_chunks[state->parallel.nown++] = (int) header->number;
			block = header->next;
		}
	}
	for (int own = 0; own < state->parallel.nown; own++)
	{
		Size		from = TESS_TABLE_CHUNK_HEADER;
		uint64		repeated = 0;

		Assert(state->parallel.own_chunks[own] < state->table.nchunks);

		check(state, state->kernels->table_link(&state->table, state->parallel.own_chunks[own],
												&from, NULL,
												state->inner_unique ? NULL : &repeated,
												&state->status));
		duplicates += repeated;
	}
	if (duplicates > 0)
		check(state, state->kernels->build_add_duplicates(state->parallel.shared->counters,
														  duplicates,
														  &state->status));
	state->parallel.nown = 0;
}

/* Free the shared table, as the last participant to leave or at a rescan. */
void
free_shared_table(TessHashJoinState *state)
{
	if (DsaPointerIsValid(state->parallel.shared->index))
	{
		dsa_free(query_dsa(state), state->parallel.shared->index);
		state->parallel.shared->index = InvalidDsaPointer;
		state->parallel.shared->index_len = 0;
	}
	if (DsaPointerIsValid(state->parallel.shared->directory))
	{
		dsa_free(query_dsa(state), state->parallel.shared->directory);
		state->parallel.shared->directory = InvalidDsaPointer;
		state->parallel.shared->nchunks = 0;
	}
	for (int list = 0; list < 2 * state->parallel.shared->participants; list++)
	{
		dsa_pointer *head = participant_list(state, list / 2, list % 2 == 1);

		while (DsaPointerIsValid(*head))
		{
			dsa_pointer block = *head;

			*head = ((JoinChunk *) dsa_get_address(query_dsa(state), block))->next;
			dsa_free(query_dsa(state), block);
		}
	}
	if (DsaPointerIsValid(state->parallel.shared->marks))
	{
		dsa_free(query_dsa(state), state->parallel.shared->marks);
		state->parallel.shared->marks = InvalidDsaPointer;
	}
	if (DsaPointerIsValid(state->parallel.shared->spill_filter))
	{
		dsa_free(query_dsa(state), state->parallel.shared->spill_filter);
		state->parallel.shared->spill_filter = InvalidDsaPointer;
		state->parallel.shared->spill_filter_words = 0;
	}
	if (DsaPointerIsValid(state->parallel.shared->value_directory))
	{
		dsa_free(query_dsa(state), state->parallel.shared->value_directory);
		state->parallel.shared->value_directory = InvalidDsaPointer;
	}
	state->parallel.shared->next_value_chunk = 0;
	state->parallel.shared->nvalue_chunks = 0;
	reset_values(state);
	if (DsaPointerIsValid(state->parallel.shared->filter))
	{
		dsa_free(query_dsa(state), state->parallel.shared->filter);
		state->parallel.shared->filter = InvalidDsaPointer;
		state->parallel.shared->filter_words = 0;
	}
	take_back_bloom(state);
	state->bloom.bits = NULL;
	state->bloom.nwords = 0;
	state->table.index = NULL;
	state->table.index_len = 0;
	state->table.nchunks = 0;
}

/*
 * Step through the shared build's phases until probing starts: the node
 * performs each action the participant returns, and the build barrier's
 * waits stay here, since they may raise an error.
 */
void
build_shared(TessHashJoinState *state)
{
	uint32		reply = 0;

	state->compact_decided = false;
	reset_values(state);
	state->build_rows = 0;
	state->duplicates = 0;
	state->null_columns = 0;
	state->parallel.spill_participant = IsParallelWorker() ? ParallelWorkerNumber + 1 : 0;
	state->parallel.round_partition = -1;
	reset_prune_keys(state);
	state->parallel.spill_words = NULL;
	state->parallel.spill_seen = 0;
	state->parallel.spill_over = false;
	state->parallel.own_base = NULL;
	state->parallel.own_len = 0;
	state->parallel.nown = 0;
	state->parallel.own_bytes = 0;
	state->parallel.appended = 0;
	state->parallel.participating = true;
	/* This build's filter: decided again by this participant's batches. */
	take_back_bloom(state);
	state->bloom.bits = NULL;
	state->bloom.nwords = 0;
	state->bloom.decided = false;
	state->bloom.shared = false;
	state->bloom.ready = false;
	state->sample_rows = 0;
	state->sample_found = 0;
	for (;;)
	{
		uint32		action;

		check(state, state->kernels->build_step(&state->parallel.participant,
												state->parallel.shared->counters, reply,
												&action, &state->status));
		reply = 0;
		switch (action)
		{
			case TESS_BUILD_ATTACH:
				reply = BarrierAttach(&state->parallel.shared->build);
				break;
			case TESS_BUILD_ARRIVE_AND_WAIT:
				reply = BarrierArriveAndWait(&state->parallel.shared->build,
											 PG_WAIT_EXTENSION) ? 1 : 0;
				break;
			case TESS_BUILD_DO_BUILD:
				Assert(BarrierPhase(&state->parallel.shared->build) == TESS_BUILD_BUILD);
				build_shared_inner(state);
				break;
			case TESS_BUILD_DO_FLUSH:
				Assert(BarrierPhase(&state->parallel.shared->build) == TESS_BUILD_FLUSH);
				shared_flush(state);
				break;
			case TESS_BUILD_DO_OUTER:
				Assert(BarrierPhase(&state->parallel.shared->build) == TESS_BUILD_OUTER);
				shared_outer(state);
				break;
			case TESS_BUILD_DO_SIZE:
				Assert(BarrierPhase(&state->parallel.shared->build) == TESS_BUILD_SIZE);
				size_shared_table(state);
				break;
			case TESS_BUILD_DO_LINK:
				Assert(BarrierPhase(&state->parallel.shared->build) == TESS_BUILD_LINK);
				link_own_chunks(state);
				break;
			case TESS_BUILD_DO_PROBE:
				Assert(BarrierPhase(&state->parallel.shared->build) == TESS_BUILD_PROBE);
				{
					uint64		records;
					uint64		nchunks;

					attach_shared_table(state);
					share_marks(state, state->parallel.shared->marks, state->parallel.shared->nchunks);
					/* The whole table's rows and duplicates, every link done. */
					check(state, state->kernels->build_totals(state->parallel.shared->counters,
															  &records,
															  &state->null_columns,
															  &nchunks,
															  &state->duplicates,
															  &state->status));
					state->build_rows = records;
					state->parallel.chain_table = true;
					if (shared_partitions(state) > 0)
						shared_probe_start(state, records);
					state->built = true;
					return;
				}
			case TESS_BUILD_DETACH:
				BarrierDetach(&state->parallel.shared->build);
				break;
			case TESS_BUILD_DONE:
				/* Attached after the last one left: nothing is left to probe. */
				state->parallel.participating = false;
				state->build_rows = 0;
				state->built = true;
				state->done = true;
				return;
			default:
				elog(ERROR, "TessHashJoin got build action %u out of order", action);
		}
	}
}

/*
 * Leave a shared build after probing; the last one to leave frees the
 * table, or with `keep`, RIGHT and FULL, owes the free until its tail is
 * done and the next call. True for the last one.
 */
bool
leave_shared(TessHashJoinState *state, bool keep)
{
	uint32		reply = 0;

	if (state->table_free_owed)
	{
		state->table_free_owed = false;
		state->parallel.chain_table = false;
		free_shared_table(state);
		return false;
	}
	if (!state->parallel.participating)
		return false;
	for (;;)
	{
		uint32		action;

		check(state, state->kernels->build_step(&state->parallel.participant,
												state->parallel.shared->counters, reply,
												&action, &state->status));
		reply = 0;
		switch (action)
		{
			case TESS_BUILD_ARRIVE_AND_DETACH:
				reply = BarrierArriveAndDetach(&state->parallel.shared->build) ? 1 : 0;
				break;
			case TESS_BUILD_DO_FREE:
				state->parallel.participating = false;
				if (keep)
				{
					state->table_free_owed = true;
					return true;
				}
				free_shared_table(state);
				state->parallel.chain_table = false;
				return true;
			case TESS_BUILD_DONE:
				state->parallel.participating = false;
				state->parallel.chain_table = false;
				state->table.index = NULL;
				state->table.index_len = 0;
				state->table.nchunks = 0;
				return false;
			default:
				elog(ERROR, "TessHashJoin got build action %u out of order", action);
		}
	}
}

/*
 * A shared table that spills (docs/spill.md, "Shared tables"). The build
 * appends to chunks in the query's shared memory as ever, counting their
 * bytes in the words every participant decides by; the budget is every
 * participant's hash_mem, as the core's shared table has. The first
 * participant whose chunks pass it splits the table: every participant,
 * once it sees that, splits its own chunks so far into partitions whose
 * chunks are in shared memory too, and goes on appending partitioned.
 * While the chunks take more than the budget, the largest partition goes
 * to disk, and each participant writes its own chunks of it to its own
 * files. At FLUSH each writes its tails of the partitions on disk and
 * hands its chunks of the others to the table, which SIZE indexes and
 * LINK links. At OUTER every participant writes its share of the outer
 * side to files, before any row goes out, since a participant that
 * returns rows may not wait at a barrier: the rows of the partitions on
 * disk to theirs, those of the partitions in memory and, for a left or
 * anti join, those without a pair to a file the shared table answers. At
 * PROBE each reads such files, one at a time as it takes them, and
 * probes the shared table; then it leaves the build and takes partitions
 * on disk, each joined whole by the one that took it, from every
 * participant's files, as a serial table joins its partitions.
 */

/* Name a side's files in the table's set; the inner side's chunks go to shared memory. */
static void
side_share(TessHashJoinState *state, SpillSide *side, const char *prefix, bool memory)
{
	TessSpillConfig config = TESS_STRUCT_INITIALIZER(TessSpillConfig);

	tess_spill_free(side->file);
	config.parent_context = side->context;
	config.kernels = state->kernels;
	config.npartitions = side->npartitions;
	config.level = 0;
	config.fingerprint = side->fingerprint;
	config.max_len = (uint64) MaxAllocHugeSize;
	config.shared = &state->parallel.shared->fileset;
	config.participant = state->parallel.spill_participant;
	config.name = psprintf("%s%d", prefix, state->css.ss.ps.plan->plan_node_id);
	config.buffer_len = TESS_SPILL_BUFFER_LEN(get_hash_memory_limit());
	side->file = tess_spill_create(&config);
	side->shared_files = true;
	if (!memory)
		return;
	side->area = query_dsa(state);
	side->shared = state->parallel.shared;
	side->spill_words = shared_words(state);
	side->spill_nwords = state->parallel.shared->spill_nwords;
	side->kernels = state->kernels;
	side->owner = state->parallel.spill_participant;
}

/*
 * The first participant past the budget: the partitions, the power of two
 * that makes the inner side it expects (twice what the participants hold,
 * or the planner's estimate if more) about half of one participant's
 * hash_mem each, since a partition on disk is joined by one, and two per
 * participant at least; a filter of every inner row, published before
 * the split, so that whoever sees the split finds it.
 */
static void
shared_split(TessHashJoinState *state)
{
	JoinShared *shared = state->parallel.shared;
	dsa_area   *area = query_dsa(state);
	Size		limit = get_hash_memory_limit();
	int			participants = Max(shared->participants, 1);
	double		held = (double) state->parallel.own_bytes + state->values.bytes;
	double		expected = held * 2 * participants;
	uint64		rows = Max((uint64) state->inner_rows * participants,
						   state->build_rows * 2 * participants);
	uint32		npartitions = JOIN_SPILL_MIN_PARTITIONS;
	uint32		in_force;
	Size		nwords;
	dsa_pointer filter;

	if (state->build_rows > 0)
		expected = Max(expected, held / state->build_rows * state->inner_rows * participants);
	while (npartitions < JOIN_SPILL_MAX_PARTITIONS &&
		   (npartitions < 2 * (uint32) participants ||
			(double) npartitions * (limit / 2) < expected) &&
		   (Size) npartitions * 2 * (4 * JOIN_SPILL_MIN_CHUNK + 2 * BLCKSZ) <= limit / 2)
		npartitions *= 2;
	check(state, state->kernels->table_bloom_words(Max(rows, 1), &nwords, &state->status));
	filter = dsa_allocate_extended(area, mul_size(sizeof(uint64), nwords),
								   DSA_ALLOC_HUGE | DSA_ALLOC_ZERO);
	SpinLockAcquire(&shared->lock);
	if (!DsaPointerIsValid(shared->spill_filter))
	{
		shared->spill_filter = filter;
		shared->spill_filter_words = nwords;
		filter = InvalidDsaPointer;
	}
	SpinLockRelease(&shared->lock);
	if (DsaPointerIsValid(filter))
		dsa_free(area, filter);
	else
		state->counters[JOIN_BLOOM_FILTERS]++;
	check(state, state->kernels->table_spill_split(shared_words(state), shared->spill_nwords,
												   npartitions, &in_force,
												   &state->status));
}

/*
 * This participant sees the table split: a level of the partitions in
 * force, its inner side in shared memory and its files in the table's
 * set; its own chunks so far split into the partitions, their values
 * copied into the partitions' value chunks, then freed.
 */
static void
shared_switch(TessHashJoinState *state)
{
	JoinShared *shared = state->parallel.shared;
	dsa_area   *area = query_dsa(state);
	JoinSpill  *spill;
	dsa_pointer block;

	spill = spill_create(state, NULL, 0, 0, (int) shared_partitions(state));
	spill->shared = true;
	spill->writers = shared->participants;
	side_share(state, &spill->build, "tjb", true);
	side_share(state, &spill->probe, "tjo", false);
	side_share(state, &spill->resident, "tjr", false);
	/* The splitting participant published the filter before the split. */
	SpinLockAcquire(&shared->lock);
	block = shared->spill_filter;
	spill->bloom_words = shared->spill_filter_words;
	SpinLockRelease(&shared->lock);
	if (!DsaPointerIsValid(block))
		elog(ERROR, "TessHashJoin found its shared table split without a filter");
	spill->bloom = dsa_get_address(area, block);
	/* Only this participant takes from its lists before SIZE. */
	for (block = *own_list(state, false); DsaPointerIsValid(block);)
	{
		JoinChunk  *header = dsa_get_address(area, block);
		dsa_pointer next = header->next;

		split_chunk(state, (char *) header + JOIN_CHUNK_HEADER, header->len,
					state->values.bases);
		shared_count(state, -record_chunk_cost(header->len, state->record_size));
		dsa_free(area, block);
		block = next;
	}
	*own_list(state, false) = InvalidDsaPointer;
	for (block = *own_list(state, true); DsaPointerIsValid(block);)
	{
		JoinChunk  *header = dsa_get_address(area, block);
		dsa_pointer next = header->next;

		shared_count(state, -(int64) (JOIN_CHUNK_HEADER + header->len));
		dsa_free(area, block);
		block = next;
	}
	*own_list(state, true) = InvalidDsaPointer;
	/* The values of the chunks split are the partitions' now. */
	if (state->values.bases != NULL)
		memset(state->values.bases, 0, sizeof(char *) * state->values.slots);
	reset_values(state);
	state->parallel.nown = 0;
	state->parallel.own_base = NULL;
	state->parallel.own_len = 0;
	state->parallel.own_bytes = 0;
	spill->total_rows = state->build_rows;
	state->parallel.spill_seen = 0;
}

/*
 * Write this participant's chunks of the partitions others sent to disk;
 * then, when evict is set and the chunks pass the budget, send the
 * largest partition to disk and write this participant's chunks of it:
 * one per call, since the others write theirs of it at their next batch.
 */
static void
shared_sync(TessHashJoinState *state, bool evict)
{
	JoinSpill  *spill = state->spill;
	SpillSide  *side = &spill->build;
	uint64		evictions;
	int32		partition;

	check(state, state->kernels->table_spill_evictions(shared_words(state),
													   state->parallel.shared->spill_nwords,
													   &evictions, &state->status));
	if (evictions != state->parallel.spill_seen)
	{
		state->parallel.spill_seen = evictions;
		for (partition = 0; partition < spill->npartitions; partition++)
			if (side->parts[partition].resident && shared_on_disk(state, partition))
				side_demote(state, side, partition);
	}
	if (!evict)
		return;
	/* The count as of now, which the others' writes lowered. */
	side_count(side, -1, 0);
	if (!side->over)
		return;
	check(state, state->kernels->table_spill_evict(shared_words(state),
												   state->parallel.shared->spill_nwords,
												   &partition, &state->status));
	if (partition >= 0 && side->parts[partition].resident)
		side_demote(state, side, partition);
}

/* After a batch of the inner side: split, switch or send partitions to disk as needed. */
void
shared_check(TessHashJoinState *state)
{
	if (state->spill == NULL)
	{
		if (state->parallel.spill_over && shared_partitions(state) == 0)
			shared_split(state);
		if (shared_partitions(state) > 0)
			shared_switch(state);
	}
	if (state->spill != NULL)
		shared_sync(state, true);
}

/* A participant of a table that spilled, one that joined late too: its level, as decided. */
static void
shared_join(TessHashJoinState *state)
{
	if (state->spill == NULL)
		shared_switch(state);
	shared_sync(state, false);
}

/*
 * FLUSH: this participant counts its records by partition, writes its
 * chunks of the partitions on disk, the tails too, and hands those of the
 * partitions in memory, with their values, to the table's lists.
 */
void
shared_flush(TessHashJoinState *state)
{
	dsa_area   *area = query_dsa(state);
	JoinSpill  *spill;
	SpillSide  *side;

	if (shared_partitions(state) == 0)
		return;
	shared_join(state);
	spill = state->spill;
	side = &spill->build;
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		SpillPart  *part = &side->parts[partition];

		if (side->rows[partition] > 0)
			check(state, state->kernels->table_spill_records(shared_words(state),
															 state->parallel.shared->spill_nwords,
															 partition, side->rows[partition],
															 NULL, &state->status));
		if (!part->resident)
		{
			pg_atomic_uint64 *stats = part_stats(state, partition);

			side_flush(state, side, partition);
			side_forget(side, partition);
			pg_atomic_fetch_add_u64(&stats[0], part->disk_bytes);
			pg_atomic_fetch_add_u64(&stats[1], part->blocks);
			continue;
		}
		for (int chunk = 0; chunk < part->nchunks; chunk++)
		{
			int			index = part->chunks[chunk];
			JoinChunk  *header = dsa_get_address(area, side->pointers[index]);

			header->next = *own_list(state, false);
			*own_list(state, false) = side->pointers[index];
			side->pointers[index] = InvalidDsaPointer;
			side->bases[index] = NULL;
		}
		for (int value = 0; value < part->nvalues; value++)
		{
			int			number = part->values[value];
			JoinChunk  *header = dsa_get_address(area, side->value_pointers[number]);

			header->next = *own_list(state, true);
			*own_list(state, true) = side->value_pointers[number];
			side->value_pointers[number] = InvalidDsaPointer;
			side->value_bases[number] = NULL;
		}
		side->bytes -= part->bytes;
		part->bytes = 0;
		part->nchunks = 0;
		part->nvalues = 0;
		part->value_current = -1;
		side->current[partition] = 0;
	}
	side_compact(side);
	tess_spill_finish(side->file);
}

/*
 * OUTER: this participant's share of the outer side, written whole: the
 * rows of the partitions on disk to theirs; those of the partitions in
 * memory, and for a left or anti join those the filter of every inner row
 * rejects or with a NULL key, which have no pair, to the rows the shared
 * table answers. Nothing goes out yet.
 */
void
shared_outer(TessHashJoinState *state)
{
	bool		answer = state->jointype == JOIN_LEFT || state->jointype == JOIN_ANTI;
	JoinSpill  *spill;

	if (shared_partitions(state) == 0)
		return;
	shared_join(state);
	/* Past the build's barrier, the keys are every participant's. */
	prune_outer(state);
	spill = state->spill;
	for (;;)
	{
		TessBatch  *batch = tess_input_next(state->outer_input);
		int			nrows;
		int			nwords;
		TessRowMask valid;
		TessRowMask passed;
		TessRowMask resident;
		uint64		any_disk = 0;
		uint64		any_resident = 0;

		if (batch == NULL)
			break;
		nrows = batch->rows.nrows;
		nwords = tess_row_mask_word_count(nrows);
		state->counters[JOIN_PROBE_ROWS] += tess_row_mask_count(&batch->rows);
		reserve_rows(state, nrows);
		memset(state->probe.valid_bits, 0, sizeof(uint64) * nwords);
		memset(state->probe.next_bits, 0, sizeof(uint64) * nwords);
		valid = (TessRowMask) {nrows, state->probe.valid_bits};
		passed = (TessRowMask) {nrows, state->probe.next_bits};
		resident = (TessRowMask) {nrows, state->probe.pending_bits};
		batch_keys(state, batch, state->keys.outer_keys, state->keys.outer_kinds, &valid);
		if (tess_row_mask_count(&valid) > 0)
			check(state, state->kernels->bloom_probe(spill->bloom, spill->bloom_words,
													 state->probe.hashes, &valid, &passed,
													 &state->status));
		state->counters[JOIN_BLOOM_REMOVED] +=
			tess_row_mask_count(&valid) - tess_row_mask_count(&passed);
		/* The rows passed split into those on disk (next_bits) and in memory. */
		for (int word = 0; word < nwords; word++)
		{
			uint64		bits = state->probe.next_bits[word];
			uint64		kept = 0;

			while (bits != 0)
			{
				int			bit = pg_rightmost_one_pos64(bits);
				int			partition = spill_partition(spill, state->probe.hashes[word * 64 + bit]);

				bits &= bits - 1;
				if (spill->build.parts[partition].resident)
					kept |= UINT64CONST(1) << bit;
			}
			state->probe.pending_bits[word] = kept |
				(answer ? batch->rows.bits[word] & ~state->probe.next_bits[word] : 0);
			state->probe.next_bits[word] &= ~kept;
			any_disk |= state->probe.next_bits[word];
			any_resident |= state->probe.pending_bits[word];
		}
		if (any_disk != 0)
			side_append(state, &spill->probe, batch, &passed, spill->stored, NULL);
		if (any_resident != 0)
			side_append(state, &spill->resident, batch, &resident, spill->stored, NULL);
		tess_input_finish(state->outer_input);
	}
	/* Every tail to disk: the others read them. */
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		side_flush(state, &spill->probe, partition);
		side_forget(&spill->probe, partition);
	}
	side_flush(state, &spill->resident, 0);
	side_forget(&spill->resident, 0);
	tess_spill_finish(spill->probe.file);
	tess_spill_finish(spill->resident.file);
}

/*
 * PROBE of a table that spilled: every participant's files are done; the
 * sizes of the partitions on disk are the table's, not this participant's.
 */
void
shared_probe_start(TessHashJoinState *state, uint64 records)
{
	JoinSpill  *spill;
	Size		record;

	shared_join(state);
	spill = state->spill;
	record = TYPEALIGN(8, 16 + 8 * spill->build.nkeys + spill->build.payload_size);
	tess_spill_finish(spill->build.file);
	tess_spill_finish(spill->probe.file);
	tess_spill_finish(spill->resident.file);
	spill->joining = true;
	spill->partition = -1;
	spill->total_rows = records;
	spill->input_rows = records;
	check(state, state->kernels->table_spill_start(shared_words(state),
												   state->parallel.shared->spill_nwords,
												   (uint32 *) &spill->start, &state->status));
	spill->visited = 0;
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		uint64		rows;

		if (spill->build.parts[partition].resident)
		{
			state->counters[JOIN_RESIDENT]++;
			continue;
		}
		check(state, state->kernels->table_spill_records(shared_words(state),
														 state->parallel.shared->spill_nwords,
														 partition, 0, &rows,
														 &state->status));
		spill->build.rows[partition] = rows;
		spill->build.parts[partition].disk_bytes = rows * record;
	}
	state->build_rows = state->parallel.shared->resident_rows;
}

/*
 * The next batch of the outer rows the shared table answers: from a file
 * this participant took, the next one once it is done. NULL when every
 * file is taken.
 */
TessBatch *
shared_resident_next(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	for (;;)
	{
		uint32		writer;

		if (spill->rows == &spill->resident && next_spilled(state, spill))
		{
			reserve_rows(state, spill->batch.rows.nrows);
			state->active_bits[0] = spill->bits[0];
			return &spill->batch;
		}
		check(state, state->kernels->table_spill_take_file(shared_words(state),
														   state->parallel.shared->spill_nwords,
														   spill->npartitions, true,
														   &writer, &state->status));
		if (writer >= (uint32) spill->writers)
			return NULL;
		spill->rows = &spill->resident;
		part_open(&spill->reader, spill->resident.file, 0, (int) writer + 1);
		spill->reader.next = (int) writer;
		spill->partition = 0;
		spill->tail_read = true;
		spill->block = NULL;
		spill->ordinal = 0;
	}
}

/*
 * The shared table answered its rows: this participant leaves it, the
 * last one freeing it, and joins the partitions on disk it takes.
 */
void
shared_resident_end(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	for (int index = 0; index < spill->nblock_values; index++)
		spill->resident.value_bases[spill->block_values[index]] = NULL;
	spill->nblock_values = 0;
	MemoryContextReset(spill->block_context);
	spill->block = NULL;
	spill->rows = &spill->probe;
	spill->partition = -1;
	spill->resident_done = true;
	/* The filter goes with the table. */
	spill->bloom = NULL;
	spill->bloom_words = 0;
	leave_shared(state, false);
	forget_marks(state);
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->bloom.bits = NULL;
	state->bloom.nwords = 0;
	state->build_rows = 0;
	state->duplicates = 0;
}

/* Whether any participant wrote outer rows of a partition. */
bool
shared_has_outer(JoinSpill *spill, int partition)
{
	for (int writer = 0; writer < spill->writers; writer++)
	{
		TessSpillReader *reader = tess_spill_open(spill->probe.file, writer, partition);
		TessSpillHeader header;
		bool		found;

		if (reader == NULL)
			continue;
		found = tess_spill_read_header(reader, &header);
		tess_spill_close(reader);
		if (found)
			return true;
	}
	return false;
}

/* The two counters of a partition of a shared table: bytes on disk and blocks of records. */
pg_atomic_uint64 *
part_stats(TessHashJoinState *state, int partition)
{
	return (pg_atomic_uint64 *) dsa_get_address(query_dsa(state), state->parallel.shared->part_stats) +
		2 * partition;
}

static JoinRound *
round_of(TessHashJoinState *state, int partition)
{
	Assert(partition >= 0 && partition < state->parallel.shared->nrounds);
	return (JoinRound *) dsa_get_address(query_dsa(state), state->parallel.shared->rounds) + partition;
}

/* Free what a round holds in shared memory. */
static void
round_release(dsa_area *area, JoinRound *round)
{
	if (DsaPointerIsValid(round->directory))
	{
		dsa_pointer *chunks = dsa_get_address(area, round->directory);

		for (int chunk = 0; chunk < round->nchunks; chunk++)
			if (DsaPointerIsValid(chunks[chunk]))
				dsa_free(area, chunks[chunk]);
		dsa_free(area, round->directory);
		round->directory = InvalidDsaPointer;
	}
	if (DsaPointerIsValid(round->values))
	{
		dsa_pointer *values = dsa_get_address(area, round->values);

		for (int number = 0; number < round->nvalues; number++)
			if (DsaPointerIsValid(values[number]))
				dsa_free(area, values[number]);
		dsa_free(area, round->values);
		round->values = InvalidDsaPointer;
	}
	if (DsaPointerIsValid(round->index))
	{
		dsa_free(area, round->index);
		round->index = InvalidDsaPointer;
	}
	if (DsaPointerIsValid(round->marks))
	{
		dsa_free(area, round->marks);
		round->marks = InvalidDsaPointer;
	}
}

/*
 * SIZE of a table that spilled: a round per partition. One on disk whose
 * blocks and index fit in one participant's hash_mem is loaded by every
 * participant together; a larger one is joined by one alone, which may
 * split it or take it in pieces.
 */
void
make_rounds(TessHashJoinState *state)
{
	dsa_area   *area = query_dsa(state);
	Size		limit = get_hash_memory_limit();
	Size		payload_size = sizeof(uint64) * (1 + state->npayload);
	int			npartitions = (int) shared_partitions(state);
	JoinRound  *rounds;

	Assert(!DsaPointerIsValid(state->parallel.shared->rounds));
	state->parallel.shared->rounds = dsa_allocate_extended(area, mul_size(npartitions, sizeof(JoinRound)),
												  DSA_ALLOC_ZERO);
	state->parallel.shared->nrounds = npartitions;
	rounds = dsa_get_address(area, state->parallel.shared->rounds);
	for (int partition = 0; partition < npartitions; partition++)
	{
		JoinRound  *round = &rounds[partition];
		pg_atomic_uint64 *stats = part_stats(state, partition);
		uint64		bytes = pg_atomic_read_u64(&stats[0]);
		uint64		blocks = pg_atomic_read_u64(&stats[1]);
		Size		size;

		BarrierInit(&round->barrier, 0);
		pg_atomic_init_u32(&round->next_chunk, 0);
		round->index = InvalidDsaPointer;
		round->directory = InvalidDsaPointer;
		round->values = InvalidDsaPointer;
		if (!shared_on_disk(state, partition))
			continue;
		check(state, state->kernels->table_spill_records(shared_words(state),
														 state->parallel.shared->spill_nwords,
														 partition, 0, &round->records,
														 &state->status));
		check(state, state->kernels->table_size(state->keys.nkeys, state->keys.inner_kinds, payload_size,
												Max(round->records, JOIN_INITIAL_ROWS),
												&size, &state->status));
		round->nchunks = (int) Min(blocks, (uint64) INT_MAX);
		round->nvalues = (int) state->parallel.shared->next_value_chunk;
		round->together = blocks > 0 && blocks <= TESS_TABLE_MAX_CHUNKS &&
			(double) bytes + size <= (double) limit;
	}
}

/* Free every round's memory and the rounds, at a rescan. */
void
free_rounds(TessHashJoinState *state)
{
	dsa_area   *area = query_dsa(state);

	if (!DsaPointerIsValid(state->parallel.shared->rounds))
		return;
	for (int partition = 0; partition < state->parallel.shared->nrounds; partition++)
		round_release(area, round_of(state, partition));
	dsa_free(area, state->parallel.shared->rounds);
	state->parallel.shared->rounds = InvalidDsaPointer;
	state->parallel.shared->nrounds = 0;
}

/* ALLOCATE, as the round's elected one: the index for its records, and the directories. */
static void
round_allocate(TessHashJoinState *state, JoinRound *round)
{
	dsa_area   *area = query_dsa(state);
	Size		payload_size = sizeof(uint64) * (1 + state->npayload);
	uint64		capacity = Max(round->records, JOIN_INITIAL_ROWS);
	Size		size;

	check(state, state->kernels->table_size(state->keys.nkeys, state->keys.inner_kinds, payload_size,
											capacity, &size, &state->status));
	round->index = dsa_allocate_extended(area, size, DSA_ALLOC_HUGE);
	round->index_len = size;
	check(state, state->kernels->table_create(dsa_get_address(area, round->index), size,
											  state->keys.nkeys, state->keys.inner_kinds, payload_size,
											  capacity, &state->status));
	round->directory = dsa_allocate_extended(area,
											 mul_size(Max(round->nchunks, 1),
													  sizeof(dsa_pointer) + sizeof(Size)),
											 DSA_ALLOC_ZERO | DSA_ALLOC_HUGE);
	round->values = dsa_allocate_extended(area,
										  mul_size(Max(round->nvalues, 1), sizeof(dsa_pointer)),
										  DSA_ALLOC_ZERO | DSA_ALLOC_HUGE);
	round->marks = allocate_marks(state, round->nchunks);
	state->counters[JOIN_ROUNDS]++;
}

/*
 * LOAD: the inner files this participant takes, block by block into
 * shared memory; a chunk of records takes the next number of the round's
 * directory and is linked at once. A link reads only its own chunk and
 * the buckets, so the chunks others load stand as empty ones here.
 */
static void
round_load(TessHashJoinState *state, JoinSpill *spill, int partition, JoinRound *round)
{
	dsa_area   *area = query_dsa(state);
	dsa_pointer *chunks = dsa_get_address(area, round->directory);
	Size	   *lens = (Size *) (chunks + Max(round->nchunks, 1));
	dsa_pointer *values = dsa_get_address(area, round->values);
	int			nchunks = Max(round->nchunks, 1);
	void	   *empty = MemoryContextAlloc(spill->part_context, TESS_TABLE_CHUNK_HEADER);
	TessTableRef ref;

	check(state, state->kernels->table_chunk_init(empty, TESS_TABLE_CHUNK_HEADER,
												  &state->status));
	spill->round_bases = MemoryContextAlloc(spill->part_context, sizeof(void *) * nchunks);
	spill->round_lens = MemoryContextAlloc(spill->part_context, sizeof(Size) * nchunks);
	for (int chunk = 0; chunk < nchunks; chunk++)
	{
		spill->round_bases[chunk] = empty;
		spill->round_lens[chunk] = TESS_TABLE_CHUNK_HEADER;
	}
	ref.index = dsa_get_address(area, round->index);
	ref.index_len = round->index_len;
	ref.chunks = spill->round_bases;
	ref.chunk_lens = spill->round_lens;
	ref.nchunks = round->nchunks;
	for (;;)
	{
		TessSpillReader *reader;
		TessSpillHeader header;
		uint32		writer;

		CHECK_FOR_INTERRUPTS();
		check(state, state->kernels->table_spill_take_file(shared_words(state),
														   state->parallel.shared->spill_nwords,
														   partition, false, &writer,
														   &state->status));
		if (writer >= (uint32) spill->writers)
			break;
		reader = tess_spill_open(spill->build.file, (int) writer, partition);
		if (reader == NULL)
			continue;
		while (tess_spill_read_header(reader, &header))
		{
			dsa_pointer block = dsa_allocate_extended(area, Max(header.len, 8), DSA_ALLOC_HUGE);
			char	   *body = dsa_get_address(area, block);
			uint32		slot;
			Size		from = TESS_TABLE_CHUNK_HEADER;

			tess_spill_read_body(reader, body, header.len);
			if (header.kind == TESS_SPILL_VALUES)
			{
				if (header.number >= (uint32) round->nvalues ||
					DsaPointerIsValid(values[header.number]))
					ereport(ERROR,
							errcode(ERRCODE_DATA_CORRUPTED),
							errmsg("TessHashJoin read value chunk %u of a round twice or past its values",
								header.number));
				values[header.number] = block;
				continue;
			}
			slot = pg_atomic_fetch_add_u32(&round->next_chunk, 1);
			if (slot >= (uint32) round->nchunks)
				ereport(ERROR,
						errcode(ERRCODE_DATA_CORRUPTED),
						errmsg("TessHashJoin read more chunks of a round than were written"));
			chunks[slot] = block;
			lens[slot] = header.len;
			spill->round_bases[slot] = body;
			spill->round_lens[slot] = header.len;
			check(state, state->kernels->table_link(&ref, (int) slot, &from, NULL, NULL,
													&state->status));
		}
		tess_spill_close(reader);
	}
}

/* PROBE: the round's table as every participant loaded it. */
static void
round_attach(TessHashJoinState *state, JoinSpill *spill, JoinRound *round)
{
	dsa_area   *area = query_dsa(state);
	dsa_pointer *chunks = dsa_get_address(area, round->directory);
	Size	   *lens = (Size *) (chunks + Max(round->nchunks, 1));
	dsa_pointer *values = dsa_get_address(area, round->values);

	if (pg_atomic_read_u32(&round->next_chunk) != (uint32) round->nchunks)
		elog(ERROR, "TessHashJoin loaded %u chunks of a round of %d",
			 pg_atomic_read_u32(&round->next_chunk), round->nchunks);
	reserve_chunks(state, Max(round->nchunks, 1));
	for (int chunk = 0; chunk < round->nchunks; chunk++)
	{
		state->chunk_bases[chunk] = dsa_get_address(area, chunks[chunk]);
		state->chunk_lens[chunk] = lens[chunk];
	}
	state->table.index = dsa_get_address(area, round->index);
	state->table.index_len = round->index_len;
	state->table.nchunks = round->nchunks;
	spill->round_values = MemoryContextAllocZero(spill->part_context,
												 sizeof(char *) * Max(round->nvalues, 1));
	for (int number = 0; number < round->nvalues; number++)
		if (DsaPointerIsValid(values[number]))
			spill->round_values[number] = dsa_get_address(area, values[number]);
	state->values.bases = spill->round_values;
	state->values.nchunks = round->nvalues;
	state->build_rows = round->records;
	/* The links counted none: the chains are walked for more. */
	state->duplicates = state->inner_unique ? 0 : 1;
	state->bloom.bits = NULL;
	state->bloom.nwords = 0;
	state->bloom.decided = true;
	state->parallel.chain_table = true;
	share_marks(state, round->marks, round->nchunks);
	join_note_memory(state);
}

/*
 * Take part in the round over a partition: true once this participant
 * probes it, false when the round is past loading, its outer rows all
 * taken.
 */
static bool
round_join(TessHashJoinState *state, int partition)
{
	JoinRound  *round = round_of(state, partition);
	uint32		reply = 0;

	memset(&state->parallel.round_participant, 0, sizeof(state->parallel.round_participant));
	for (;;)
	{
		uint32		action;

		check(state, state->kernels->round_step(&state->parallel.round_participant, reply, &action,
												&state->status));
		reply = 0;
		switch (action)
		{
			case TESS_BUILD_ATTACH:
				reply = BarrierAttach(&round->barrier);
				break;
			case TESS_BUILD_ARRIVE_AND_WAIT:
				reply = BarrierArriveAndWait(&round->barrier, PG_WAIT_EXTENSION) ? 1 : 0;
				break;
			case TESS_BUILD_DO_ALLOCATE:
				Assert(BarrierPhase(&round->barrier) == TESS_ROUND_ALLOCATE);
				round_allocate(state, round);
				break;
			case TESS_BUILD_DO_LOAD:
				Assert(BarrierPhase(&round->barrier) == TESS_ROUND_LOAD);
				round_load(state, state->spill, partition, round);
				break;
			case TESS_BUILD_DO_PROBE:
				Assert(BarrierPhase(&round->barrier) == TESS_ROUND_PROBE);
				round_attach(state, state->spill, round);
				state->parallel.round_partition = partition;
				return true;
			case TESS_BUILD_DETACH:
				BarrierDetach(&round->barrier);
				break;
			case TESS_BUILD_DONE:
				return false;
			default:
				elog(ERROR, "TessHashJoin got round action %u out of order", action);
		}
	}
}

/*
 * Leave the round probed, without waiting: true for the last one, which
 * frees it; RIGHT and FULL leave before the tail, which the last one
 * returns first, owing the free.
 */
bool
round_depart(TessHashJoinState *state)
{
	JoinRound  *round = round_of(state, state->parallel.round_partition);
	uint32		reply = 0;

	Assert(!state->round_departed);
	state->round_departed = true;
	for (;;)
	{
		uint32		action;

		check(state, state->kernels->round_step(&state->parallel.round_participant, reply, &action,
												&state->status));
		reply = 0;
		switch (action)
		{
			case TESS_BUILD_ARRIVE_AND_DETACH:
				reply = BarrierArriveAndDetach(&round->barrier) ? 1 : 0;
				break;
			case TESS_BUILD_DO_FREE:
				state->round_free_owed = true;
				return true;
			case TESS_BUILD_DONE:
				return false;
			default:
				elog(ERROR, "TessHashJoin got round action %u out of order", action);
		}
	}
}

/* Leave the round probed, unless left already; the last one frees it. */
void
round_leave(TessHashJoinState *state)
{
	JoinRound  *round = round_of(state, state->parallel.round_partition);

	if (!state->round_departed)
		(void) round_depart(state);
	if (state->round_free_owed)
		round_release(query_dsa(state), round);
	state->round_departed = false;
	state->round_free_owed = false;
	state->parallel.round_partition = -1;
	state->parallel.chain_table = false;
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->values.bases = NULL;
	state->values.nchunks = 0;
}

/* The next outer file of the round's partition this participant takes; false when none is left. */
bool
round_next_outer(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;
	uint32		writer;

	check(state, state->kernels->table_spill_take_file(shared_words(state),
													   state->parallel.shared->spill_nwords,
													   spill->partition, true, &writer,
													   &state->status));
	if (writer >= (uint32) spill->writers)
		return false;
	part_open(&spill->reader, spill->probe.file, spill->partition, (int) writer + 1);
	spill->reader.next = (int) writer;
	spill->tail_read = true;
	spill->block = NULL;
	spill->ordinal = 0;
	return true;
}


/*
 * The next partition on disk of a shared table, from this participant's
 * start round the circle: a round it takes part in, or one it joins alone.
 * False when it visited every one.
 */
bool
shared_next_partition(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	while (spill->visited < spill->npartitions)
	{
		int			partition = (spill->start + spill->visited++) % spill->npartitions;
		bool		taken;

		if (spill->build.parts[partition].resident)
			continue;
		/* RIGHT and FULL return a partition's inner rows without outer ones too. */
		if ((!state->preserve_inner || spill->build.rows[partition] == 0) &&
			!shared_has_outer(spill, partition))
			continue;
		if (round_of(state, partition)->together)
		{
			if (!round_join(state, partition))
				continue;
			/* The outer files come one at a time, as next_pass takes them. */
			spill->partition = partition;
			spill->reader.open = false;
			spill->tail_read = true;
			spill->block = NULL;
			return true;
		}
		check(state, state->kernels->table_spill_take_alone(shared_words(state),
															state->parallel.shared->spill_nwords,
															partition, &taken,
															&state->status));
		if (!taken)
			continue;
		state->counters[JOIN_ALONE]++;
		spill->probe.rows[partition] = 1;
		return open_partition(state, partition);
	}
	spill->partition = spill->npartitions;
	return false;
}


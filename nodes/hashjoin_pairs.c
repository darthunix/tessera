/*
 * TessHashJoin's pairs: the rounds of an outer batch over the records of
 * its keys, a compact batch of the pairs, the NULL round of a LEFT join,
 * the marks of RIGHT and FULL and the tail of the records without a pair.
 */
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
			join_outer_finish(state, state->outer_batch);
			state->outer_batch = NULL;
		}
		batch = join_outer_next(state);
		if (batch == NULL)
			return false;
		found = join_probe_batch(state, batch);
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
			join_outer_finish(state, batch);
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
				join_child_column(state->outer_batch, state->child_columns[column],
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
		/* The rounds below read the records' inner columns. */
		state->null_round = false;
		if (state->outer_batch != NULL)
		{
			join_outer_finish(state, state->outer_batch);
			state->outer_batch = NULL;
		}
		batch = join_outer_next(state);
		if (batch == NULL)
			return false;
		nrows = batch->rows.nrows;
		nwords = tess_row_mask_word_count(nrows);
		join_reserve_rows(state, nrows);
		memset(state->matched_bits, 0, sizeof(uint64) * nwords);
		state->outer_batch = batch;
		/* A table that spills has inner rows on disk even with none resident. */
		if ((state->build_rows > 0 ||
			 (state->spill != NULL && !state->spill->joining)) &&
			join_probe_batch(state, batch))
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
		/*
		 * An anti join's rows have no pair, so an inner column is NULL in
		 * each: the planner makes an anti join of an outer join whose
		 * inner key is tested for NULL above it, and the outer join's
		 * target keeps the inner columns.
		 */
		state->null_round = state->jointype == JOIN_ANTI;
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
	join_reserve_rows(state, JOIN_COMPACT_ROWS);
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
bool
join_next_output(TessHashJoinState *state)
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
			if (!state->tail.request && !join_tail_turn(state))
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

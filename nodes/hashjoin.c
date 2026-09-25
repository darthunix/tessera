#include "postgres.h"

#include "access/parallel.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/optimizer.h"
#include "utils/datum.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"
#include "storage/shm_toc.h"
#include "utils/ruleutils.h"

#include "tessera/kernel_ops.h"
#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

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

/* The payload holds a word of NULL bits, so at most 64 inner columns. */
#define JOIN_MAX_PAYLOAD 64
/* Rows of the buffers before the first batch shows its size. */
#define JOIN_INITIAL_ROWS 64
/* Rows of a compact batch: the pairs of several rounds, one after another. */
#define JOIN_COMPACT_ROWS 64
/* A round with at least this many rows is published as it is, not copied. */
#define JOIN_DENSE_ROUND 32

/* The counters every participant of a parallel plan shares. */
enum
{
	JOIN_BUILDS,
	JOIN_BUILD_ROWS,
	/* Summed over the builds; EXPLAIN shows the mean. */
	JOIN_BUCKETS,
	JOIN_GROWS,
	JOIN_PROBE_ROWS,
	JOIN_MATCHES,
	/* The participant's peak bytes, and what of them exceeded hash_mem. */
	JOIN_MEMORY,
	JOIN_OVERRUN,
	/* Batches of pairs copied one after another: compact mode. */
	JOIN_COMPACT_BATCHES,
	/* Pairs the residual join clauses removed. */
	JOIN_FILTER_REMOVED,
	JOIN_OUTPUT_REMOVED,
	JOIN_NCOUNTERS
};

/* Which child a column of the scan tuple comes from. */
typedef enum JoinSide
{
	JOIN_SIDE_OUTER = 0,
	JOIN_SIDE_INNER = 1
} JoinSide;

typedef struct TessHashJoinState
{
	CustomScanState css;
	const TessKernelOps *kernels;
	PlanState  *outer;
	PlanState  *inner;
	TessInput  *outer_input;
	TessInput  *inner_input;
	TessOutput *output;
	/* The node's layout: which scan tuple column each target is. */
	TessLayout	layout;
	/* The parent's request, frozen at the first execution; NULL before. */
	const TessRequest *request;

	/* Per scan tuple column: its side and its column in that child's batches. */
	int			ncolumns;
	int		   *sides;
	int		   *child_columns;
	/* Per scan tuple column: 1 + its payload word, or 0 when not kept. */
	int		   *payload_words;
	/* The key's column in each child's batches, and its kind there. */
	/* The keys: each one's column in each child's batches, and its kind there. */
	int			nkeys;
	int			outer_keys[TESS_TABLE_MAX_KEYS];
	int			inner_keys[TESS_TABLE_MAX_KEYS];
	TessTableKeyKind outer_kinds[TESS_TABLE_MAX_KEYS];
	TessTableKeyKind inner_kinds[TESS_TABLE_MAX_KEYS];
	/* The key columns of the batch being inserted or probed. */
	TessDatumColumn key_columns[TESS_TABLE_MAX_KEYS];
	TessTableKey table_keys[TESS_TABLE_MAX_KEYS];
	/* Every outer row matches at most one inner row: no second round. */
	bool		inner_unique;
	/*
	 * INNER, SEMI, ANTI or LEFT: the kinds that keep the outer side, which
	 * the node probes with.
	 */
	JoinType	jointype;
	/* Per filter of an outer join in evaluation order: whether it runs in batches. */
	List	   *filter_batch;
	/* The residual clauses after the keys that run in batches, first. */
	/* Per residual clause in evaluation order: whether it runs in batches. */
	List	   *residual_batch;
	/* The planner's estimate of the inner rows, the table's first capacity. */
	int			inner_rows;
	/* The types of the scan tuple columns, for copying inner values. */
	int16	   *typlens;
	bool	   *typbyvals;

	/*
	 * The inner columns the parent asked for, in payload order: each
	 * record's payload is a word of their NULL bits, then a Datum each.
	 */
	int			npayload;
	int		   *payload_columns;

	/* The table's region, and the copies of by-reference inner values. */
	MemoryContext table_context;
	MemoryContext values_context;
	char	   *region;
	Size		region_len;
	bool		built;

	/* Buffers for one batch of either side, for capacity rows. */
	int			capacity;
	uint32	   *hashes;
	/* Insertion: each row's new record; probing: each row's match. */
	uint32	   *offsets;
	uint64	   *valid_bits;
	uint64	   *pending_bits;
	uint64	   *duplicate_bits;
	/* The payload of every row of an inner batch, one after another. */
	uint64	   *payload;
	/* The rows of the current round, and a copy the next step reads. */
	uint64	   *round_bits;
	uint64	   *next_bits;
	/* The published batch's mask, which the parent may narrow. */
	uint64	   *published_bits;
	/* Per row of the round: its record's NULL bits, then each kept column. */
	Datum	   *null_words;
	Datum	  **inner_values;
	bool	  **inner_isnull;
	/* What this round already gathered from the records. */
	bool		nulls_gathered;
	bool	   *gathered;

	/* The copies of the compact batch's by-reference outer values. */
	MemoryContext compact_context;
	/* The published batch is a compact one, not a round over the outer batch. */
	bool		output_compact;
	/* The records and rows the published batch reads: a round's or a compact batch's. */
	uint32	   *current_offsets;
	uint64	   *current_bits;

	/*
	 * Compact mode, for a table with duplicate keys: the pairs of the
	 * rounds are copied one after another into batches of
	 * JOIN_COMPACT_ROWS rows, the outer columns by value, the inner ones
	 * gathered from the pairs' records, instead of publishing every round
	 * over the outer batch's rows with a quarter of them selected.
	 */
	bool		compact;
	/* The outer scan columns the parent asked for, all passed by value. */
	int			nouter;
	int		   *outer_columns;
	/* Per scan column: its values in the compact batch; outer ones only. */
	Datum	  **compact_values;
	bool	  **compact_isnull;
	uint32		compact_offsets[JOIN_COMPACT_ROWS];
	uint64		compact_bits[1];
	/* The round being copied: its rows not yet copied, its outer columns. */
	bool		round_open;
	uint64	   *taken_bits;
	TessDatumColumn *round_columns;

	/* The outer batch whose rounds are being published, or NULL. */
	TessBatch  *outer_batch;
	/* The batch this node publishes: the outer rows of one round. */
	TessBatch	batch;
	/* Row mode: the column of every slot attribute, and the next row. */
	TessDatumColumn *columns;
	int			next_row;
	bool		serving;
	/* The residual join clauses over the pairs, or NULL; the scan tuple's layout. */
	TessQual   *qual;
	/* An outer join's filters over the rows it returns, or NULL. */
	TessQual   *filter;
	/*
	 * SEMI, ANTI and LEFT: the rows of the outer batch with a pair that
	 * passed the join clauses so far.
	 */
	uint64	   *matched_bits;
	/*
	 * LEFT: the published round extends the outer rows without a match
	 * with NULL inner columns, which these all-NULL columns give; a
	 * compact batch in progress holds it back for the next call.
	 */
	bool		null_round;
	bool		null_held;
	Datum	   *null_values;
	bool	   *null_isnull;
	/* The targets computed over the pairs, or NULL; the batch published then. */
	TessProjection *projection;
	List	   *computed;
	TessBatch  *published;
	TessLayout	scan_layout;
	/* Nothing is left to return. */
	bool		done;
	bool		compact_decided;

	/* Written by a kernel on failure only. */
	TessStatus	status;

	/* The rows of the current table, and those whose key it held already. */
	uint64		build_rows;
	uint64		duplicates;
	/* Bit w: payload column w holds a NULL somewhere in the table. */
	uint64		null_columns;
	/* This participant's counters; the memory ones are set when read. */
	uint64		counters[JOIN_NCOUNTERS];
	/* The most memory the table and the copies took, in bytes. */
	Size		peak_memory;
	/* The counters of every participant, in a parallel plan. */
	TessSharedStats *stats;
} TessHashJoinState;

static const CustomExecMethods join_exec_methods;

/* Raise the error a kernel stored, if the call failed. */
static inline void
check(TessHashJoinState *state, TessStatusCode code)
{
	if (code != TESS_OK)
		tess_status_report(&state->status);
}

static void child_column(TessBatch *batch, int column, const TessRowMask *rows,
						 TessColumnPurpose purpose, TessDatumColumn *result);

/*
 * The keys of a batch of one side: each key column, read for the selected
 * rows and hashed in key order, the first key's hash folding in the
 * others'; valid gets the rows whose keys are all non-NULL, since an
 * equality with NULL is never true. The table's keys point at the columns.
 */
static void
batch_keys(TessHashJoinState *state, TessBatch *batch, const int *columns,
		   const TessTableKeyKind *kinds, TessRowMask *valid)
{
	for (int key = 0; key < state->nkeys; key++)
	{
		TessDatumColumn *keys = &state->key_columns[key];
		bool		int8 = kinds[key] == TESS_TABLE_KEY_INT8;

		child_column(batch, columns[key], key == 0 ? &batch->rows : valid,
					 TESS_COLUMN_FOR_FILTER, keys);
		if (key == 0)
			check(state, (int8 ? state->kernels->int8_hash :
						  state->kernels->int4_hash) (keys, NULL, &batch->rows,
													  TESS_NULL_KEYS_REJECT,
													  state->hashes, valid,
													  &state->status));
		else
			check(state, (int8 ? state->kernels->int8_hash_next :
						  state->kernels->int4_hash_next) (keys, NULL,
														   TESS_NULL_KEYS_REJECT,
														   state->hashes, valid,
														   &state->status));
		state->table_keys[key].kind = kinds[key];
		state->table_keys[key].column = keys;
		state->table_keys[key].prepared = NULL;
	}
}

/* A column of a child's batch, checked. */
static void
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
static void
reserve_rows(TessHashJoinState *state, int nrows)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	int			nwords;

	if (nrows <= state->capacity)
		return;
	nrows = Max(nrows, JOIN_INITIAL_ROWS);
	nwords = tess_row_mask_word_count(nrows);
	if (state->hashes != NULL)
	{
		pfree(state->hashes);
		pfree(state->offsets);
		pfree(state->valid_bits);
		pfree(state->pending_bits);
		pfree(state->duplicate_bits);
		pfree(state->payload);
		pfree(state->round_bits);
		pfree(state->next_bits);
		pfree(state->published_bits);
		pfree(state->taken_bits);
		pfree(state->null_words);
		pfree(state->matched_bits);
		pfree(state->null_values);
		pfree(state->null_isnull);
		for (int word = 0; word < state->npayload; word++)
		{
			pfree(state->inner_values[word]);
			pfree(state->inner_isnull[word]);
		}
	}
	state->hashes = MemoryContextAllocZero(context, sizeof(uint32) * nrows);
	state->offsets = MemoryContextAllocZero(context, sizeof(uint32) * nrows);
	state->valid_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->pending_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->duplicate_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->payload = MemoryContextAllocZero(context,
											mul_size(sizeof(uint64) * nrows,
													 1 + state->npayload));
	state->round_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->next_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->published_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->taken_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->null_words = MemoryContextAllocZero(context, sizeof(Datum) * nrows);
	state->matched_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->null_values = MemoryContextAllocZero(context, sizeof(Datum) * nrows);
	state->null_isnull = MemoryContextAlloc(context, sizeof(bool) * nrows);
	memset(state->null_isnull, true, sizeof(bool) * nrows);
	/* Zeroed: rows outside a round are initialized memory, as batches promise. */
	for (int word = 0; word < state->npayload; word++)
	{
		state->inner_values[word] = MemoryContextAllocZero(context,
														   sizeof(Datum) * nrows);
		state->inner_isnull[word] = MemoryContextAllocZero(context,
														   sizeof(bool) * nrows);
	}
	state->capacity = nrows;
}

/* The bytes the table and the copies of inner values take now. */
static void
note_memory(TessHashJoinState *state)
{
	Size		memory = state->region_len +
		MemoryContextMemAllocated(state->values_context, true);

	state->peak_memory = Max(state->peak_memory, memory);
}

/* An empty table in a new region, sized for the planner's estimate. */
static void
create_table(TessHashJoinState *state)
{
	Size		payload_size = sizeof(uint64) * (1 + state->npayload);
	uint64		capacity = Max(state->inner_rows, JOIN_INITIAL_ROWS);
	Size		size;

	MemoryContextReset(state->table_context);
	MemoryContextReset(state->values_context);
	check(state, state->kernels->table_size(state->nkeys, state->inner_kinds,
											payload_size, capacity,
											&size, &state->status));
	state->region = MemoryContextAllocExtended(state->table_context, size,
											   MCXT_ALLOC_HUGE | MCXT_ALLOC_ZERO);
	state->region_len = size;
	check(state, state->kernels->table_create(state->region, size, state->nkeys,
											  state->inner_kinds,
											  payload_size, capacity,
											  &state->status));
	note_memory(state);
}

/*
 * Twice the region: repalloc keeps the records where they are, and the
 * table rebuilds its buckets at the new end.
 */
static void
grow_table(TessHashJoinState *state)
{
	Size		len = state->region_len;

	if (len > MaxAllocHugeSize / 2)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin hash table cannot grow past %zu bytes", len)));
	state->region = repalloc_huge(state->region, len * 2);
	state->region_len = len * 2;
	check(state, state->kernels->table_grow(state->region, state->region_len,
											&state->status));
	state->counters[JOIN_GROWS]++;
	note_memory(state);
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
		state->payload[row * width] = 0;
	/* The columns' NULL bits, gathered below for the whole table. */
	for (int word = 0; word < state->npayload; word++)
	{
		int			scan_column = state->payload_columns[word];
		int16		typlen = state->typlens[scan_column];
		bool		byval = state->typbyvals[scan_column];
		TessDatumColumn values;
		MemoryContext oldcontext;

		child_column(batch, state->child_columns[scan_column], valid,
					 TESS_COLUMN_FOR_PROJECTION, &values);
		oldcontext = MemoryContextSwitchTo(state->values_context);
		row = -1;
		while ((row = tess_row_mask_next(valid, row)) >= 0)
		{
			uint64	   *record = &state->payload[row * width];

			if (values.isnull[row])
			{
				record[0] |= UINT64CONST(1) << word;
				record[1 + word] = 0;
				state->null_columns |= UINT64CONST(1) << word;
			}
			else
				record[1 + word] = byval ? values.values[row] :
					datumCopy(values.values[row], false, typlen);
		}
		MemoryContextSwitchTo(oldcontext);
	}
}

/* Insert the rows of one inner batch, growing the region until they fit. */
static void
insert_batch(TessHashJoinState *state, TessBatch *batch)
{
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask valid;
	TessRowMask pending;
	TessRowMask duplicates;
	int			count;

	reserve_rows(state, nrows);
	/* A shorter batch than the last: no bits past its rows may remain. */
	memset(state->valid_bits, 0, sizeof(uint64) * nwords);
	memset(state->duplicate_bits, 0, sizeof(uint64) * nwords);
	valid = (TessRowMask) {nrows, state->valid_bits};
	pending = (TessRowMask) {nrows, state->pending_bits};
	duplicates = (TessRowMask) {nrows, state->duplicate_bits};
	batch_keys(state, batch, state->inner_keys, state->inner_kinds, &valid);
	count = tess_row_mask_count(&valid);
	if (count == 0)
		return;
	fill_payload(state, batch, &valid);
	memcpy(state->pending_bits, state->valid_bits, sizeof(uint64) * nwords);
	for (;;)
	{
		/*
		 * A key's records next to each other: the rounds step from one to
		 * the next, and a table without duplicates has no second round.
		 */
		check(state, state->kernels->table_insert_grouped(state->region,
														  state->region_len,
														  state->hashes,
														  state->nkeys,
														  state->table_keys,
														  (const uint8 *) state->payload,
														  &pending,
														  state->offsets,
														  &duplicates,
														  &state->status));
		state->duplicates += tess_row_mask_count(&duplicates);
		if (tess_row_mask_count(&pending) == 0)
			break;
		/* The table is full: the rows left pending go in after growth. */
		grow_table(state);
	}
	state->build_rows += count;
	state->counters[JOIN_BUILD_ROWS] += count;
	note_memory(state);
}

/* Read every batch of the inner child into a new table. */
static void
build_table(TessHashJoinState *state)
{
	create_table(state);
	/* Another table, maybe with other duplicates: decide compact mode again. */
	state->compact_decided = false;
	state->build_rows = 0;
	state->duplicates = 0;
	state->null_columns = 0;
	/* A column without NULLs is never gathered for them: no flag may stay set. */
	if (state->inner_isnull != NULL && state->capacity > 0)
		for (int word = 0; word < state->npayload; word++)
			memset(state->inner_isnull[word], 0, sizeof(bool) * state->capacity);
	for (;;)
	{
		TessBatch  *batch = tess_input_next(state->inner_input);

		if (batch == NULL)
			break;
		if (tess_row_mask_count(&batch->rows) > 0)
			insert_batch(state, batch);
		tess_input_finish(state->inner_input);
	}
	if (state->build_rows > 0)
	{
		TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);

		check(state, state->kernels->table_stats(state->region,
												 state->region_len, &stats,
												 &state->status));
		state->counters[JOIN_BUCKETS] += stats.buckets;
	}
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

	if (state->gathered[word])
		return;
	/* A column no inner row left NULL keeps its flags false. */
	if (nullable && !state->nulls_gathered)
	{
		check(state, state->kernels->table_gather(state->region,
												  state->region_len,
												  state->current_offsets, &round, 0,
												  state->null_words,
												  &state->status));
		state->nulls_gathered = true;
	}
	check(state, state->kernels->table_gather(state->region, state->region_len,
											  state->current_offsets, &round,
											  sizeof(uint64) * (1 + word),
											  state->inner_values[word],
											  &state->status));
	if (nullable)
		while ((row = tess_row_mask_next(&round, row)) >= 0)
			state->inner_isnull[word][row] =
				(DatumGetUInt64(state->null_words[row]) >> word) & 1;
	state->gathered[word] = true;
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
		if (state->compact_values[column] == NULL)
			elog(ERROR, "TessHashJoin column %d was not requested", column);
		result->values = state->compact_values[column];
		result->isnull = state->compact_isnull[column];
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
	result->values = state->inner_values[word - 1];
	result->isnull = state->inner_isnull[word - 1];
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
	TessRowMask round = {nrows, state->round_bits};

	memcpy(state->published_bits, state->round_bits,
		   sizeof(uint64) * tess_row_mask_word_count(nrows));
	state->batch.rows.nrows = nrows;
	state->batch.rows.bits = state->published_bits;
	state->current_offsets = state->offsets;
	state->current_bits = state->round_bits;
	state->nulls_gathered = false;
	memset(state->gathered, 0, sizeof(bool) * Max(state->npayload, 1));
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

	for (int word = 0; word < nwords; word++)
	{
		state->published_bits[word] = outer->rows.bits[word] &
			~state->matched_bits[word];
		any |= state->published_bits[word];
	}
	if (any == 0)
		return false;
	state->null_round = true;
	state->batch.rows.nrows = nrows;
	state->batch.rows.bits = state->published_bits;
	state->current_offsets = state->offsets;
	state->current_bits = state->published_bits;
	state->nulls_gathered = false;
	memset(state->gathered, 0, sizeof(bool) * Max(state->npayload, 1));
	return true;
}

/*
 * Probe the table with one outer batch: the rows whose key found a record
 * become the first round. False when none did.
 */
static bool
probe_batch(TessHashJoinState *state, TessBatch *batch)
{
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask valid;
	TessRowMask found;

	reserve_rows(state, nrows);
	/* A shorter batch than the last: no bits past its rows may remain. */
	memset(state->valid_bits, 0, sizeof(uint64) * nwords);
	memset(state->round_bits, 0, sizeof(uint64) * nwords);
	valid = (TessRowMask) {nrows, state->valid_bits};
	found = (TessRowMask) {nrows, state->round_bits};
	batch_keys(state, batch, state->outer_keys, state->outer_kinds, &valid);
	if (tess_row_mask_count(&valid) == 0)
		return false;
	check(state, state->kernels->table_probe(state->region, state->region_len,
											 state->hashes, state->nkeys,
											 state->table_keys, &valid,
											 state->offsets, &found,
											 &state->status));
	return tess_row_mask_count(&found) > 0;
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

		if (state->outer_batch != NULL && !state->null_round)
		{
			int			nrows = state->outer_batch->rows.nrows;

			/* Without duplicates no row has a next record. */
			if (!state->inner_unique && state->duplicates > 0)
			{
				TessRowMask rows = {nrows, state->next_bits};
				TessRowMask found = {nrows, state->round_bits};

				memcpy(state->next_bits, state->round_bits,
					   sizeof(uint64) * tess_row_mask_word_count(nrows));
				check(state, state->kernels->table_next_in_group(state->region,
																 state->region_len,
																 state->offsets,
																 &rows, &found,
																 &state->status));
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
			tess_input_finish(state->outer_input);
			state->outer_batch = NULL;
		}
		batch = tess_input_next(state->outer_input);
		if (batch == NULL)
			return false;
		state->counters[JOIN_PROBE_ROWS] += tess_row_mask_count(&batch->rows);
		found = probe_batch(state, batch);
		if (state->jointype == JOIN_LEFT)
		{
			int			nwords = tess_row_mask_word_count(batch->rows.nrows);

			/* Without join clauses every row found has its match. */
			state->outer_batch = batch;
			if (found && state->qual == NULL)
				memcpy(state->matched_bits, state->round_bits, sizeof(uint64) * nwords);
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
			tess_input_finish(state->outer_input);
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

		if (!state->round_open)
		{
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
				tess_row_mask_count(&(TessRowMask) {nrows, state->round_bits}) >=
				JOIN_DENSE_ROUND)
			{
				state->output_compact = false;
				return true;
			}
			memcpy(state->taken_bits, state->round_bits,
				   sizeof(uint64) * tess_row_mask_word_count(nrows));
			for (int index = 0; index < state->nouter; index++)
			{
				int			column = state->outer_columns[index];

				child_column(state->outer_batch, state->child_columns[column],
							 &state->outer_batch->rows,
							 TESS_COLUMN_FOR_PROJECTION,
							 &state->round_columns[index]);
			}
			state->round_open = true;
		}
		nrows = state->outer_batch->rows.nrows;
		nwords = tess_row_mask_word_count(nrows);
		for (int index = 0; index < nwords && count < JOIN_COMPACT_ROWS; index++)
		{
			uint64		bits = state->taken_bits[index];

			while (bits != 0 && count < JOIN_COMPACT_ROWS)
			{
				int			row = index * 64 + pg_rightmost_one_pos64(bits);

				bits &= bits - 1;
				state->compact_offsets[count] = state->offsets[row];
				for (int column = 0; column < state->nouter; column++)
				{
					int			scan = state->outer_columns[column];

					Datum		value = state->round_columns[column].values[row];
					bool		isnull = state->round_columns[column].isnull[row];

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
					state->compact_values[scan][count] = value;
					state->compact_isnull[scan][count] = isnull;
				}
				count++;
			}
			state->taken_bits[index] = bits;
		}
		/* A round copied whole: the next call of next_round advances. */
		for (int index = 0; index < nwords; index++)
			if (state->taken_bits[index] != 0)
				goto more;
		state->round_open = false;
more:
		;
	}
	if (count == 0)
		return false;
	state->output_compact = true;
	state->compact_bits[0] = count == 64 ? ~UINT64CONST(0) :
		(UINT64CONST(1) << count) - 1;
	state->counters[JOIN_COMPACT_BATCHES]++;
	state->batch.rows.nrows = JOIN_COMPACT_ROWS;
	state->batch.rows.bits = state->compact_bits;
	state->current_offsets = state->compact_offsets;
	state->current_bits = state->compact_bits;
	state->nulls_gathered = false;
	memset(state->gathered, 0, sizeof(bool) * Max(state->npayload, 1));
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

		if (state->outer_batch != NULL)
		{
			tess_input_finish(state->outer_input);
			state->outer_batch = NULL;
		}
		batch = tess_input_next(state->outer_input);
		if (batch == NULL)
			return false;
		state->counters[JOIN_PROBE_ROWS] += tess_row_mask_count(&batch->rows);
		nrows = batch->rows.nrows;
		nwords = tess_row_mask_word_count(nrows);
		reserve_rows(state, nrows);
		memset(state->matched_bits, 0, sizeof(uint64) * nwords);
		state->outer_batch = batch;
		if (state->build_rows > 0 && probe_batch(state, batch))
		{
			if (state->qual == NULL)
				memcpy(state->matched_bits, state->round_bits, sizeof(uint64) * nwords);
			else
				for (;;)
				{
					TessRowMask round = {nrows, state->round_bits};
					TessRowMask rest = {nrows, state->next_bits};
					uint64		left = 0;

					start_round(state);
					ResetExprContext(econtext);
					(void) tess_qual_apply(state->qual, &state->batch, econtext,
										   tess_row_mask_count(&round));
					for (int word = 0; word < nwords; word++)
						state->matched_bits[word] |= state->published_bits[word];
					if (state->inner_unique || state->duplicates == 0)
						break;
					for (int word = 0; word < nwords; word++)
					{
						state->next_bits[word] = state->round_bits[word] &
							~state->matched_bits[word];
						left |= state->next_bits[word];
					}
					if (left == 0)
						break;
					check(state, state->kernels->table_next_in_group(state->region,
																	 state->region_len,
																	 state->offsets,
																	 &rest, &round,
																	 &state->status));
					if (tess_row_mask_count(&round) == 0)
						break;
				}
		}
		/* The rows returned: SEMI the matched ones, ANTI the others. */
		for (int word = 0; word < nwords; word++)
		{
			state->published_bits[word] = state->jointype == JOIN_SEMI ?
				state->matched_bits[word] :
				batch->rows.bits[word] & ~state->matched_bits[word];
			any |= state->published_bits[word];
		}
		if (any == 0)
			continue;
		state->batch.rows.nrows = nrows;
		state->batch.rows.bits = state->published_bits;
		state->current_offsets = state->offsets;
		state->current_bits = state->published_bits;
		state->nulls_gathered = false;
		memset(state->gathered, 0, sizeof(bool) * Max(state->npayload, 1));
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
		if (state->compact ? !fill_compact(state) : !next_round(state))
			return false;
		/* The join clauses decide the pairs; the rows without one have none. */
		if (state->qual != NULL && !state->null_round)
		{
			ResetExprContext(econtext);
			if (tess_qual_apply(state->qual, &state->batch, econtext,
								tess_row_mask_count(&state->batch.rows)) == 0)
				continue;
			/* LEFT: the pairs that passed; no compact batch with join clauses. */
			if (state->jointype == JOIN_LEFT)
				for (int word = 0; word < tess_row_mask_word_count(state->batch.rows.nrows); word++)
					state->matched_bits[word] |= state->published_bits[word];
		}
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

	for (int key = 0; key < state->nkeys; key++)
	{
		outer_key = bms_add_member(outer_key, state->outer_keys[key]);
		inner_key = bms_add_member(inner_key, state->inner_keys[key]);
	}
	/* The residual clauses read their columns of the pairs too. */
	if (state->qual != NULL)
		needed = bms_add_members(needed, tess_qual_columns(state->qual));
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
			state->outer_columns[state->nouter++] = column;
			continue;
		}
		if (state->npayload == JOIN_MAX_PAYLOAD)
			elog(ERROR, "TessHashJoin keeps at most %d inner columns",
				 JOIN_MAX_PAYLOAD);
		inner_columns = bms_add_member(inner_columns, state->child_columns[column]);
		state->payload_columns[state->npayload] = column;
		state->payload_words[column] = ++state->npayload;
	}
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
	state->inner_values = palloc0_array(Datum *, Max(state->npayload, 1));
	state->inner_isnull = palloc0_array(bool *, Max(state->npayload, 1));
	state->gathered = palloc0_array(bool, Max(state->npayload, 1));
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
	ListCell   *side;
	ListCell   *column;
	int			index = 0;

	state->residual_batch = tess_plan_read_int_list(reader, "residual_batch");
	state->filter_batch = tess_plan_read_int_list(reader, "filter_batch");
	state->jointype = (JoinType) tess_plan_read_int(reader, "jointype");
	state->inner_unique = tess_plan_read_int(reader, "inner_unique") != 0;
	state->inner_rows = tess_plan_read_int(reader, "inner_rows");
	tess_plan_reader_finish(reader);
	state->nkeys = list_length(outer_keys);
	if (list_length(sides) != state->ncolumns ||
		list_length(columns) != state->ncolumns ||
		state->nkeys < 1 || state->nkeys > TESS_TABLE_MAX_KEYS ||
		list_length(inner_keys) != state->nkeys ||
		list_length(outer_kinds) != state->nkeys ||
		list_length(inner_kinds) != state->nkeys ||
		(state->jointype != JOIN_INNER && state->jointype != JOIN_SEMI &&
		 state->jointype != JOIN_ANTI && state->jointype != JOIN_LEFT) ||
		(state->filter_batch != NIL &&
		 state->jointype != JOIN_LEFT && state->jointype != JOIN_ANTI))
		elog(ERROR, "TessHashJoin received foreign plan data");
	for (int key = 0; key < state->nkeys; key++)
	{
		state->outer_keys[key] = list_nth_int(outer_keys, key);
		state->inner_keys[key] = list_nth_int(inner_keys, key);
		state->outer_kinds[key] = list_nth_int(outer_kinds, key);
		state->inner_kinds[key] = list_nth_int(inner_kinds, key);
		if ((state->outer_kinds[key] != TESS_TABLE_KEY_INT4 &&
			 state->outer_kinds[key] != TESS_TABLE_KEY_INT8) ||
			(state->inner_kinds[key] != TESS_TABLE_KEY_INT4 &&
			 state->inner_kinds[key] != TESS_TABLE_KEY_INT8))
			elog(ERROR, "TessHashJoin received foreign plan data");
	}
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
	state->outer_columns = palloc0_array(int, state->ncolumns);
	state->compact_values = palloc0_array(Datum *, state->ncolumns);
	state->compact_isnull = palloc0_array(bool *, state->ncolumns);
	state->round_columns = palloc0_array(TessDatumColumn, Max(state->ncolumns, 1));
	state->payload_columns = palloc0_array(int, JOIN_MAX_PAYLOAD);

	state->outer = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
	state->inner = ExecInitNode(lsecond(cscan->custom_plans), estate, eflags);
	css->custom_ps = list_make2(state->outer, state->inner);
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
	/* The residual clauses: those the compiler took in batches, then by rows. */
	state->scan_layout = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	state->scan_layout.ncolumns = state->ncolumns;
	state->scan_layout.ntargets = state->ncolumns;
	/* custom_exprs: the key clauses, the residual ones, an outer join's filters. */
	if (list_length(cscan->custom_exprs) !=
		state->nkeys + list_length(state->residual_batch) +
		list_length(state->filter_batch))
		elog(ERROR, "TessHashJoin received a foreign plan");
	if (state->filter_batch != NIL)
	{
		TessQualConfig filter = TESS_STRUCT_INITIALIZER(TessQualConfig);
		List	   *filters = list_copy_tail(cscan->custom_exprs,
											 state->nkeys +
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
															  state->nkeys),
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
static void
decide_compact(TessHashJoinState *state)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;

	state->compact_decided = true;
	state->compact = false;
	/*
	 * SEMI and ANTI return outer rows, not pairs; LEFT with join clauses
	 * must see each round whole to know the rows without a match.
	 */
	if (state->request->output_mode != TESS_OUTPUT_BATCH ||
		state->inner_unique || state->duplicates == 0 ||
		state->jointype == JOIN_SEMI || state->jointype == JOIN_ANTI ||
		(state->jointype == JOIN_LEFT && state->qual != NULL))
		return;
	for (int index = 0; index < state->nouter; index++)
	{
		int			column = state->outer_columns[index];

		if (state->compact_values[column] != NULL)
			continue;
		state->compact_values[column] =
			MemoryContextAllocZero(context, sizeof(Datum) * JOIN_COMPACT_ROWS);
		state->compact_isnull[column] =
			MemoryContextAllocZero(context, sizeof(bool) * JOIN_COMPACT_ROWS);
	}
	/* The copies of by-reference values, for one compact batch at a time. */
	if (state->compact_context == NULL)
		state->compact_context = AllocSetContextCreate(context,
													   "TessHashJoin compact values",
													   ALLOCSET_DEFAULT_SIZES);
	state->compact = true;
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
		build_table(state);
	/*
	 * Nothing to match: the outer child is never read, as in the core,
	 * unless its rows go out without a match (LEFT, ANTI).
	 */
	if (state->build_rows == 0 &&
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
	state->round_open = false;
	state->null_round = false;
	state->null_held = false;
	state->serving = false;
	state->next_row = -1;
	state->done = false;
	if (css->ss.ps.chgParam != NULL)
	{
		UpdateChangedParamSet(state->outer, css->ss.ps.chgParam);
		UpdateChangedParamSet(state->inner, css->ss.ps.chgParam);
	}
	if (state->inner->chgParam != NULL)
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
	values[JOIN_OVERRUN] = state->peak_memory > limit ?
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
	const uint64 *totals = NULL;
	uint64		own[JOIN_NCOUNTERS];

	context = set_deparse_context_plan(es->deparse_cxt, css->ss.ps.plan,
									   ancestors);
	if (state->jointype != JOIN_INNER)
		ExplainPropertyText("Join Type",
							state->jointype == JOIN_SEMI ? "Semi" :
							state->jointype == JOIN_ANTI ? "Anti" : "Left", es);
	ExplainPropertyText("Hash Cond",
						deparse_expression((Node *) make_ands_explicit(list_copy_head(cscan->custom_exprs,
																					  state->nkeys)),
										   context, useprefix, false), es);
	for (int part = 0; part < 2; part++)
	{
		/*
		 * The residual join clauses, then an outer join's filters: those
		 * the compiler took in batches, and the others.
		 */
		List	   *flags = part == 0 ? state->residual_batch : state->filter_batch;
		int			first = state->nkeys +
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
	if (state->stats != NULL)
		totals = tess_shared_stats_totals(state->stats);
	if (totals == NULL)
	{
		join_counters(state, own);
		totals = own;
	}
	ExplainPropertyInteger("Buckets", NULL,
						   totals[JOIN_BUILDS] > 0 ?
						   totals[JOIN_BUCKETS] / totals[JOIN_BUILDS] : 0, es);
	ExplainPropertyInteger("Memory Usage", "kB",
						   (totals[JOIN_MEMORY] + 1023) / 1024, es);
	if (totals[JOIN_OVERRUN] > 0)
		ExplainPropertyInteger("Overrun", "kB",
							   (totals[JOIN_OVERRUN] + 1023) / 1024, es);
	ExplainPropertyInteger("Builds", NULL, totals[JOIN_BUILDS], es);
	ExplainPropertyInteger("Build Rows", NULL, totals[JOIN_BUILD_ROWS], es);
	ExplainPropertyInteger("Table Grows", NULL, totals[JOIN_GROWS], es);
	ExplainPropertyInteger("Probe Rows", NULL, totals[JOIN_PROBE_ROWS], es);
	ExplainPropertyInteger("Matches", NULL, totals[JOIN_MATCHES], es);
	if (state->qual != NULL)
		ExplainPropertyInteger("Rows Removed by Join Filter", NULL,
							   totals[JOIN_FILTER_REMOVED], es);
	if (state->filter != NULL)
		ExplainPropertyInteger("Rows Removed by Filter", NULL,
							   totals[JOIN_OUTPUT_REMOVED], es);
	if (totals[JOIN_COMPACT_BATCHES] > 0)
		ExplainPropertyInteger("Compact Batches", NULL,
							   totals[JOIN_COMPACT_BATCHES], es);
}

/*
 * A parallel plan: the outer child divides the rows, and every
 * participant builds the whole inner side into a table of its own, as the
 * core's hash join without a shared table does. The node shares only its
 * counters, in the rows of its chunk.
 */
static Size
join_estimate_dsm(CustomScanState *css, ParallelContext *pcxt)
{
	return tess_shared_stats_estimate(JOIN_NCOUNTERS, pcxt->nworkers);
}

static void
join_initialize_dsm(CustomScanState *css, ParallelContext *pcxt,
					void *coordinate)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	/* A Gather a limit above shut down sets up anew when rescanned. */
	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	state->stats = tess_shared_stats_init(css->ss.ps.state->es_query_cxt,
										  coordinate, JOIN_NCOUNTERS,
										  pcxt->nworkers, pcxt->seg);
}

static void
join_reinitialize_dsm(CustomScanState *css, ParallelContext *pcxt,
					  void *coordinate)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	tess_shared_stats_reset(state->stats);
}

static void
join_initialize_worker(CustomScanState *css, shm_toc *toc, void *coordinate)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	state->stats = tess_shared_stats_attach(css->ss.ps.state->es_query_cxt,
											coordinate, ParallelWorkerNumber + 1);
}

static void
join_shutdown(CustomScanState *css)
{
	TessHashJoinState *state = (TessHashJoinState *) css;
	uint64		values[JOIN_NCOUNTERS];

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

/*
 * TessSort under a bound (a LIMIT above): the rows kept in a heap of the
 * bound's best, compacted when it overflows. See "Top-N" in docs/nodes.md.
 */
#include "postgres.h"

#include "catalog/pg_collation_d.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "lib/binaryheap.h"
#include "utils/builtins.h"
#include "utils/datum.h"
#include "utils/lsyscache.h"
#include "utils/ruleutils.h"

#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"
#include "sort_node.h"

static void compact_rows(TessSortState *state);

/* The reference of a heap item: the low 32 bits of its last word. */
static uint32
item_ref(TessSortState *state, uint64 item)
{
	return (uint32) state->heap[item * state->words + state->words - 1];
}

/* The values of item slot's keys from the first generic one on. */
static inline Datum *
top_slot_values(TessSortState *state, uint64 slot)
{
	return &state->top_values[slot * (state->nkeys - state->generic)];
}

static inline bool *
top_slot_isnull(TessSortState *state, uint64 slot)
{
	return &state->top_isnull[slot * (state->nkeys - state->generic)];
}

/*
 * The values of the generic keys of records refs[row] for the rows of
 * mask, into the slots from `first` on, in the order of the rows.
 */
static void
top_gather(TessSortState *state, TessRows *rows, const uint32 *refs,
		   const TessRowMask *mask, uint64 first)
{
	int			ngeneric = state->nkeys - state->generic;

	for (int key = state->generic; key < state->nkeys; key++)
	{
		int			row = -1;
		uint64		slot = first;

		tess_rows_gather(rows, state->key_columns[key], refs, mask,
						 state->top_gathered, state->top_gathered_null);
		while ((row = tess_row_mask_next(mask, row)) >= 0)
		{
			state->top_values[slot * ngeneric + key - state->generic] = state->top_gathered[row];
			state->top_isnull[slot * ngeneric + key - state->generic] = state->top_gathered_null[row];
			slot++;
		}
	}
}

/*
 * The order of two slots of a generic key's heap: their words, the
 * reference's bits left out, then the comparisons.
 */
static int
compare_slots(TessSortState *state, uint64 a, uint64 b)
{
	const uint64 *x = &state->heap[a * state->words];
	const uint64 *y = &state->heap[b * state->words];
	int			last = state->words - 1;
	Datum	   *xv = top_slot_values(state, a);
	Datum	   *yv = top_slot_values(state, b);
	bool	   *xn = top_slot_isnull(state, a);
	bool	   *yn = top_slot_isnull(state, b);

	for (int word = 0; word < last; word++)
		if (x[word] != y[word])
			return x[word] < y[word] ? -1 : 1;
	if ((x[last] >> 32) != (y[last] >> 32))
		return (x[last] >> 32) < (y[last] >> 32) ? -1 : 1;
	for (int key = state->generic; key < state->nkeys; key++)
	{
		int			at = key - state->generic;
		int			result = ApplySortComparator(xv[at], xn[at], yv[at], yn[at],
												 &state->ssup[key]);

		if (result != 0)
			return result;
	}
	return 0;
}

/* Copy slot `from` of the heap to slot `to`. */
static void
copy_slot(TessSortState *state, uint64 to, uint64 from)
{
	int			ngeneric = state->nkeys - state->generic;

	memcpy(&state->heap[to * state->words], &state->heap[from * state->words],
		   sizeof(uint64) * state->words);
	memcpy(top_slot_values(state, to), top_slot_values(state, from), sizeof(Datum) * ngeneric);
	memcpy(top_slot_isnull(state, to), top_slot_isnull(state, from), sizeof(bool) * ngeneric);
}

/*
 * The item in the spare slot (heap_capacity) into the max-heap: added
 * while there is room, else in place of the worst when it is better.
 */
static void
top_push_slot(TessSortState *state)
{
	uint64		spare = state->heap_capacity;
	uint64		place;

	if (state->heap_len < state->heap_capacity)
	{
		place = state->heap_len++;
		while (place > 0)
		{
			uint64		parent = (place - 1) / 2;

			if (compare_slots(state, spare, parent) <= 0)
				break;
			copy_slot(state, place, parent);
			place = parent;
		}
		copy_slot(state, place, spare);
		return;
	}
	if (compare_slots(state, spare, 0) >= 0)
		return;
	place = 0;
	for (;;)
	{
		uint64		child = 2 * place + 1;

		if (child >= state->heap_len)
			break;
		if (child + 1 < state->heap_len && compare_slots(state, child + 1, child) > 0)
			child++;
		if (compare_slots(state, child, spare) <= 0)
			break;
		copy_slot(state, place, child);
		place = child;
	}
	copy_slot(state, place, spare);
}

/*
 * Top-N of a generic key: a batch's key columns and its items' lanes
 * first; once the heap is full, the batch keeps only the rows that order
 * before the worst kept by their lanes, or by the comparisons when the
 * lanes are equal; those have their other columns read, are appended and
 * go into the heap one by one.
 */
void
sort_top_batch_generic(TessSortState *state, TessBatch *batch)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	int			nrows = batch->rows.nrows;
	int			ngeneric = state->nkeys - state->generic;
	int			words = state->words;
	uint64		spare = state->heap_capacity;
	int			count;
	int			row = -1;
	int			lane = 0;

	if (nrows > state->top_batch_capacity)
	{
		MemoryContext context = state->css.ss.ps.state->es_query_cxt;

		state->top_batch_capacity = Max(nrows, SORT_ROWS);
		for (int word = 0; word < words; word++)
		{
			if (state->top_lanes[word] != NULL)
				pfree(state->top_lanes[word]);
			state->top_lanes[word] = MemoryContextAlloc(context,
														sizeof(uint64) * state->top_batch_capacity);
		}
		if (state->top_lane_of != NULL)
		{
			pfree(state->top_lane_of);
			pfree(state->top_gathered);
			pfree(state->top_gathered_null);
		}
		state->top_lane_of = MemoryContextAlloc(context, sizeof(int) * state->top_batch_capacity);
		state->top_gathered = MemoryContextAllocZero(context,
													 sizeof(Datum) * state->top_batch_capacity);
		state->top_gathered_null = MemoryContextAllocZero(context,
														  sizeof(bool) * state->top_batch_capacity);
	}
	sort_batch_keys(state, batch);
	for (int key = state->generic + 1; key < state->nkeys; key++)
		sort_batch_column(state, batch, state->key_columns[key]);
	tess_status_check(state->kernels->sort_key_lanes(state->nkernel, state->top_keys,
													 state->table_keys, &batch->rows, words,
													 state->top_lanes, state->top_batch_capacity,
													 &count, &status),
					  &status);
	while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
		state->top_lane_of[row] = lane++;
	if (state->heap_len == state->heap_capacity)
	{
		/* The rows that do not beat the worst kept leave the mask. */
		row = -1;
		while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
		{
			int			at = state->top_lane_of[row];
			int			result = 0;

			for (int word = 0; word < words; word++)
			{
				uint64		x = state->top_lanes[word][at];
				uint64		y = state->heap[word];

				/* The worst's reference is no key. */
				if (word == words - 1)
				{
					x >>= 32;
					y >>= 32;
				}
				if (x != y)
				{
					result = x < y ? -1 : 1;
					break;
				}
			}
			for (int key = state->generic; result == 0 && key < state->nkeys; key++)
			{
				const TessDatumColumn *column = &state->columns[state->key_columns[key]];

				result = ApplySortComparator(column->values[row], column->isnull[row],
											 top_slot_values(state, 0)[key - state->generic],
											 top_slot_isnull(state, 0)[key - state->generic],
											 &state->ssup[key]);
			}
			if (result >= 0)
				batch->rows.bits[row / 64] &= ~(UINT64CONST(1) << (row % 64));
		}
		if (tess_row_mask_count(&batch->rows) == 0)
			return;
	}
	for (int column = 0; column < state->ncolumns; column++)
		if (!sort_is_key_column(state, column))
			sort_batch_column(state, batch, column);
	sort_append_rows(state, batch);
	/* The kept rows' values from their records, which outlive the batch. */
	{
		Datum	   *values = palloc_array(Datum, (Size) nrows * ngeneric);
		bool	   *isnull = palloc_array(bool, (Size) nrows * ngeneric);

		for (int key = state->generic; key < state->nkeys; key++)
			tess_rows_gather(state->rows, state->key_columns[key], state->batch_refs,
							 &batch->rows, &values[(key - state->generic) * nrows],
							 &isnull[(key - state->generic) * nrows]);
		row = -1;
		while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
		{
			int			at = state->top_lane_of[row];
			uint64	   *item = &state->heap[spare * words];

			for (int word = 0; word < words; word++)
				item[word] = state->top_lanes[word][at];
			item[words - 1] |= state->batch_refs[row];
			for (int key = 0; key < ngeneric; key++)
			{
				top_slot_values(state, spare)[key] = values[key * nrows + row];
				top_slot_isnull(state, spare)[key] = isnull[key * nrows + row];
			}
			top_push_slot(state);
		}
		pfree(values);
		pfree(isnull);
	}
	sort_note_memory(state, (state->heap_capacity + 1) *
				(words * sizeof(uint64) + ngeneric * (sizeof(Datum) + sizeof(bool))));
	if (tess_rows_count(state->rows) > Max(4 * state->heap_capacity, 65536))
		compact_rows(state);
}

/*
 * The records outnumber what the heap needs: make the rows anew from the
 * heap's records and the heap from them, so that memory stays bounded when
 * every row beats the ones kept, as keys in the reverse of the order do.
 */
static void
compact_rows(TessSortState *state)
{
	TessRows   *rows = tess_rows_create(&state->rows_config);
	uint64		len = state->heap_len;
	uint32	   *kept = palloc_array(uint32, Max(len, 1));
	uint32		new_refs[SORT_ROWS];
	int			ncolumns = Max(state->ncolumns, 1);
	Datum	   *values = palloc_array(Datum, ncolumns * SORT_ROWS);
	bool	   *nulls = palloc_array(bool, ncolumns * SORT_ROWS);
	TessDatumColumn *columns = palloc_array(TessDatumColumn, ncolumns);
	TessTableKey keys[TESS_TABLE_MAX_KEYS];

	/* The heap is made anew below: its records are taken first. */
	for (uint64 item = 0; item < len; item++)
		kept[item] = item_ref(state, item);
	state->heap_len = 0;
	for (uint64 first = 0; first < len; first += SORT_ROWS)
	{
		int			n = (int) Min((uint64) SORT_ROWS, len - first);
		uint64		bits[1] = {n == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << n) - 1};
		TessRowMask mask = {n, bits};

		for (int column = 0; column < state->ncolumns; column++)
		{
			tess_rows_gather(state->rows, column, &kept[first], &mask,
							 &values[column * SORT_ROWS], &nulls[column * SORT_ROWS]);
			columns[column] = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
			columns[column].values = &values[column * SORT_ROWS];
			columns[column].isnull = &nulls[column * SORT_ROWS];
			columns[column].nrows = n;
		}
		for (int key = 0; key < state->nkernel; key++)
		{
			keys[key].kind = state->keys[key].kind;
			keys[key].column = &columns[state->key_columns[key]];
			keys[key].prepared = NULL;
			if (key == state->generic)
			{
				sort_abbreviate_column(state, keys[key].column, &mask);
				keys[key].column = &state->abbrev_column;
			}
		}
		tess_rows_append(rows, keys, columns, &mask, new_refs);
		if (state->generic >= 0)
		{
			/* The items stay where they are: their references and values change. */
			for (int row = 0; row < n; row++)
			{
				uint64	   *last = &state->heap[(first + row) * state->words + state->words - 1];

				*last = (*last & ~UINT64CONST(0xFFFFFFFF)) | new_refs[row];
			}
			top_gather(state, rows, new_refs, &mask, first);
			continue;
		}
		/* The items of the kept rows go in anew, by their new records. */
		tess_rows_top_push(rows, state->top_keys, new_refs, &mask, state->heap,
						   state->heap_capacity, &state->heap_len);
	}
	if (state->generic >= 0)
		state->heap_len = len;
	pfree(kept);
	pfree(values);
	pfree(nulls);
	pfree(columns);
	tess_rows_free(state->rows);
	state->rows = rows;
	state->compactions++;
}

/*
 * Top-N: a batch's key columns first; once the heap is full, the batch
 * keeps only the rows whose keys beat the worst kept, and only those have
 * their other columns read, are appended and go into the heap.
 */
void
sort_top_batch(TessSortState *state, TessBatch *batch)
{
	sort_batch_keys(state, batch);
	if (state->heap_len == state->heap_capacity)
	{
		TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
		int			kept;

		tess_status_check(state->kernels->sort_top_candidates(state->nkernel,
															  state->top_keys,
															  state->table_keys,
															  &batch->rows,
															  state->heap, &kept,
															  &status),
						  &status);
		if (kept == 0)
			return;
	}
	for (int column = 0; column < state->ncolumns; column++)
		if (!sort_is_key_column(state, column))
			sort_batch_column(state, batch, column);
	sort_append_rows(state, batch);
	tess_rows_top_push(state->rows, state->top_keys, state->batch_refs,
					   &batch->rows, state->heap, state->heap_capacity,
					   &state->heap_len);
	sort_note_memory(state, state->heap_capacity * state->words * sizeof(uint64));
	if (tess_rows_count(state->rows) > Max(4 * state->heap_capacity, 65536))
		compact_rows(state);
}

/*
 * Whether a bound makes a top-N sort: its heap and the rows that may be
 * appended before a rebuild fit work_mem. Every key takes its bit for NULL.
 */
bool
sort_choose_topn(TessSortState *state)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	double		bytes;

	if (state->bound < 0)
		return false;
	for (int key = 0; key < state->nkernel; key++)
	{
		state->top_keys[key] = state->keys[key];
		state->top_keys[key].flags |= TESS_SORT_NULLABLE;
	}
	tess_status_check(state->kernels->sort_item_words(state->nkernel, state->top_keys,
													  &state->words, &status),
					  &status);
	bytes = (double) state->bound * state->words * sizeof(uint64) +
		(double) Max(4 * (double) state->bound, 65536.0) *
		(16.0 + 8.0 * (state->nkernel + (state->ncolumns + 63) / 64 + state->ncolumns));
	/* A generic key's heap: a slot more, and the values of its keys. */
	if (state->generic >= 0)
		bytes += (double) (state->bound + 1) *
			(state->words * sizeof(uint64) +
			 (state->nkeys - state->generic) * (sizeof(Datum) + sizeof(bool)));
	return bytes <= (double) work_mem * 1024.0;
}

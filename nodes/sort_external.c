/*
 * TessSort past work_mem: sorted runs written to files and merged, pass
 * after pass, into the output. See "External sort" in docs/nodes.md.
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

/* The words of a run's chunk of columns: NULL bits, the columns, the item's words. */
int
sort_run_words(TessSortState *state)
{
	return state->ncolumns + state->ext_words;
}

/* The set runs are written into ends its writes; its runs can be read. */
void
sort_set_finish(TessSortState *state)
{
	if (state->writing == NULL)
		return;
	tess_spill_finish(state->writing->file);
	state->writing->finished = true;
	state->writing = NULL;
}

/* A run of no blocks yet, the next partition of the set being written. */
static SortRun *
run_create(TessSortState *state)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	SortRun    *run = MemoryContextAllocZero(context, sizeof(SortRun));

	if (state->writing != NULL && state->writing->used == SORT_SET_RUNS)
		sort_set_finish(state);
	if (state->writing == NULL)
	{
		TessSpillConfig config = TESS_STRUCT_INITIALIZER(TessSpillConfig);
		RunSet	   *set = MemoryContextAllocZero(context, sizeof(RunSet));

		config.parent_context = context;
		config.kernels = state->kernels;
		config.npartitions = SORT_SET_RUNS;
		config.level = 0;
		config.fingerprint = ((uint64) state->ncolumns << 32) | (uint64) state->ext_words;
		config.max_len = (uint64) MaxAllocHugeSize;
		config.buffer_len = TESS_SPILL_BUFFER_LEN((Size) work_mem * 1024);
		set->file = tess_spill_create(&config);
		state->writing = set;
	}
	run->set = state->writing;
	run->partition = state->writing->used++;
	run->set->alive++;
	if (state->nruns == state->run_slots)
	{
		state->run_slots = Max(state->run_slots * 2, 8);
		state->runs = state->runs == NULL ?
			MemoryContextAlloc(context, sizeof(SortRun *) * state->run_slots) :
			repalloc(state->runs, sizeof(SortRun *) * state->run_slots);
	}
	return run;
}

/* Forget a run; its set goes with its last run. */
static void
run_free(SortRun *run)
{
	RunSet	   *set = run->set;

	tess_spill_drop(set->file, run->partition);
	if (--set->alive == 0 && set->finished)
	{
		tess_spill_free(set->file);
		pfree(set);
	}
	if (run->positions != NULL)
	{
		pfree(run->positions);
		pfree(run->block_rows);
		pfree(run->block_first);
	}
	pfree(run);
}

static void
writer_reset_chunk(TessSortState *state, RunWriter *writer)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	Size		capacity;

	tess_status_check(state->kernels->spill_columns_init(writer->chunk, writer->chunk_len,
														 sort_run_words(state), &capacity,
														 &status),
					  &status);
	writer->capacity = (uint32) capacity;
	writer->rows = 0;
	writer->values_used = 0;
}

static void
writer_start(TessSortState *state, RunWriter *writer, SortRun *run)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;

	writer->run = run;
	writer->null_lanes = tess_spill_columns_null_lanes(sort_run_words(state));
	writer->chunk_len = TESS_SPILL_COLUMNS_HEADER +
		sizeof(uint64) * (Size) state->block_rows * (writer->null_lanes + sort_run_words(state));
	writer->chunk = MemoryContextAllocExtended(context, writer->chunk_len, MCXT_ALLOC_HUGE);
	writer->values_len = state->block_values;
	writer->values = MemoryContextAllocExtended(context, writer->values_len, MCXT_ALLOC_HUGE);
	writer_reset_chunk(state, writer);
}

/* Write the block pair the writer holds: its values, then its rows. */
static void
writer_flush(TessSortState *state, RunWriter *writer)
{
	SortRun    *run = writer->run;
	TessSpillPosition position;

	if (writer->rows == 0)
		return;
	if (run->nblocks == run->slots)
	{
		MemoryContext context = state->css.ss.ps.state->es_query_cxt;

		run->slots = Max(run->slots * 2, 16);
		run->positions = run->positions == NULL ?
			MemoryContextAlloc(context, sizeof(TessSpillPosition) * run->slots) :
			repalloc(run->positions, sizeof(TessSpillPosition) * run->slots);
		run->block_rows = run->block_rows == NULL ?
			MemoryContextAlloc(context, sizeof(uint32) * run->slots) :
			repalloc(run->block_rows, sizeof(uint32) * run->slots);
		run->block_first = run->block_first == NULL ?
			MemoryContextAlloc(context, sizeof(uint64) * run->slots) :
			repalloc(run->block_first, sizeof(uint64) * run->slots);
	}
	tess_spill_columns_set_rows(writer->chunk, writer->rows);
	state->disk_bytes += tess_spill_write(run->set->file, run->partition, TESS_SPILL_VALUES,
										  (uint32) run->nblocks, writer->values,
										  writer->values_used, &position);
	state->disk_bytes += tess_spill_write(run->set->file, run->partition, TESS_SPILL_COLUMNS,
										  (uint32) run->nblocks, writer->chunk,
										  writer->chunk_len, NULL);
	run->positions[run->nblocks] = position;
	run->block_rows[run->nblocks] = writer->rows;
	run->block_first[run->nblocks] = run->rows;
	run->rows += writer->rows;
	run->nblocks++;
	writer_reset_chunk(state, writer);
}

static void
writer_finish(TessSortState *state, RunWriter *writer)
{
	writer_flush(state, writer);
	pfree(writer->chunk);
	pfree(writer->values);
	state->runs[state->nruns++] = writer->run;
	state->runs_written++;
}

/*
 * Append n rows to the run: column c of row r is values[c][r] unless
 * isnull[c][r], and its item's words are at keys[r]. A by-reference value
 * is copied into the block's values; a block fills with rows or values.
 */
static void
writer_add(TessSortState *state, RunWriter *writer, int n, Datum *const *values,
		   bool *const *isnull, const uint64 *const *keys)
{
	const int16 *typlens = state->rows_config.typlens;
	const bool *typbyvals = state->rows_config.typbyvals;
	Size		capacity = writer->capacity;
	bool		byref = false;

	for (int column = 0; column < state->ncolumns; column++)
		byref |= !typbyvals[column];
	/*
	 * By-value columns only: each column's words copied into its lane a
	 * run of rows at a time, the NULL bits set where a column has NULLs;
	 * row by row this was 7 % of an external sort.
	 */
	if (!byref)
	{
		int			row = 0;

		while (row < n)
		{
			int			take;
			uint64	   *nulls;
			uint64	   *words;

			if (writer->rows == writer->capacity)
				writer_flush(state, writer);
			take = Min(n - row, (int) (writer->capacity - writer->rows));
			nulls = tess_spill_columns_lane(writer->chunk, 0) + writer->rows;
			words = nulls + capacity * writer->null_lanes;
			for (int lane = 0; lane < writer->null_lanes; lane++)
				memset(nulls + capacity * lane, 0, sizeof(uint64) * take);
			for (int column = 0; column < state->ncolumns; column++)
			{
				uint64	   *lane = words + capacity * column;
				uint64	   *column_nulls = nulls +
					capacity * tess_spill_columns_null_lane(column);
				const bool *flags = &isnull[column][row];

				memcpy(lane, &values[column][row], sizeof(uint64) * take);
				if (memchr(flags, true, take) == NULL)
					continue;
				for (int at = 0; at < take; at++)
					if (flags[at])
					{
						column_nulls[at] |= tess_spill_columns_null_bit(column);
						lane[at] = 0;
					}
			}
			for (int word = 0; word < state->ext_words; word++)
			{
				uint64	   *lane = words + capacity * (state->ncolumns + word);

				for (int at = 0; at < take; at++)
					lane[at] = keys[row + at][word];
			}
			writer->rows += take;
			row += take;
		}
		return;
	}
	for (int row = 0; row < n; row++)
	{
		Size		need = 0;
		uint32		place;
		uint64	   *nulls;
		uint64	   *words;

		for (int column = 0; column < state->ncolumns; column++)
			if (!isnull[column][row] && !typbyvals[column])
				need += MAXALIGN(datumGetSize(values[column][row], false, typlens[column]));
		if (writer->rows == writer->capacity ||
			(writer->rows > 0 && writer->values_used + need > writer->values_len))
			writer_flush(state, writer);
		if (writer->values_used + need > writer->values_len)
		{
			writer->values_len = Max(writer->values_len * 2, writer->values_used + need);
			writer->values = repalloc_huge(writer->values, writer->values_len);
		}
		place = writer->rows++;
		nulls = tess_spill_columns_lane(writer->chunk, 0) + place;
		words = nulls + capacity * writer->null_lanes;
		for (int lane = 0; lane < writer->null_lanes; lane++)
			nulls[capacity * lane] = 0;
		for (int column = 0; column < state->ncolumns; column++)
		{
			uint64	   *lane = words + capacity * column;

			if (isnull[column][row])
			{
				nulls[capacity * tess_spill_columns_null_lane(column)] |=
					tess_spill_columns_null_bit(column);
				lane[0] = 0;
			}
			else if (typbyvals[column])
				lane[0] = (uint64) values[column][row];
			else
			{
				Size		size = datumGetSize(values[column][row], false, typlens[column]);

				memcpy(writer->values + writer->values_used,
					   DatumGetPointer(values[column][row]), size);
				lane[0] = writer->values_used;
				writer->values_used += MAXALIGN(size);
			}
		}
		for (int word = 0; word < state->ext_words; word++)
			words[capacity * (state->ncolumns + word)] = keys[row][word];
	}
}

/*
 * The rows in memory go to a run, sorted, and memory is freed for the
 * next: their items with every key's bit for NULL, the reference in the
 * last word's low bits left out of the run's lanes.
 */
void
sort_spill_run(TessSortState *state)
{
	uint64		count = tess_rows_count(state->rows);
	uint32	   *refs;
	uint64	   *items;
	int			words;
	RunWriter	writer;
	Datum	  **values = state->values;
	bool	  **nulls = state->isnull;
	const uint64 *keys[SORT_ROWS];
	uint64		copies[SORT_ROWS][TESS_SORT_MAX_ITEM_WORDS];

	if (count == 0)
		return;
	refs = MemoryContextAllocExtended(state->css.ss.ps.state->es_query_cxt,
									  mul_size(sizeof(uint32), count), MCXT_ALLOC_HUGE);
	items = tess_rows_sort_items(state->rows, state->ext_keys, refs, &words);
	if (words != state->item_words)
		elog(ERROR, "TessSort items of %d words, not %d", words, state->item_words);
	if (state->generic >= 0)
		sort_ties(state, &items, words, refs, count, false);
	sort_note_memory(state, mul_size(count, sizeof(uint32) + sizeof(uint64) * words));
	writer_start(state, &writer, run_create(state));
	for (uint64 first = 0; first < count; first += SORT_ROWS)
	{
		int			n = (int) Min((uint64) SORT_ROWS, count - first);
		uint64		bits[1] = {n == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << n) - 1};
		TessRowMask mask = {n, bits};

		tess_rows_gather_columns(state->rows, &refs[first], &mask, values, nulls);
		/* The reference left out: its word dropped, or its bits zeroed in a copy. */
		for (int row = 0; row < n; row++)
		{
			const uint64 *item = &items[(first + row) * words];

			if (state->ext_words < words)
			{
				keys[row] = item;
				continue;
			}
			memcpy(copies[row], item, sizeof(uint64) * words);
			copies[row][words - 1] &= ~UINT64CONST(0xFFFFFFFF);
			keys[row] = copies[row];
		}
		writer_add(state, &writer, n, values, nulls, keys);
		CHECK_FOR_INTERRUPTS();
	}
	writer_finish(state, &writer);
	pfree(items);
	pfree(refs);
	tess_rows_reset(state->rows);
}

/*
 * What an external sort would take: the keys of every run's items, each
 * with its bit for NULL, so that every run's items have one width, and
 * the blocks' size.
 */
void
sort_plan_external(TessSortState *state)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	Size		row_bytes;
	Size		block_bytes;

	for (int key = 0; key < state->nkernel; key++)
	{
		state->ext_keys[key] = state->keys[key];
		state->ext_keys[key].flags |= TESS_SORT_NULLABLE;
	}
	tess_status_check(state->kernels->sort_item_words(state->nkernel, state->ext_keys,
													  &state->item_words, &status),
					  &status);
	/*
	 * A run keeps its items' words without the reference: the last word
	 * goes when it holds no key's bits, as an int4 key's 33 bits leave it,
	 * and the merge compares one word, not two.
	 */
	{
		int			bits = 0;

		for (int key = 0; key < state->nkernel; key++)
			bits += (state->ext_keys[key].kind == TESS_TABLE_KEY_INT8 ? 64 : 32) + 1;
		state->ext_words = bits <= 64 * (state->item_words - 1) ?
			state->item_words - 1 : state->item_words;
	}
	/*
	 * A merge holds a block pair of each run it takes, rows and values, and
	 * briefly the pairs a batch put out points into: blocks of a 128th of
	 * work_mem, 64 rows to 256 kB, so that a work_mem of 4 MB merges 64
	 * runs at once, and a merge of 6 runs at least, as the core's (a small
	 * work_mem is passed then), up to TESS_SORT_MAX_MERGE_RUNS. Blocks of
	 * a 64th and two pairs each took a pass more for 44 runs.
	 */
	row_bytes = sizeof(uint64) *
		(tess_spill_columns_null_lanes(sort_run_words(state)) + sort_run_words(state));
	block_bytes = Min((Size) work_mem * 1024 / 128, (Size) 256 * 1024);
	state->block_rows = (uint32) Max(block_bytes / row_bytes, (Size) SORT_ROWS);
	state->block_values = Max(block_bytes, (Size) 4096);
	block_bytes = Max(block_bytes, (Size) state->block_rows * row_bytes);
	state->fan_in = (int) ((Size) work_mem * 1024 / (2 * block_bytes));
	state->fan_in = Max(state->fan_in, 6);
	state->fan_in = Min(state->fan_in, TESS_SORT_MAX_MERGE_RUNS);
}

/* Blocks rows put out may point into, freed now that those rows are done with. */
static void
free_retired(TessSortState *state)
{
	foreach_ptr(void, block, state->retired)
		pfree(block);
	list_free(state->retired);
	state->retired = NIL;
}

/* The input's next block pair, or false at the run's end; its last one retires. */
static bool
input_load(TessSortState *state, MergeInput *input)
{
	TessSpillHeader header;
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	MemoryContext old;

	if (input->values != NULL)
	{
		old = MemoryContextSwitchTo(context);
		state->retired = lappend(state->retired, input->values);
		state->retired = lappend(state->retired, input->chunk);
		MemoryContextSwitchTo(old);
		input->values = NULL;
		input->chunk = NULL;
	}
	input->rows = 0;
	input->place = 0;
	if (input->block >= input->run->nblocks)
		return false;
	if (!tess_spill_read_header(input->reader, &header) || header.kind != TESS_SPILL_VALUES)
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("TessSort run lost its block of values %d", input->block));
	input->values = MemoryContextAllocExtended(context, Max(header.len, 8), MCXT_ALLOC_HUGE);
	tess_spill_read_body(input->reader, input->values, header.len);
	if (!tess_spill_read_header(input->reader, &header) || header.kind != TESS_SPILL_COLUMNS)
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("TessSort run lost its block of rows %d", input->block));
	input->chunk = MemoryContextAllocExtended(context, Max(header.len, 8), MCXT_ALLOC_HUGE);
	tess_spill_read_body(input->reader, input->chunk, header.len);
	input->rows = tess_spill_columns_rows(input->chunk);
	if (input->rows != input->run->block_rows[input->block])
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("TessSort run block %d has %u rows, not %u", input->block,
					input->rows, input->run->block_rows[input->block]));
	input->block++;
	return true;
}

static void
input_open(TessSortState *state, MergeInput *input, SortRun *run)
{
	memset(input, 0, sizeof(MergeInput));
	input->run = run;
	input->reader = tess_spill_open(run->set->file, 0, run->partition);
	if (input->reader == NULL && run->nblocks > 0)
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("TessSort lost a run"));
	if (input->reader != NULL)
		(void) input_load(state, input);
}

static void
input_close(MergeInput *input)
{
	if (input->reader != NULL)
		tess_spill_close(input->reader);
	if (input->values != NULL)
		pfree(input->values);
	if (input->chunk != NULL)
		pfree(input->chunk);
	memset(input, 0, sizeof(MergeInput));
}

/* Row place of the input's block into the columns' arrays at out, and its item's words. */
static void
input_take(TessSortState *state, MergeInput *input, uint32 place, int out,
		   Datum *const *values, bool *const *isnull, uint64 *keys)
{
	const bool *typbyvals = state->rows_config.typbyvals;
	Size		capacity = tess_spill_columns_capacity(input->chunk);
	const uint64 *nulls = tess_spill_columns_lane(input->chunk, 0) + place;
	const uint64 *words = tess_spill_columns_word(input->chunk, 0) + place;

	for (int column = 0; column < state->ncolumns; column++)
	{
		uint64		word = words[capacity * column];

		isnull[column][out] = tess_spill_columns_is_null(nulls[capacity * tess_spill_columns_null_lane(column)], column);
		if (isnull[column][out])
			values[column][out] = (Datum) 0;
		else if (typbyvals[column])
			values[column][out] = (Datum) word;
		else
			values[column][out] = PointerGetDatum(input->values + word);
	}
	if (keys != NULL)
		for (int word = 0; word < state->ext_words; word++)
			keys[word] = words[capacity * (state->ncolumns + word)];
}

/* Column `column` of the input's row place, as input_take reads it. */
static Datum
input_value(TessSortState *state, MergeInput *input, int column, bool *isnull)
{
	Size		capacity = tess_spill_columns_capacity(input->chunk);
	uint64		nulls = tess_spill_columns_lane(input->chunk, 0)[capacity * tess_spill_columns_null_lane(column) +
																input->place];
	uint64		word = tess_spill_columns_word(input->chunk, 0)[capacity * column + input->place];

	*isnull = tess_spill_columns_is_null(nulls, column);
	if (*isnull)
		return (Datum) 0;
	if (state->rows_config.typbyvals[column])
		return (Datum) word;
	return PointerGetDatum(input->values + word);
}

/*
 * The order of two inputs' next rows for a merge of a generic key: their
 * items' words, then the comparisons of the keys from the first generic
 * one on; the binary heap keeps the greatest first, so the result is
 * reversed.
 */
static int
compare_inputs(bh_node_type a, bh_node_type b, void *arg)
{
	TessSortState *state = arg;
	MergeInput *left = &state->merging[DatumGetInt32(a)];
	MergeInput *right = &state->merging[DatumGetInt32(b)];
	Size		left_capacity = tess_spill_columns_capacity(left->chunk);
	Size		right_capacity = tess_spill_columns_capacity(right->chunk);
	const uint64 *left_words = tess_spill_columns_word(left->chunk, state->ncolumns);
	const uint64 *right_words = tess_spill_columns_word(right->chunk, state->ncolumns);

	for (int word = 0; word < state->ext_words; word++)
	{
		uint64		x = left_words[left_capacity * word + left->place];
		uint64		y = right_words[right_capacity * word + right->place];

		if (x != y)
			return x < y ? 1 : -1;
	}
	for (int key = state->generic; key < state->nkeys; key++)
	{
		int			column = state->key_columns[key];
		bool		left_null;
		bool		right_null;
		Datum		x = input_value(state, left, column, &left_null);
		Datum		y = input_value(state, right, column, &right_null);
		int			result = ApplySortComparator(x, left_null, y, right_null,
												 &state->ssup[key]);

		if (result != 0)
			return -result;
	}
	return 0;
}

/*
 * merge_rows for a generic key: the kernels' merge compares words only,
 * so the inputs' next rows are kept in a binary heap by compare_inputs,
 * which is made anew at every call.
 */
static int
merge_rows_generic(TessSortState *state, MergeInput *inputs, int ninputs, int max,
				   Datum *const *values, bool *const *isnull,
				   uint64 (*keys)[TESS_SORT_MAX_ITEM_WORDS])
{
	binaryheap *heap;
	int			taken = 0;

	if (state->merge_heap == NULL || state->merge_heap->bh_space < ninputs)
	{
		MemoryContext old = MemoryContextSwitchTo(state->css.ss.ps.state->es_query_cxt);

		if (state->merge_heap != NULL)
			binaryheap_free(state->merge_heap);
		state->merge_heap = binaryheap_allocate(ninputs, compare_inputs, state);
		MemoryContextSwitchTo(old);
	}
	heap = state->merge_heap;
	binaryheap_reset(heap);
	state->merging = inputs;
	for (int input = 0; input < ninputs; input++)
	{
		MergeInput *in = &inputs[input];

		if (in->place == in->rows && in->reader != NULL)
			(void) input_load(state, in);
		if (in->place < in->rows)
			binaryheap_add_unordered(heap, Int32GetDatum(input));
	}
	binaryheap_build(heap);
	while (taken < max && !binaryheap_empty(heap))
	{
		int			input = DatumGetInt32(binaryheap_first(heap));
		MergeInput *in = &inputs[input];

		input_take(state, in, in->place++, taken, values, isnull,
				   keys == NULL ? NULL : keys[taken]);
		taken++;
		/* A block given out whole: the next one of the run, if any. */
		if (in->place == in->rows &&
			(in->reader == NULL || !input_load(state, in)))
			(void) binaryheap_remove_first(heap);
		else
			binaryheap_replace_first(heap, Int32GetDatum(input));
	}
	return taken;
}

/*
 * Up to max rows in order from the inputs into the columns' arrays, and
 * their items' words when keys is not NULL; 0 once every input is done.
 */
static int
merge_rows(TessSortState *state, MergeInput *inputs, int ninputs, uint32 *tree, int max,
		   Datum *const *values, bool *const *isnull,
		   uint64 (*keys)[TESS_SORT_MAX_ITEM_WORDS])
{
	const uint64 *lanes[TESS_SORT_MAX_MERGE_RUNS * TESS_SORT_MAX_ITEM_WORDS];
	uint32		left[TESS_SORT_MAX_MERGE_RUNS];
	bool		more[TESS_SORT_MAX_MERGE_RUNS];
	uint32		order[SORT_ROWS];
	int			taken = 0;

	free_retired(state);
	if (state->generic >= 0)
		return merge_rows_generic(state, inputs, ninputs, max, values, isnull, keys);
	while (taken < max)
	{
		TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
		int			count;
		int			refill;

		for (int input = 0; input < ninputs; input++)
		{
			MergeInput *in = &inputs[input];

			/* A block given out whole: the next one of the run. */
			if (in->place == in->rows && in->reader != NULL)
				(void) input_load(state, in);
			left[input] = in->rows - in->place;
			more[input] = in->reader != NULL && in->block < in->run->nblocks;
			for (int word = 0; word < state->ext_words; word++)
				lanes[input * state->ext_words + word] = left[input] == 0 ? NULL :
					tess_spill_columns_word(in->chunk, state->ncolumns + word) + in->place;
		}
		tess_status_check(state->kernels->sort_merge(ninputs, state->ext_words, lanes, left, more,
													 tree, order, max - taken, &count, &refill,
													 &status),
						  &status);
		for (int row = 0; row < count; row++)
		{
			MergeInput *in = &inputs[order[row]];

			input_take(state, in, in->place++, taken + row, values, isnull,
					   keys == NULL ? NULL : keys[taken + row]);
		}
		taken += count;
		if (count == 0 && refill < 0)
			break;
	}
	return taken;
}

/* Merge the runs from `first`, `count` of them, into one run, and free them. */
static void
merge_pass(TessSortState *state, int first, int count, RunWriter *writer)
{
	MergeInput *inputs = palloc0_array(MergeInput, count);
	Datum	  **values = state->values;
	bool	  **nulls = state->isnull;
	uint64		keys[SORT_ROWS][TESS_SORT_MAX_ITEM_WORDS];
	const uint64 *pointers[SORT_ROWS];
	uint32		tree[TESS_SORT_MERGE_STATE_WORDS] = {0};

	for (int row = 0; row < SORT_ROWS; row++)
		pointers[row] = keys[row];
	for (int input = 0; input < count; input++)
		input_open(state, &inputs[input], state->runs[first + input]);
	for (;;)
	{
		int			n = merge_rows(state, inputs, count, tree, SORT_ROWS, values, nulls, keys);

		if (n == 0)
			break;
		writer_add(state, writer, n, values, nulls, pointers);
		CHECK_FOR_INTERRUPTS();
	}
	free_retired(state);
	for (int input = 0; input < count; input++)
	{
		input_close(&inputs[input]);
		run_free(state->runs[first + input]);
	}
	pfree(inputs);
}

/*
 * After the input: the runs merge, fan_in at a time, into longer runs
 * until one merge takes them all; for a scan backward, into one run, read
 * by blocks. Then the last merge's inputs open.
 */
void
sort_merge_runs(TessSortState *state)
{
	bool		one = (state->eflags & EXEC_FLAG_BACKWARD) != 0;

	while (state->nruns > (one ? 1 : state->fan_in))
	{
		int			nold = state->nruns;
		SortRun   **old = palloc_array(SortRun *, nold);
		int			done = 0;

		memcpy(old, state->runs, sizeof(SortRun *) * nold);
		state->nruns = 0;
		while (done < nold)
		{
			int			count = Min(state->fan_in, nold - done);
			RunWriter	writer;

			/* A run left alone goes on as it is. */
			if (count == 1)
			{
				state->runs[state->nruns++] = old[done++];
				continue;
			}
			writer_start(state, &writer, run_create(state));
			/* merge_pass reads its runs from state->runs: put them there for it. */
			{
				SortRun   **saved = state->runs;
				int			nsaved = state->nruns;

				state->runs = old;
				merge_pass(state, done, count, &writer);
				state->runs = saved;
				state->nruns = nsaved;
			}
			writer_finish(state, &writer);
			done += count;
		}
		pfree(old);
		/* The pass's runs are read by the next. */
		sort_set_finish(state);
		state->merge_passes++;
	}
	state->single = state->nruns == 1;
	if (state->single)
	{
		memset(&state->shown, 0, sizeof(MergeInput));
		state->shown.run = state->runs[0];
		state->shown.reader = tess_spill_open(state->runs[0]->set->file, 0,
											  state->runs[0]->partition);
		state->shown.block = -1;
		return;
	}
	state->merge_state[0] = 0;
	state->ninputs = state->nruns;
	state->inputs = MemoryContextAllocZero(state->css.ss.ps.state->es_query_cxt,
										   sizeof(MergeInput) * state->ninputs);
	for (int input = 0; input < state->ninputs; input++)
		input_open(state, &state->inputs[input], state->runs[input]);
}

/* The last merge from the first row again, for a rescan. */
void
sort_restart_merge(TessSortState *state)
{
	free_retired(state);
	if (state->single)
	{
		if (state->shown.values != NULL)
			pfree(state->shown.values);
		if (state->shown.chunk != NULL)
			pfree(state->shown.chunk);
		state->shown.values = NULL;
		state->shown.chunk = NULL;
		state->shown.block = -1;
		return;
	}
	state->merge_state[0] = 0;
	for (int input = 0; input < state->ninputs; input++)
	{
		input_close(&state->inputs[input]);
		input_open(state, &state->inputs[input], state->runs[input]);
	}
}

/* Free every run and input of an external sort, for the end or a new read. */
void
sort_free_external(TessSortState *state)
{
	/* A set cut short by an error or a new read is finished, to go with its runs. */
	sort_set_finish(state);
	free_retired(state);
	for (int input = 0; input < state->ninputs; input++)
		input_close(&state->inputs[input]);
	if (state->inputs != NULL)
		pfree(state->inputs);
	state->inputs = NULL;
	state->ninputs = 0;
	if (state->single)
	{
		if (state->shown.reader != NULL)
			tess_spill_close(state->shown.reader);
		if (state->shown.values != NULL)
			pfree(state->shown.values);
		if (state->shown.chunk != NULL)
			pfree(state->shown.chunk);
		memset(&state->shown, 0, sizeof(MergeInput));
	}
	for (int run = 0; run < state->nruns; run++)
		run_free(state->runs[run]);
	state->nruns = 0;
	state->external = false;
	state->single = false;
	state->merge_passes = 0;
	state->runs_written = 0;
	state->disk_bytes = 0;
}

/* The single run's block holding row place, in memory. */
void
sort_show_block_of(TessSortState *state, uint64 place)
{
	SortRun    *run = state->shown.run;
	int			low = 0;
	int			high = run->nblocks - 1;
	TessSpillHeader header;
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;

	while (low < high)
	{
		int			middle = (low + high + 1) / 2;

		if (run->block_first[middle] <= place)
			low = middle;
		else
			high = middle - 1;
	}
	if (state->shown.block == low)
		return;
	/* The block of the rows shown so far goes with the next rows. */
	if (state->shown.values != NULL)
	{
		MemoryContext old = MemoryContextSwitchTo(context);

		state->retired = lappend(state->retired, state->shown.values);
		state->retired = lappend(state->retired, state->shown.chunk);
		MemoryContextSwitchTo(old);
	}
	tess_spill_seek(state->shown.reader, run->positions[low]);
	if (!tess_spill_read_header(state->shown.reader, &header) || header.kind != TESS_SPILL_VALUES)
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("TessSort run lost its block of values %d", low));
	state->shown.values = MemoryContextAllocExtended(context, Max(header.len, 8), MCXT_ALLOC_HUGE);
	tess_spill_read_body(state->shown.reader, state->shown.values, header.len);
	if (!tess_spill_read_header(state->shown.reader, &header) || header.kind != TESS_SPILL_COLUMNS)
		ereport(ERROR,
				errcode(ERRCODE_DATA_CORRUPTED),
				errmsg("TessSort run lost its block of rows %d", low));
	state->shown.chunk = MemoryContextAllocExtended(context, Max(header.len, 8), MCXT_ALLOC_HUGE);
	tess_spill_read_body(state->shown.reader, state->shown.chunk, header.len);
	state->shown.rows = tess_spill_columns_rows(state->shown.chunk);
	state->shown.block = low;
}

/*
 * The external sort's rows from place start into the batch: from the
 * single run's block that holds it, the rows of that block from there; or
 * the next rows of the last merge. Returns the rows, 0 at the end.
 */
int
sort_external_window(TessSortState *state, uint64 start, bool backward)
{
	int			n;

	if (!state->single)
		n = merge_rows(state, state->inputs, state->ninputs, state->merge_state, SORT_ROWS,
					   state->values, state->isnull, NULL);
	else
	{
		SortRun    *run = state->shown.run;
		uint64		first;
		uint64		end;

		free_retired(state);
		if (start >= run->rows)
			return 0;
		sort_show_block_of(state, start);
		first = run->block_first[state->shown.block];
		end = first + state->shown.rows;
		n = (int) Min((uint64) SORT_ROWS, end - start);
		(void) backward;
		for (int row = 0; row < n; row++)
			input_take(state, &state->shown, (uint32) (start - first + row), row,
					   state->values, state->isnull, NULL);
	}
	state->start = start;
	state->window_bits[0] = n == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << n) - 1;
	state->batch.rows.nrows = n;
	state->batch.rows.bits = state->window_bits;
	memset(state->gathered, true, sizeof(bool) * state->ncolumns);
	return n;
}

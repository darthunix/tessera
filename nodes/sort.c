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

/*
 * TessSort stands in for the core's Sort under ORDER BY: it reads every
 * batch of its batch child into records of the table format (TessRows),
 * each with the sort keys in its slots and the output columns in its
 * payload, sorts the records by their keys with the kernels
 * (tessera/sort.h) and returns them in order, in batches whose columns are
 * gathered from the records when a parent asks for them, or row by row,
 * forward and backward, to a row-wise parent. Keys are values of the
 * output: the kernels order words (integers, dates, times, booleans); a
 * key of another type orders by its type's comparison (sort support), its
 * word its abbreviated key, when it has one, and the rows the words leave
 * equal by the comparison in C (see "Other types"). Rows past work_mem are sorted into runs on disk
 * and merged (sort_external.c); under a bound the node keeps a heap of
 * the best rows (sort_topn.c). See docs/nodes.md.
 */


static const CustomExecMethods sort_exec_methods;
static void reread_child(TessSortState *state);
/* The rows' by-reference values are copies: gathered, they stay valid. */
static void
sort_get_column(TessBatch *batch, int column, const TessRowMask *rows,
				TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessSortState *state = (TessSortState *) batch->private_data;
	TessRowMask window = {batch->rows.nrows, state->window_bits};

	if (column < 0 || column >= state->ncolumns)
		elog(ERROR, "TessSort has no column %d", column);
	/*
	 * The whole batch at once, at most SORT_ROWS rows; and every column
	 * with the first when the parent read more than one of the batch
	 * before, as a row-wise parent reads them all: a record is located once
	 * for all of them. A parent reading one column has it alone.
	 */
	if (!state->gathered[column])
	{
		if (state->columns_read_before > 1)
		{
			tess_rows_gather_columns(state->rows, &state->refs[state->start], &window,
									 state->values, state->isnull);
			memset(state->gathered, true, sizeof(bool) * state->ncolumns);
			state->columns_read = state->ncolumns;
		}
		else
		{
			tess_rows_gather(state->rows, column, &state->refs[state->start],
							 &window, state->values[column], state->isnull[column]);
			state->gathered[column] = true;
			state->columns_read++;
		}
	}
	result->values = state->values[column];
	result->isnull = state->isnull[column];
	result->nrows = batch->rows.nrows;
}

static const TessBatchOps sort_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = sort_get_column,
};

/*
 * The order of a type's abbreviated keys, from sort support prepared with
 * abbreviate: a comparison of unsigned or signed integers, or numeric's,
 * which is a signed integer's reversed; NONE without one the node takes.
 * The comparators carry master's names (since 2026-08); PostgreSQL 15 to
 * 18 call them ssup_datum_unsigned_cmp, ssup_datum_signed_cmp and
 * ssup_datum_int32_cmp, without a uint32 one, and older cores have none.
 */
static SortAbbrev
abbrev_order_of(SortSupport abbrev, Oid type)
{
	if (abbrev->abbrev_converter == NULL)
		return SORT_ABBREV_NONE;
	if (abbrev->comparator == ssup_datum_uint64_cmp)
		return SORT_ABBREV_UNSIGNED;
	if (abbrev->comparator == ssup_datum_int64_cmp)
		return SORT_ABBREV_SIGNED;
	if (abbrev->comparator == ssup_datum_uint32_cmp)
		return SORT_ABBREV_UINT32;
	if (abbrev->comparator == ssup_datum_int32_cmp)
		return SORT_ABBREV_INT32;
	if (type == NUMERICOID &&
		abbrev->comparator(Int64GetDatum(0), Int64GetDatum(1), abbrev) > 0 &&
		abbrev->comparator(Int64GetDatum(-1), Int64GetDatum(0), abbrev) > 0)
		return SORT_ABBREV_REVERSED;
	return SORT_ABBREV_NONE;
}

/* Sort support for a key's comparison: its ordering operator, collation and place of NULLs. */
void
tess_sort_support(SortSupport ssup, Oid sortop, Oid collation, bool nulls_first)
{
	memset(ssup, 0, sizeof(SortSupportData));
	ssup->ssup_cxt = CurrentMemoryContext;
	ssup->ssup_collation = collation;
	ssup->ssup_nulls_first = nulls_first;
	ssup->abbreviate = false;
	PrepareSortSupportFromOrderingOp(sortop, ssup);
}

/* A key's abbreviated keys, when its type has ones the node takes. */
void
tess_sort_abbrev_init(TessSortAbbrev *abbrev, Oid sortop, Oid collation,
					  bool nulls_first, Oid type)
{
	SortSupport ssup = &abbrev->ssup;

	memset(ssup, 0, sizeof(SortSupportData));
	ssup->ssup_cxt = CurrentMemoryContext;
	ssup->ssup_collation = collation;
	ssup->ssup_nulls_first = nulls_first;
	ssup->abbreviate = true;
	PrepareSortSupportFromOrderingOp(sortop, ssup);
	abbrev->order = abbrev_order_of(ssup, type);
}

bool
tess_sort_abbreviates(const TessSortAbbrev *abbrev)
{
	return abbrev->order != SORT_ABBREV_NONE;
}

/*
 * The word of a value that is not NULL: its abbreviated key as a signed
 * integer in its order, or 0 without one. The converter may allocate in
 * the current context.
 */
int64
tess_sort_abbrev_word(TessSortAbbrev *abbrev, Datum value)
{
	Datum		abbreviated;

	if (abbrev->order == SORT_ABBREV_NONE)
		return 0;
	abbreviated = abbrev->ssup.abbrev_converter(value, &abbrev->ssup);
	switch ((SortAbbrev) abbrev->order)
	{
		case SORT_ABBREV_UNSIGNED:
			return (int64) (DatumGetUInt64(abbreviated) ^ (UINT64CONST(1) << 63));
		case SORT_ABBREV_SIGNED:
			return DatumGetInt64(abbreviated);
		case SORT_ABBREV_REVERSED:
			return ~DatumGetInt64(abbreviated);
		case SORT_ABBREV_UINT32:
			return (int64) DatumGetUInt32(abbreviated);
		case SORT_ABBREV_INT32:
			return (int64) DatumGetInt32(abbreviated);
		case SORT_ABBREV_NONE:
			break;
	}
	return 0;
}

/*
 * Other types: the comparison of every key from the first generic one on,
 * by its ordering operator, collation and place of NULLs, and the
 * abbreviated key of the first, when the type has one whose order the
 * node can make a word's: a comparison of unsigned or signed integers, or
 * numeric's, which is a signed integer's reversed.
 */
static void
generic_begin(TessSortState *state, TupleDesc desc, List *sortops, List *collations)
{
	int			first = state->generic;
	Oid			type = TupleDescAttr(desc, state->key_columns[first])->atttypid;

	state->ssup = palloc0_array(SortSupportData, state->nkeys);
	for (int key = first; key < state->nkeys; key++)
	{
		Oid			sortop = (Oid) list_nth_int(sortops, key);

		if (!OidIsValid(sortop))
			elog(ERROR, "TessSort received a foreign plan");
		tess_sort_support(&state->ssup[key], sortop, (Oid) list_nth_int(collations, key),
						  (state->keys[key].flags & TESS_SORT_NULLS_FIRST) != 0);
	}
	tess_sort_abbrev_init(&state->abbrev, (Oid) list_nth_int(sortops, first),
						  state->ssup[first].ssup_collation,
						  state->ssup[first].ssup_nulls_first, type);
	state->abbrev_taken = (SortAbbrev) state->abbrev.order;
	state->abbrev_context = AllocSetContextCreate(CurrentMemoryContext,
												  "TessSort abbreviated keys",
												  ALLOCSET_DEFAULT_SIZES);
	/* As tuplesort: the first test at ten rows. */
	state->abbrev_next = 10;
}

/*
 * The first generic key's words of the selected rows of its column: its
 * abbreviated keys as signed integers in their order, or 0 without one;
 * NULL kept.
 */
void
sort_abbreviate_column(TessSortState *state, const TessDatumColumn *column,
				  const TessRowMask *rows)
{
	MemoryContext old;
	int			row = -1;

	if (column->nrows > state->abbrev_capacity)
	{
		MemoryContext context = state->css.ss.ps.state->es_query_cxt;

		if (state->abbrev_values != NULL)
		{
			pfree(state->abbrev_values);
			pfree(state->abbrev_isnull);
		}
		state->abbrev_capacity = Max(column->nrows, SORT_ROWS);
		state->abbrev_values = MemoryContextAllocZero(context,
													  sizeof(Datum) * state->abbrev_capacity);
		state->abbrev_isnull = MemoryContextAllocZero(context,
													  sizeof(bool) * state->abbrev_capacity);
	}
	MemoryContextReset(state->abbrev_context);
	old = MemoryContextSwitchTo(state->abbrev_context);
	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		state->abbrev_isnull[row] = column->isnull[row];
		state->abbrev_values[row] = Int64GetDatum(column->isnull[row] ? 0 :
												  tess_sort_abbrev_word(&state->abbrev,
																		column->values[row]));
		state->abbrev_rows += !column->isnull[row];
	}
	MemoryContextSwitchTo(old);
	state->abbrev_column = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
	state->abbrev_column.values = state->abbrev_values;
	state->abbrev_column.isnull = state->abbrev_isnull;
	state->abbrev_column.nrows = column->nrows;
}

/*
 * A row of a group: the first generic key's value in place, as the core's
 * SortTuple keeps it, the other keys' values at its row of tie_values.
 */
typedef struct TieRow
{
	Datum		value;
	uint32		ref;
	/* The row in the group, and the value's NULL flag in the top bit. */
	uint32		row;
} TieRow;

#define TIE_NULL ((uint32) 1 << 31)

/* The comparison of two rows of a group by the keys from the first generic one on. */
static inline int
compare_ties(const TieRow *left, const TieRow *right, TessSortState *state)
{
	uint64		n = state->tie_capacity;
	int			result = ApplySortComparator(left->value, (left->row & TIE_NULL) != 0,
											 right->value, (right->row & TIE_NULL) != 0,
											 &state->ssup[state->generic]);

	for (int key = state->generic + 1; result == 0 && key < state->nkeys; key++)
	{
		uint64		base = (uint64) (key - state->generic - 1) * n;

		uint32		x = left->row & ~TIE_NULL;
		uint32		y = right->row & ~TIE_NULL;

		result = ApplySortComparator(state->tie_values[base + x], state->tie_isnull[base + x],
									 state->tie_values[base + y], state->tie_isnull[base + y],
									 &state->ssup[key]);
	}
	return result;
}

/*
 * The core's sort template, as its own sorts use it: the rows swapped as
 * TieRows and compared inline, not through qsort_arg's bytes and pointer.
 */
#define ST_SORT sort_tie_rows
#define ST_ELEMENT_TYPE TieRow
#define ST_COMPARE(a, b, state) compare_ties(a, b, state)
#define ST_COMPARE_ARG_TYPE TessSortState
#define ST_CHECK_FOR_INTERRUPTS
#define ST_SCOPE static
#define ST_DEFINE
#include "lib/sort_template.h"

/* Order the n records refs of one group of equal words by the comparisons. */
static void
sort_group(TessSortState *state, uint32 *refs, uint64 n)
{
	int			nafter = state->nkeys - state->generic - 1;
	TieRow	   *ties;

	if (n >= TIE_NULL)
		ereport(ERROR,
				errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				errmsg("TessSort cannot order " UINT64_FORMAT " rows of equal keys", n));
	if (n > state->tie_capacity)
	{
		MemoryContext context = state->css.ss.ps.state->es_query_cxt;
		uint64		capacity = Max(n, (uint64) 1024);

		if (state->tie_rows != NULL)
			pfree(state->tie_rows);
		if (state->tie_values != NULL)
		{
			pfree(state->tie_values);
			pfree(state->tie_isnull);
		}
		state->tie_rows = MemoryContextAllocExtended(context, mul_size(sizeof(TieRow), capacity),
													 MCXT_ALLOC_HUGE);
		/* Keys after the first generic one only. */
		if (nafter > 0)
		{
			state->tie_values = MemoryContextAllocExtended(context,
														   mul_size(sizeof(Datum) * nafter, capacity),
														   MCXT_ALLOC_HUGE);
			state->tie_isnull = MemoryContextAllocExtended(context,
														   mul_size(sizeof(bool) * nafter, capacity),
														   MCXT_ALLOC_HUGE);
		}
		state->tie_capacity = capacity;
	}
	ties = state->tie_rows;
	for (uint64 first = 0; first < n; first += SORT_ROWS)
	{
		int			count = (int) Min((uint64) SORT_ROWS, n - first);
		uint64		bits[1] = {count == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << count) - 1};
		TessRowMask mask = {count, bits};
		Datum		values[SORT_ROWS];
		bool		isnull[SORT_ROWS];

		tess_rows_gather(state->rows, state->key_columns[state->generic], &refs[first], &mask,
						 values, isnull);
		for (int row = 0; row < count; row++)
		{
			ties[first + row].value = values[row];
			ties[first + row].ref = refs[first + row];
			ties[first + row].row = (uint32) (first + row) | (isnull[row] ? TIE_NULL : 0);
		}
		for (int key = state->generic + 1; key < state->nkeys; key++)
		{
			uint64		base = (uint64) (key - state->generic - 1) * state->tie_capacity + first;

			tess_rows_gather(state->rows, state->key_columns[key], &refs[first], &mask,
							 &state->tie_values[base], &state->tie_isnull[base]);
		}
	}
	sort_tie_rows(ties, n, state);
	for (uint64 row = 0; row < n; row++)
		refs[row] = ties[row].ref;
}

/* The rows of groups, freed once the rows are sorted. */
static void
free_ties(TessSortState *state)
{
	if (state->tie_rows != NULL)
		pfree(state->tie_rows);
	if (state->tie_values != NULL)
	{
		pfree(state->tie_values);
		pfree(state->tie_isnull);
	}
	state->tie_rows = NULL;
	state->tie_values = NULL;
	state->tie_isnull = NULL;
	state->tie_capacity = 0;
}

/*
 * After the kernels sorted count items of words words by the keys up to
 * the first generic one: the records of each run of items whose keys'
 * words are equal (all but the reference's 32 bits) ordered by the
 * comparisons of the keys from that one on. The runs' starts are marked
 * first, so that a caller done with the items frees them (*items NULL)
 * before the rows of the runs take their memory.
 */
void
sort_ties(TessSortState *state, uint64 **items, int words, uint32 *refs, uint64 count,
		  bool free_items)
{
	uint64	   *starts = palloc0_array(uint64, (count + 64) / 64);
	const uint64 *item = *items;
	uint64		first = 0;

	for (uint64 place = 1; place < count; place++)
	{
		const uint64 *next = item + words;

		if ((words > 1 && memcmp(item, next, sizeof(uint64) * (words - 1)) != 0) ||
			((item[words - 1] ^ next[words - 1]) >> 32) != 0)
			starts[place / 64] |= UINT64CONST(1) << (place % 64);
		item = next;
	}
	starts[count / 64] |= UINT64CONST(1) << (count % 64);
	if (free_items)
	{
		pfree(*items);
		*items = NULL;
	}
	while (first < count)
	{
		uint64		end = first + 1;

		while (((starts[end / 64] >> (end % 64)) & 1) == 0)
			end++;
		if (end - first > 1)
			sort_group(state, &refs[first], end - first);
		first = end;
		CHECK_FOR_INTERRUPTS();
	}
	pfree(starts);
}

static void
sort_begin(CustomScanState *css, EState *estate, int eflags)
{
	TessSortState *state = (TessSortState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
	TessRowsConfig rows = TESS_STRUCT_INITIALIZER(TessRowsConfig);
	TupleTableSlot *result = css->ss.ps.ps_ResultTupleSlot;
	TupleDesc	desc = result->tts_tupleDescriptor;
	TessTableKeyKind kinds[TESS_TABLE_MAX_KEYS];
	Bitmapset  *projection = NULL;
	TessPlanReader *reader;
	List	   *columns;
	List	   *keys;
	List	   *key_kinds;
	List	   *key_flags;
	List	   *key_sortops;
	List	   *key_collations;
	int16	   *typlens;
	bool	   *typbyvals;

	/* The planner puts Material above a batch subtree for mark/restore. */
	if (eflags & EXEC_FLAG_MARK)
		elog(ERROR, "TessSort does not support mark/restore");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_sort_node || info.nchildren != 1 ||
		info.child_names[0] == NULL)
		elog(ERROR, "TessSort received a foreign plan");
	reader = tess_plan_reader_create((List *) info.node_data, TESS_SORT_DATA,
									 TESS_SORT_DATA_VERSION);
	columns = tess_plan_read_int_list(reader, "columns");
	keys = tess_plan_read_int_list(reader, "keys");
	key_kinds = tess_plan_read_int_list(reader, "kinds");
	key_flags = tess_plan_read_int_list(reader, "flags");
	key_sortops = tess_plan_read_int_list(reader, "sortops");
	key_collations = tess_plan_read_int_list(reader, "collations");
	tess_plan_reader_finish(reader);
	state->ncolumns = list_length(columns);
	state->nkeys = list_length(keys);
	if (state->ncolumns != desc->natts || state->ncolumns == 0 ||
		state->ncolumns > TESS_ROWS_MAX_COLUMNS || state->nkeys == 0 ||
		state->nkeys > TESS_TABLE_MAX_KEYS ||
		list_length(key_kinds) != state->nkeys ||
		list_length(key_flags) != state->nkeys ||
		list_length(key_sortops) != state->nkeys ||
		list_length(key_collations) != state->nkeys)
		elog(ERROR, "TessSort received a foreign plan");
	state->kernels = tess_runtime_kernels();
	if (state->kernels == NULL)
		elog(ERROR, "TessSort needs the kernels module");

	/* The child is read forward once, as the core's sort reads its own. */
	state->child = ExecInitNode(linitial(cscan->custom_plans), estate,
								eflags & ~(EXEC_FLAG_REWIND | EXEC_FLAG_BACKWARD |
										   EXEC_FLAG_MARK));
	css->custom_ps = list_make1(state->child);
	state->input = tess_input_create(estate->es_query_cxt, state->child);
	state->child_columns = palloc_array(int, state->ncolumns);
	typlens = palloc_array(int16, state->ncolumns);
	typbyvals = palloc_array(bool, state->ncolumns);
	foreach_int(column, columns)
	{
		int			index = foreach_current_index(column);
		Form_pg_attribute attribute = TupleDescAttr(desc, index);

		state->child_columns[index] = column;
		projection = bms_add_member(projection, column);
		typlens[index] = attribute->attlen;
		typbyvals[index] = attribute->attbyval;
	}
	state->key_columns = palloc_array(int, state->nkeys);
	state->generic = -1;
	foreach_int(key, keys)
	{
		int			index = foreach_current_index(key);
		int			kind = list_nth_int(key_kinds, index);

		if (key < 0 || key >= state->ncolumns)
			elog(ERROR, "TessSort received a foreign plan");
		state->key_columns[index] = key;
		/* A generic key's word is its abbreviated key, an int8. */
		if (kind == TESS_SORT_KIND_GENERIC && state->generic < 0)
			state->generic = index;
		kinds[index] = kind == TESS_SORT_KIND_GENERIC ? TESS_TABLE_KEY_INT8 :
			(TessTableKeyKind) kind;
		state->keys[index].kind = kinds[index];
		state->keys[index].flags = (uint32) list_nth_int(key_flags, index);
	}
	state->nkernel = state->generic < 0 ? state->nkeys : state->generic + 1;
	if (state->generic >= 0)
		generic_begin(state, desc, key_sortops, key_collations);
	/* Whole batches: every column of the rows kept. */
	request.projection_columns = projection;
	request.output_mode = TESS_OUTPUT_BATCH;
	tess_input_set_request(state->input, &request);

	memcpy(state->kinds, kinds, sizeof(TessTableKeyKind) * state->nkernel);
	rows.parent_context = estate->es_query_cxt;
	rows.kernels = state->kernels;
	rows.nkeys = state->nkernel;
	rows.kinds = state->kinds;
	rows.ncolumns = state->ncolumns;
	rows.typlens = typlens;
	rows.typbyvals = typbyvals;
	/* Chunks of an eighth of work_mem, so that runs fill it evenly. */
	rows.chunk_len = Max((Size) work_mem * 1024 / 8, (Size) 8192);
	state->rows_config = rows;
	state->rows = tess_rows_create(&rows);
	state->bound = -1;
	state->used_bound = -1;
	state->columns = palloc0_array(TessDatumColumn, state->ncolumns);
	state->values = palloc_array(Datum *, state->ncolumns);
	state->isnull = palloc_array(bool *, state->ncolumns);
	state->gathered = palloc0_array(bool, state->ncolumns);
	for (int column = 0; column < state->ncolumns; column++)
	{
		state->values[column] = palloc0_array(Datum, SORT_ROWS);
		state->isnull[column] = palloc0_array(bool, SORT_ROWS);
	}
	state->batch.abi_version = TESS_BATCH_ABI_VERSION;
	state->batch.struct_size = sizeof(TessBatch);
	state->batch.table_oid = InvalidOid;
	state->batch.ops = &sort_batch_ops;
	state->batch.private_data = state;
	state->current = -1;
	state->eflags = eflags;
	sort_plan_external(state);
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   result, &info.layout);
}

/* The records, what their items and references took at the sort, and the peak. */
void
sort_note_memory(TessSortState *state, Size extra)
{
	Size		memory = add_size(tess_rows_memory(state->rows), extra);

	state->counters.memory = Max(state->counters.memory, memory);
}

/* Output column `column` of the batch, for its selected rows. */
void
sort_batch_column(TessSortState *state, TessBatch *batch, int column)
{
	TessDatumColumn *values = &state->columns[column];

	*values = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
	batch->ops->get_datum_column(batch, state->child_columns[column],
								 &batch->rows, TESS_COLUMN_FOR_PROJECTION,
								 values);
	if (values->values == NULL || values->isnull == NULL ||
		values->nrows != batch->rows.nrows)
		elog(ERROR, "TessSort child returned an invalid column");
}

/*
 * The key columns of the batch the kernels order, as the table takes
 * them: a generic key's, its abbreviated keys.
 */
void
sort_batch_keys(TessSortState *state, TessBatch *batch)
{
	for (int key = 0; key < state->nkernel; key++)
	{
		sort_batch_column(state, batch, state->key_columns[key]);
		state->table_keys[key].kind = state->keys[key].kind;
		state->table_keys[key].column = &state->columns[state->key_columns[key]];
		state->table_keys[key].prepared = NULL;
		if (key == state->generic)
		{
			sort_abbreviate_column(state, state->table_keys[key].column, &batch->rows);
			state->table_keys[key].column = &state->abbrev_column;
		}
	}
}

/* Whether an output column is a key's the kernels order, fetched with the keys. */
bool
sort_is_key_column(TessSortState *state, int column)
{
	for (int key = 0; key < state->nkernel; key++)
		if (state->key_columns[key] == column)
			return true;
	return false;
}

/* The selected rows of the batch into records, their references into batch_refs. */
void
sort_append_rows(TessSortState *state, TessBatch *batch)
{
	int			nrows = batch->rows.nrows;

	if (nrows > state->capacity)
	{
		state->capacity = Max(nrows, SORT_ROWS);
		state->batch_refs = state->batch_refs == NULL ?
			MemoryContextAlloc(state->css.ss.ps.state->es_query_cxt,
							   sizeof(uint32) * state->capacity) :
			repalloc(state->batch_refs, sizeof(uint32) * state->capacity);
	}
	tess_rows_append(state->rows, state->table_keys, state->columns,
					 &batch->rows, state->batch_refs);
}

/*
 * Give up the abbreviated keys, as tuplesort does, when the type's abort
 * test finds them telling too few rows apart, at 10, 20, 40, ... rows
 * abbreviated: only while every row is in memory, since a run on disk
 * keeps its items. The keys the records hold become 0, and the next rows'
 * are 0 without a conversion, so the comparisons of the group of equal
 * words order the rows, as the core's full comparator does.
 */
static void
consider_abbrev_abort(TessSortState *state)
{
	SortSupport ssup = &state->abbrev.ssup;

	if (state->external || !tess_sort_abbreviates(&state->abbrev) ||
		ssup->abbrev_abort == NULL || state->abbrev_rows < state->abbrev_next)
		return;
	while (state->abbrev_next <= state->abbrev_rows)
		state->abbrev_next *= 2;
	if (!ssup->abbrev_abort((int) Min(state->abbrev_rows, (uint64) INT_MAX), ssup))
		return;
	state->abbrev.order = SORT_ABBREV_NONE;
	tess_rows_clear_key(state->rows, state->generic);
	state->abbrev_given_up = true;
}

/* The rows of one batch of the child into records. */
static void
append_batch(TessSortState *state, TessBatch *batch)
{
	if (state->generic >= 0)
		consider_abbrev_abort(state);
	sort_batch_keys(state, batch);
	for (int column = 0; column < state->ncolumns; column++)
		if (!sort_is_key_column(state, column))
			sort_batch_column(state, batch, column);
	for (int key = 0; key < state->nkernel; key++)
		if (!state->key_nulls[key])
			state->key_nulls[key] =
				tess_rows_selected_null(&batch->rows,
										state->table_keys[key].column->isnull);
	sort_append_rows(state, batch);
}























/*
 * Whether the rows in memory, what sorting them takes and a chunk more,
 * which the next batch may need, pass work_mem.
 */
static bool
rows_full(TessSortState *state)
{
	uint64		count = tess_rows_count(state->rows);
	Size		bytes = tess_rows_memory(state->rows) +
		(Size) count * (sizeof(uint32) + sizeof(uint64) * state->item_words) +
		state->rows_config.chunk_len;

	return bytes > (Size) work_mem * 1024;
}
















/*
 * Read every batch of the child and sort the records. The child runs
 * forward whatever direction the first fetch has.
 */
static void
sort_rows(TessSortState *state)
{
	EState	   *estate = state->css.ss.ps.state;
	ScanDirection direction = estate->es_direction;
	TessSortKey keys[TESS_TABLE_MAX_KEYS];

	state->topn = sort_choose_topn(state);
	state->used_bound = state->topn ? state->bound : -1;
	if (state->topn)
	{
		/* It fits work_mem (sort_choose_topn): the counts are exact. */
		Size		words = (Size) sort_topn_heap_words(state, (double) state->bound);

		if (state->heap != NULL)
			pfree(state->heap);
		state->heap_capacity = (Size) state->bound;
		state->heap_len = 0;
		/* A generic key's heap has a spare slot, and the keys' values. */
		if (state->generic >= 0)
		{
			Size		slots = (Size) sort_topn_value_slots(state, (double) state->bound);

			if (state->top_values != NULL)
			{
				pfree(state->top_values);
				pfree(state->top_isnull);
			}
			state->top_values = MemoryContextAllocExtended(estate->es_query_cxt,
														   mul_size(slots, sizeof(Datum)),
														   MCXT_ALLOC_HUGE);
			state->top_isnull = MemoryContextAllocExtended(estate->es_query_cxt, slots,
														   MCXT_ALLOC_HUGE);
		}
		state->heap = MemoryContextAllocExtended(estate->es_query_cxt,
												 mul_size(Max(words, 1), sizeof(uint64)),
												 MCXT_ALLOC_HUGE);
	}
	estate->es_direction = ForwardScanDirection;
	/* A bound of no rows reads nothing. */
	while (!(state->topn && state->heap_capacity == 0))
	{
		TessBatch  *batch = tess_input_next(state->input);
		int			rows;

		if (batch == NULL)
			break;
		rows = tess_row_mask_count(&batch->rows);
		state->counters.batches++;
		state->counters.rows += rows;
		if (rows > 0 && state->topn && state->generic >= 0)
			sort_top_batch_generic(state, batch);
		else if (rows > 0 && state->topn)
			sort_top_batch(state, batch);
		else if (rows > 0)
			append_batch(state, batch);
		tess_input_finish(state->input);
		/* Past work_mem: the rows so far go to a run, sorted. */
		if (!state->topn && rows > 0 && rows_full(state))
		{
			state->external = true;
			sort_spill_run(state);
		}
		CHECK_FOR_INTERRUPTS();
	}
	estate->es_direction = direction;
	if (state->external)
	{
		sort_spill_run(state);
		free_ties(state);
		sort_set_finish(state);
		sort_merge_runs(state);
		sort_note_memory(state, (Size) Max(state->ninputs, 1) *
					(state->block_values + sizeof(uint64) * state->block_rows *
					 (tess_spill_columns_null_lanes(sort_run_words(state)) + sort_run_words(state))));
		state->count = 0;
		for (int run = 0; run < state->nruns; run++)
			state->count += state->runs[run]->rows;
		state->sorted = true;
		state->current = -1;
		return;
	}
	if (state->topn)
	{
		TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

		/* The heap's items, sorted, give the best rows in order. */
		state->count = state->heap_len;
		if (state->refs != NULL)
			pfree(state->refs);
		state->refs = MemoryContextAllocExtended(estate->es_query_cxt,
												 mul_size(sizeof(uint32),
														  Max(state->count, 1)),
												 MCXT_ALLOC_HUGE);
		if (state->count > 0)
			tess_status_check(state->kernels->sort(state->heap, (Size) state->count,
												   state->item_words, state->refs, &status),
							  &status);
		/* Items of equal words by the comparisons, as a full sort orders them. */
		if (state->count > 0 && state->generic >= 0)
		{
			uint64	   *items = state->heap;

			sort_ties(state, &items, state->item_words, state->refs, state->count, false);
			free_ties(state);
		}
		sort_note_memory(state, (Size) sort_topn_heap_bytes(state, state->heap_capacity));
		state->sorted = true;
		state->current = -1;
		return;
	}
	state->count = tess_rows_count(state->rows);
	/* A key takes the bit for NULL only when one of its rows held one. */
	for (int key = 0; key < state->nkernel; key++)
	{
		keys[key] = state->keys[key];
		if (state->key_nulls[key])
			keys[key].flags |= TESS_SORT_NULLABLE;
	}
	if (state->refs != NULL)
		pfree(state->refs);
	state->refs = MemoryContextAllocExtended(estate->es_query_cxt,
											 mul_size(sizeof(uint32),
													  Max(state->count, 1)),
											 MCXT_ALLOC_HUGE);
	if (state->count > 0)
	{
		int			words;
		TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

		if (state->kernels->sort_item_words(state->nkernel, keys, &words,
											&status) != TESS_OK)
			tess_status_report(&status);
		sort_note_memory(state, mul_size(state->count,
									sizeof(uint32) + sizeof(uint64) * words));
		if (state->generic >= 0)
		{
			uint64	   *items = tess_rows_sort_items(state->rows, keys, state->refs, &words);

			sort_ties(state, &items, words, state->refs, state->count, true);
			sort_note_memory(state, mul_size(state->count, sizeof(uint32)) +
						state->tie_capacity * sizeof(TieRow));
			free_ties(state);
		}
		else
			tess_rows_sort(state->rows, keys, state->refs);
	}
	else
		sort_note_memory(state, 0);
	state->sorted = true;
	state->current = -1;
}

/* Make the rows from place start the batch, its columns not gathered yet. */
static void
show_window(TessSortState *state, uint64 start)
{
	int			n = (int) Min((uint64) SORT_ROWS, state->count - start);

	state->start = start;
	state->window_bits[0] = n == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << n) - 1;
	state->batch.rows.nrows = n;
	state->batch.rows.bits = state->window_bits;
	memset(state->gathered, 0, sizeof(bool) * state->ncolumns);
	state->columns_read_before = state->columns_read;
	state->columns_read = 0;
}

/* The next batch of rows in order for a batch-aware parent, or NULL. */
static TupleTableSlot *
next_batch(TessSortState *state)
{
	uint64		start = (uint64) (state->current + 1);

	tess_output_release(state->output);
	state->published = false;
	if (start >= state->count)
	{
		state->current = (int64) state->count;
		return NULL;
	}
	if (state->external)
	{
		if (sort_external_window(state, start, false) == 0)
		{
			state->current = (int64) state->count;
			return NULL;
		}
	}
	else
		show_window(state, start);
	state->current = (int64) (start + state->batch.rows.nrows - 1);
	state->published = true;
	return tess_output_publish(state->output, &state->batch);
}

/*
 * The next row in the scan's direction for a row-wise parent: a place
 * outside the batch shown makes the batch the rows from it on, or, going
 * backward, the rows up to it.
 */
static TupleTableSlot *
next_row(TessSortState *state, bool forward)
{
	int64		place = state->current + (forward ? 1 : -1);
	TupleTableSlot *slot;

	if (place < 0)
	{
		state->current = -1;
		return NULL;
	}
	if ((uint64) place >= state->count)
	{
		state->current = (int64) state->count;
		return NULL;
	}
	state->current = place;
	if (state->published && (uint64) place >= state->start &&
		(uint64) place < state->start + state->batch.rows.nrows)
		return tess_output_select(state->output, (int) (place - state->start));
	if (state->published)
		tess_output_finish(state->output);
	tess_output_release(state->output);
	if (state->external)
	{
		uint64		start = (uint64) place;

		/* Backward: the rows of place's block up to it, from the single run. */
		if (!forward)
		{
			if (!state->single)
				elog(ERROR, "TessSort returns rows backward only when planned for it");
			sort_show_block_of(state, (uint64) place);
			start = Max(state->shown.run->block_first[state->shown.block],
						(uint64) Max(place - (SORT_ROWS - 1), 0));
		}
		else if (!state->single && state->published &&
				 (uint64) place != state->start + state->batch.rows.nrows)
			elog(ERROR, "TessSort merges its runs forward only");
		if (sort_external_window(state, start, !forward) == 0)
		{
			state->published = false;
			state->current = (int64) state->count;
			return NULL;
		}
	}
	else
		show_window(state, forward ? (uint64) place :
					(uint64) Max(place - (SORT_ROWS - 1), 0));
	state->published = true;
	slot = tess_output_publish(state->output, &state->batch);
	if ((uint64) place == state->start)
		return slot;
	return tess_output_select(state->output, (int) (place - state->start));
}

static TupleTableSlot *
sort_exec(CustomScanState *css)
{
	TessSortState *state = (TessSortState *) css;
	bool		forward = ScanDirectionIsForward(css->ss.ps.state->es_direction);
	bool		rows = tess_output_request(state->output)->output_mode ==
		TESS_OUTPUT_ROWS;

	/* Rows read for fewer than the parent now needs are read again. */
	if (state->sorted && state->used_bound >= 0 &&
		(state->bound < 0 || state->bound > state->used_bound))
		reread_child(state);
	if (!state->sorted)
		sort_rows(state);
	if (rows)
		return next_row(state, forward);
	if (!forward)
		elog(ERROR, "TessSort returns batches only forward");
	return next_batch(state);
}

static void
sort_end(CustomScanState *css)
{
	TessSortState *state = (TessSortState *) css;

	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	tess_output_end(state->output);
	ExecEndNode(state->child);
	sort_free_external(state);
	tess_rows_free(state->rows);
}

/* Rescan the child and forget the rows, to read and sort them anew. */
static void
reread_child(TessSortState *state)
{
	tess_output_clear(state->output);
	state->published = false;
	state->current = -1;
	ExecReScan(state->child);
	tess_input_rescan(state->input);
	tess_rows_reset(state->rows);
	sort_free_external(state);
	memset(state->key_nulls, 0, sizeof(state->key_nulls));
	state->sorted = false;
	state->count = 0;
	state->heap_len = 0;
}

/*
 * A rescan returns the sorted rows again from the first, unless a
 * parameter of the child changed: then the child is read and sorted anew.
 * A parent may set another bound before it fetches; the execution reads
 * again when the rows kept are too few for it.
 */
static void
sort_rescan(CustomScanState *css)
{
	TessSortState *state = (TessSortState *) css;

	tess_output_clear(state->output);
	state->published = false;
	state->current = -1;
	state->bound = -1;
	if (css->ss.ps.chgParam == NULL && state->sorted)
	{
		/* The runs stay on disk: the last merge starts again. */
		if (state->external)
			sort_restart_merge(state);
		return;
	}
	/* The core passes changed parameters to outer and inner plans only. */
	if (css->ss.ps.chgParam != NULL)
		UpdateChangedParamSet(state->child, css->ss.ps.chgParam);
	if (!state->sorted && state->child->chgParam == NULL)
		return;
	reread_child(state);
}

static void
sort_set_tuple_bound(CustomScanState *css, int64 tuples_needed)
{
	TessSortState *state = (TessSortState *) css;

	state->bound = tuples_needed < 0 ? -1 : tuples_needed;
}

/* This participant's counters. */
static void
sort_counters(TessSortState *state, uint64 *values)
{
	Size		limit = (Size) work_mem * 1024;

	memset(values, 0, sizeof(uint64) * SORT_NCOUNTERS);
	values[SORT_BATCHES] = state->counters.batches;
	values[SORT_INPUT_ROWS] = state->counters.rows;
	values[SORT_MEMORY] = state->counters.memory;
	values[SORT_OVERRUN] = state->counters.memory > limit ? state->counters.memory - limit : 0;
	values[SORT_SORTED] = state->sorted ? 1 : 0;
	values[SORT_EXTERNAL] = state->sorted && state->external ? 1 : 0;
	values[SORT_TOPN] = state->sorted && state->topn ? 1 : 0;
	values[SORT_RUNS] = (uint64) state->runs_written;
	values[SORT_PASSES] = (uint64) state->merge_passes;
	values[SORT_DISK] = state->disk_bytes;
	values[SORT_REBUILT] = state->topn ? state->compactions : 0;
	values[SORT_ABBREV_GIVEN_UP] = state->abbrev_given_up ? 1 : 0;
}

static void
sort_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	TessSortState *state = (TessSortState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	List	   *context = set_deparse_context_plan(es->deparse_cxt,
												   css->ss.ps.plan, ancestors);
	bool		useprefix = es->rtable_size > 1 || es->verbose;
	List	   *keys = NIL;
	const uint64 *totals;
	uint64		own[SORT_NCOUNTERS];

	foreach_ptr(Node, expr, cscan->custom_exprs)
	{
		char	   *key = deparse_expression(expr, context, useprefix, false);
		uint32		flags = state->keys[foreach_current_index(expr)].flags;
		bool		descending = (flags & TESS_SORT_DESCENDING) != 0;
		bool		nulls_first = (flags & TESS_SORT_NULLS_FIRST) != 0;

		int			index = foreach_current_index(expr);
		Oid			collation = state->ssup != NULL && index >= state->generic ?
			state->ssup[index].ssup_collation : InvalidOid;

		/* As the core shows them: what is not the default is spelled out. */
		if (OidIsValid(collation) && collation != DEFAULT_COLLATION_OID)
			key = psprintf("%s COLLATE %s", key,
						   quote_identifier(get_collation_name(collation)));
		if (descending)
			key = psprintf("%s DESC", key);
		if (nulls_first != descending)
			key = psprintf("%s NULLS %s", key, nulls_first ? "FIRST" : "LAST");
		keys = lappend(keys, key);
	}
	ExplainPropertyList("Sort Key", keys, es);
	if (!es->analyze)
		return;
	sort_counters(state, own);
	totals = tess_shared_stats_totals_or(state->stats, own);
	if (totals[SORT_SORTED] == 0)
		return;
	ExplainPropertyText("Sort Method", totals[SORT_TOPN] > 0 ? "top-N in memory" :
						totals[SORT_EXTERNAL] > 0 ? "external merge" : "in memory", es);
	tess_explain_kb("Memory Usage", totals[SORT_MEMORY], es);
	if (totals[SORT_EXTERNAL] > 0)
		tess_explain_kb("Disk Usage", totals[SORT_DISK], es);
	if (totals[SORT_OVERRUN] > 0)
		tess_explain_kb("Overrun", totals[SORT_OVERRUN], es);
	/* How the node sorted: VERBOSE only. */
	if (!es->verbose)
		return;
	if (totals[SORT_EXTERNAL] > 0)
	{
		ExplainPropertyInteger("Runs", NULL, totals[SORT_RUNS], es);
		if (totals[SORT_PASSES] > 0)
			ExplainPropertyInteger("Merge Passes", NULL, totals[SORT_PASSES], es);
	}
	ExplainPropertyInteger("Input Batches", NULL, totals[SORT_BATCHES], es);
	ExplainPropertyInteger("Input Rows", NULL, totals[SORT_INPUT_ROWS], es);
	if (totals[SORT_REBUILT] > 0)
		ExplainPropertyInteger("Rows Rebuilt", NULL, totals[SORT_REBUILT], es);
	/*
	 * A key of another type: the order of its abbreviated keys, which the
	 * node knows by the core's comparator (abbrev_order_of), or none, and
	 * whether it gave them up; a core that renamed its comparators shows
	 * none here, a sort slower but right.
	 */
	if (state->generic >= 0)
		ExplainPropertyText("Abbreviated Keys",
							totals[SORT_ABBREV_GIVEN_UP] > 0 ? "given up" :
							state->abbrev_taken == SORT_ABBREV_UNSIGNED ? "unsigned" :
							state->abbrev_taken == SORT_ABBREV_SIGNED ? "signed" :
							state->abbrev_taken == SORT_ABBREV_REVERSED ? "reversed" :
							state->abbrev_taken == SORT_ABBREV_UINT32 ? "uint32" :
							state->abbrev_taken == SORT_ABBREV_INT32 ? "int32" : "none", es);
}

/*
 * Under a Gather Merge every participant sorts its share, and the node,
 * parallel-aware for this alone, shares only its counters, in the rows of
 * its chunk.
 */
TESS_NODE_STATS_CALLBACKS(sort, TessSortState, SORT_NCOUNTERS, sort_counters)

static const CustomExecMethods sort_exec_methods = {
	.CustomName = "TessSort",
	.BeginCustomScan = sort_begin,
	.ExecCustomScan = sort_exec,
	.EndCustomScan = sort_end,
	.ReScanCustomScan = sort_rescan,
	TESS_NODE_STATS_METHODS(sort),
	.ExplainCustomScan = sort_explain,
};

static Node *
sort_create_state(CustomScan *cscan)
{
	TessSortState *state = (TessSortState *)
		newNode(sizeof(TessSortState), T_CustomScanState);

	state->css.methods = &sort_exec_methods;
	return (Node *) state;
}

const CustomScanMethods tess_sort_scan_methods = {
	.CustomName = "TessSort",
	.CreateCustomScanState = sort_create_state,
};

const TessNode tess_sort_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_SORT_NODE_NAME,
	.set_tuple_bound = sort_set_tuple_bound,
};

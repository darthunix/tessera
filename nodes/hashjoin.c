#include "postgres.h"

#include "executor/executor.h"
#include "miscadmin.h"
#include "nodes/nodeFuncs.h"
#include "utils/datum.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"

#include "tessera/kernel_ops.h"
#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * TessHashJoin joins two batch children on one equality of integer keys.
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
	int			outer_key;
	int			inner_key;
	TessTableKeyKind outer_kind;
	TessTableKeyKind inner_kind;
	/* Every outer row matches at most one inner row: no second round. */
	bool		inner_unique;
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
	/* The payload of every row of an inner batch, one after another. */
	uint64	   *payload;

	/* Written by a kernel on failure only. */
	TessStatus	status;

	uint64		build_batches;
	uint64		build_rows;
	uint64		builds;
	uint64		grows;
	/* The most memory the table and the copies took, in bytes. */
	Size		peak_memory;
} TessHashJoinState;

static const CustomExecMethods join_exec_methods;

/* Raise the error a kernel stored, if the call failed. */
static inline void
check(TessHashJoinState *state, TessStatusCode code)
{
	if (code != TESS_OK)
		tess_status_report(&state->status);
}

/* The first key's hashes of the selected rows, under the join's NULL policy. */
static void
hash_keys(TessHashJoinState *state, TessTableKeyKind kind,
		  const TessDatumColumn *keys, const TessRowMask *rows,
		  TessRowMask *valid)
{
	TessStatusCode (*hash) (const TessDatumColumn *, const TessRowMask *,
							const TessRowMask *, TessNullKeys, uint32 *,
							TessRowMask *, TessStatus *);

	hash = kind == TESS_TABLE_KEY_INT8 ? state->kernels->int8_hash :
		state->kernels->int4_hash;
	/* An equality with NULL is never true: NULL keys leave the rows. */
	check(state, hash(keys, NULL, rows, TESS_NULL_KEYS_REJECT, state->hashes,
					  valid, &state->status));
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
		pfree(state->payload);
	}
	state->hashes = MemoryContextAllocZero(context, sizeof(uint32) * nrows);
	state->offsets = MemoryContextAllocZero(context, sizeof(uint32) * nrows);
	state->valid_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->pending_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->payload = MemoryContextAllocZero(context,
											mul_size(sizeof(uint64) * nrows,
													 1 + state->npayload));
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
	TessTableKeyKind kind = state->inner_kind;
	Size		payload_size = sizeof(uint64) * (1 + state->npayload);
	uint64		capacity = Max(state->inner_rows, JOIN_INITIAL_ROWS);
	Size		size;

	MemoryContextReset(state->table_context);
	MemoryContextReset(state->values_context);
	check(state, state->kernels->table_size(1, &kind, payload_size, capacity,
											&size, &state->status));
	state->region = MemoryContextAllocExtended(state->table_context, size,
											   MCXT_ALLOC_HUGE | MCXT_ALLOC_ZERO);
	state->region_len = size;
	check(state, state->kernels->table_create(state->region, size, 1, &kind,
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
	state->grows++;
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
	TessDatumColumn keys;
	TessTableKey key = {0};
	int			count;

	reserve_rows(state, nrows);
	valid = (TessRowMask) {nrows, state->valid_bits};
	pending = (TessRowMask) {nrows, state->pending_bits};
	child_column(batch, state->inner_key, &batch->rows, TESS_COLUMN_FOR_FILTER,
				 &keys);
	hash_keys(state, state->inner_kind, &keys, &batch->rows, &valid);
	count = tess_row_mask_count(&valid);
	if (count == 0)
		return;
	fill_payload(state, batch, &valid);
	memcpy(state->pending_bits, state->valid_bits, sizeof(uint64) * nwords);
	key.kind = state->inner_kind;
	key.column = &keys;
	for (;;)
	{
		check(state, state->kernels->table_insert(state->region,
												  state->region_len,
												  state->hashes, 1, &key,
												  (const uint8 *) state->payload,
												  &pending, state->offsets,
												  &state->status));
		if (tess_row_mask_count(&pending) == 0)
			break;
		/* The table is full: the rows left pending go in after growth. */
		grow_table(state);
	}
	state->build_rows += count;
	note_memory(state);
}

/* Read every batch of the inner child into a new table. */
static void
build_table(TessHashJoinState *state)
{
	create_table(state);
	state->build_rows = 0;
	for (;;)
	{
		TessBatch  *batch = tess_input_next(state->inner_input);

		if (batch == NULL)
			break;
		state->build_batches++;
		if (tess_row_mask_count(&batch->rows) > 0)
			insert_batch(state, batch);
		tess_input_finish(state->inner_input);
	}
	state->builds++;
	state->built = true;
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
	Bitmapset  *outer_key = bms_make_singleton(state->outer_key);
	Bitmapset  *inner_key = bms_make_singleton(state->inner_key);
	int			column = -1;

	if (request->output_mode == TESS_OUTPUT_ROWS)
	{
		int			natts = state->css.ss.ps.ps_ResultTupleSlot->tts_tupleDescriptor->natts;

		for (int attribute = 0; attribute < natts; attribute++)
			needed = bms_add_member(needed,
									tess_layout_column(&state->layout, attribute));
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
			continue;
		}
		if (state->npayload == JOIN_MAX_PAYLOAD)
			elog(ERROR, "TessHashJoin keeps at most %d inner columns",
				 JOIN_MAX_PAYLOAD);
		inner_columns = bms_add_member(inner_columns, state->child_columns[column]);
		state->payload_columns[state->npayload] = column;
		state->payload_words[column] = ++state->npayload;
	}
	/* The key before any other column, then the rows that survive it. */
	outer_request.filter_columns = outer_key;
	outer_request.projection_columns = outer_columns;
	outer_request.output_mode = TESS_OUTPUT_BATCH;
	outer_request.max_batch_rows = request->max_batch_rows;
	tess_input_set_request(state->outer_input, &outer_request);
	inner_request.filter_columns = inner_key;
	inner_request.projection_columns = inner_columns;
	inner_request.output_mode = TESS_OUTPUT_BATCH;
	tess_input_set_request(state->inner_input, &inner_request);
	state->request = request;
}

/* The plan's own data, written by the planner (join_planner.c). */
static void
read_node_data(TessHashJoinState *state, const List *data)
{
	TessPlanReader *reader = tess_plan_reader_create(data, TESS_HASH_JOIN_DATA,
													 TESS_HASH_JOIN_DATA_VERSION);
	List	   *sides = tess_plan_read_int_list(reader, "sides");
	List	   *columns = tess_plan_read_int_list(reader, "child_columns");
	ListCell   *side;
	ListCell   *column;
	int			index = 0;

	state->outer_key = tess_plan_read_int(reader, "outer_key");
	state->inner_key = tess_plan_read_int(reader, "inner_key");
	state->outer_kind = tess_plan_read_int(reader, "outer_kind");
	state->inner_kind = tess_plan_read_int(reader, "inner_kind");
	state->inner_unique = tess_plan_read_int(reader, "inner_unique") != 0;
	state->inner_rows = tess_plan_read_int(reader, "inner_rows");
	tess_plan_reader_finish(reader);
	if (list_length(sides) != state->ncolumns ||
		list_length(columns) != state->ncolumns ||
		(state->outer_kind != TESS_TABLE_KEY_INT4 &&
		 state->outer_kind != TESS_TABLE_KEY_INT8) ||
		(state->inner_kind != TESS_TABLE_KEY_INT4 &&
		 state->inner_kind != TESS_TABLE_KEY_INT8))
		elog(ERROR, "TessHashJoin received foreign plan data");
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
		info.computed != NIL || cscan->custom_scan_tlist == NIL)
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
	state->status = (TessStatus) TESS_STRUCT_INITIALIZER(TessStatus);
}

static TupleTableSlot *
join_exec(CustomScanState *css)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	if (state->request == NULL)
		send_requests(state);
	if (!state->built)
		build_table(state);
	elog(ERROR, "TessHashJoin cannot probe its table yet");
	return NULL;
}

static void
join_end(CustomScanState *css)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

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
};

const CustomScanMethods tess_hash_join_scan_methods = {
	.CustomName = "TessHashJoin",
	.CreateCustomScanState = join_create_state,
};

const TessNode tess_hash_join_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_HASH_JOIN_NODE_NAME,
};
